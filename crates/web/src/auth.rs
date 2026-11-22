//! Admin accounts, browser sessions and API tokens (PLAN.md 3.5a).
//!
//! Everything lives in one small SQLite file (`auth.sqlite`, kept apart from
//! the recordings index):
//!
//! - `admins`: argon2id password hashes. No default account: the first one
//!   is made with `scanner admin create`.
//! - `sessions`: browser logins. The cookie holds a random secret; the table
//!   holds only its SHA-256, so a leaked database can't be replayed. Each
//!   session has its own CSRF token. Sessions end after
//!   [`SESSION_IDLE_NS`] without use or [`SESSION_MAX_NS`] in total, at
//!   logout, at a password change, or when the account is deleted.
//! - `api_tokens`: named bearer tokens for scripts, stored hashed, revocable,
//!   with an optional expiry.
//! - `audit`: logins, failed logins, logouts and admin changes.
//!
//! [`LoginLimiter`] slows password guessing per client address and per
//! username.

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use base64::Engine;
use rusqlite::{Connection, OptionalExtension, params};
use scanner_store::migrate::{self, MigrateError, Migration, Schema, Up};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// A session unused for this long ends (24 h).
pub const SESSION_IDLE_NS: i64 = 24 * 3600 * 1_000_000_000;
/// No session lasts longer than this (7 days).
pub const SESSION_MAX_NS: i64 = 7 * 24 * 3600 * 1_000_000_000;
/// Passwords shorter than this are refused.
pub const MIN_PASSWORD_LEN: usize = 10;
/// Prefix that makes API tokens recognisable (e.g. to secret scanners).
pub const TOKEN_PREFIX: &str = "scn_";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Schema(#[from] MigrateError),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, AuthError>;

/// Nanoseconds since the Unix epoch.
pub fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// 32 random bytes, base64url without padding (43 characters).
fn random_secret() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("the OS random number generator failed");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Invalid(format!("hashing failed: {e}")))
}

fn verify_hash(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|h| {
            Argon2::default()
                .verify_password(password.as_bytes(), &h)
                .is_ok()
        })
        .unwrap_or(false)
}

/// A hash to verify against when the username doesn't exist, so a login for
/// an unknown name takes as long as a wrong password.
fn dummy_hash() -> &'static str {
    static H: OnceLock<String> = OnceLock::new();
    H.get_or_init(|| hash_password(&random_secret()).unwrap_or_default())
}

fn check_password_rules(password: &str) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(AuthError::Invalid(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    if password.len() > 1024 {
        return Err(AuthError::Invalid("password is too long".into()));
    }
    Ok(())
}

fn check_username(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(AuthError::Invalid(
            "username must be 1-64 letters, digits, '.', '_' or '-'".into(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Admin {
    pub id: i64,
    pub username: String,
    pub created_ns: i64,
}

/// A session just created: `secret` goes in the cookie and is not stored.
#[derive(Debug, Clone)]
pub struct NewSession {
    pub secret: String,
    pub csrf: String,
    pub expires_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub admin_id: i64,
    pub username: String,
    pub csrf: String,
    /// When it ends if it stays in use.
    pub expires_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Token {
    pub id: i64,
    pub name: String,
    pub created_ns: i64,
    pub expires_ns: Option<i64>,
    pub last_used_ns: Option<i64>,
    pub revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditEntry {
    pub ts_ns: i64,
    pub actor: String,
    pub action: String,
    pub detail: String,
    pub client: String,
}

pub struct AuthStore {
    conn: Mutex<Connection>,
}

/// The accounts database's migrations; see [`scanner_store::migrate`] for
/// the rules. Version 1 is the schema from before versioning, so it keeps
/// `IF NOT EXISTS`; later steps must not.
pub static SCHEMA: Schema = Schema {
    name: "accounts database",
    application_id: 0x5343_4E41, // "SCNA"
    migrations: &[Migration {
        description: "admins, sessions, api_tokens and audit tables",
        up: Up::Sql(V1),
    }],
};

const V1: &str = "
CREATE TABLE IF NOT EXISTS admins (
    id INTEGER PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    created_ns INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    id_hash TEXT PRIMARY KEY,
    admin_id INTEGER NOT NULL REFERENCES admins(id) ON DELETE CASCADE,
    csrf TEXT NOT NULL,
    created_ns INTEGER NOT NULL,
    last_seen_ns INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS api_tokens (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    created_ns INTEGER NOT NULL,
    expires_ns INTEGER,
    last_used_ns INTEGER,
    revoked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS audit (
    id INTEGER PRIMARY KEY,
    ts_ns INTEGER NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    detail TEXT NOT NULL,
    client TEXT NOT NULL
);
";

impl AuthStore {
    /// Open (creating if needed) the auth database. On Unix a new file is
    /// made readable by its owner only.
    pub fn open(path: &Path) -> Result<Self> {
        let new = !path.exists();
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)
                .map_err(|e| AuthError::Invalid(format!("creating {}: {e}", dir.display())))?;
        }
        let conn = Connection::open(path)?;
        #[cfg(unix)]
        if new {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        #[cfg(not(unix))]
        let _ = new;
        Self::init(conn)
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")?;
        migrate::migrate(&mut conn, &SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---- admins ----

    pub fn create_admin(&self, username: &str, password: &str, now: i64) -> Result<i64> {
        check_username(username)?;
        check_password_rules(password)?;
        let hash = hash_password(password)?;
        let db = self.db();
        let exists: bool = db
            .query_row(
                "SELECT 1 FROM admins WHERE username = ?1",
                [username],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if exists {
            return Err(AuthError::Invalid(format!(
                "admin {username:?} already exists"
            )));
        }
        db.execute(
            "INSERT INTO admins (username, password_hash, created_ns) VALUES (?1, ?2, ?3)",
            params![username, hash, now],
        )?;
        Ok(db.last_insert_rowid())
    }

    /// Set a new password and end every session of that admin.
    pub fn set_password(&self, username: &str, password: &str) -> Result<()> {
        check_password_rules(password)?;
        let hash = hash_password(password)?;
        let db = self.db();
        let n = db.execute(
            "UPDATE admins SET password_hash = ?2 WHERE username = ?1",
            params![username, hash],
        )?;
        if n == 0 {
            return Err(AuthError::Invalid(format!("no admin named {username:?}")));
        }
        db.execute(
            "DELETE FROM sessions WHERE admin_id = (SELECT id FROM admins WHERE username = ?1)",
            [username],
        )?;
        Ok(())
    }

    pub fn delete_admin(&self, username: &str) -> Result<()> {
        let n = self
            .db()
            .execute("DELETE FROM admins WHERE username = ?1", [username])?;
        if n == 0 {
            return Err(AuthError::Invalid(format!("no admin named {username:?}")));
        }
        Ok(())
    }

    pub fn admins(&self) -> Result<Vec<Admin>> {
        let db = self.db();
        let mut st = db.prepare("SELECT id, username, created_ns FROM admins ORDER BY id")?;
        let rows = st.query_map([], |r| {
            Ok(Admin {
                id: r.get(0)?,
                username: r.get(1)?,
                created_ns: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn has_admins(&self) -> Result<bool> {
        Ok(self
            .db()
            .query_row("SELECT 1 FROM admins LIMIT 1", [], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// The admin's id if the password is right. Takes about as long for an
    /// unknown username as for a wrong password. CPU-heavy (argon2id): call
    /// from a blocking thread.
    pub fn verify_password(&self, username: &str, password: &str) -> Result<Option<i64>> {
        let found: Option<(i64, String)> = self
            .db()
            .query_row(
                "SELECT id, password_hash FROM admins WHERE username = ?1",
                [username],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match found {
            Some((id, hash)) => verify_hash(password, &hash).then_some(id),
            None => {
                verify_hash(password, dummy_hash());
                None
            }
        })
    }

    // ---- sessions ----

    pub fn create_session(&self, admin_id: i64, now: i64) -> Result<NewSession> {
        let secret = random_secret();
        let csrf = random_secret();
        self.db().execute(
            "INSERT INTO sessions (id_hash, admin_id, csrf, created_ns, last_seen_ns)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![sha256_hex(&secret), admin_id, csrf, now],
        )?;
        Ok(NewSession {
            secret,
            csrf,
            expires_ns: now + SESSION_IDLE_NS.min(SESSION_MAX_NS),
        })
    }

    /// Look up a session by its cookie secret, ending it if it has expired
    /// and marking it used otherwise.
    pub fn session(&self, secret: &str, now: i64) -> Result<Option<Session>> {
        let h = sha256_hex(secret);
        let db = self.db();
        let row: Option<(i64, String, String, i64, i64)> = db
            .query_row(
                "SELECT s.admin_id, a.username, s.csrf, s.created_ns, s.last_seen_ns
                 FROM sessions s JOIN admins a ON a.id = s.admin_id WHERE s.id_hash = ?1",
                [&h],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((admin_id, username, csrf, created, seen)) = row else {
            return Ok(None);
        };
        if now - seen > SESSION_IDLE_NS || now - created > SESSION_MAX_NS {
            db.execute("DELETE FROM sessions WHERE id_hash = ?1", [&h])?;
            return Ok(None);
        }
        // Writing on every request is wasteful; a minute's precision is plenty.
        let seen = if now - seen > 60_000_000_000 {
            db.execute(
                "UPDATE sessions SET last_seen_ns = ?2 WHERE id_hash = ?1",
                params![h, now],
            )?;
            now
        } else {
            seen
        };
        Ok(Some(Session {
            admin_id,
            username,
            csrf,
            expires_ns: (seen + SESSION_IDLE_NS).min(created + SESSION_MAX_NS),
        }))
    }

    pub fn delete_session(&self, secret: &str) -> Result<()> {
        self.db().execute(
            "DELETE FROM sessions WHERE id_hash = ?1",
            [sha256_hex(secret)],
        )?;
        Ok(())
    }

    /// End every browser session (all admins). Returns how many ended.
    pub fn delete_all_sessions(&self) -> Result<usize> {
        Ok(self.db().execute("DELETE FROM sessions", [])?)
    }

    // ---- API tokens ----

    /// Make a token. The secret is returned once and only its hash is kept.
    pub fn create_token(
        &self,
        name: &str,
        expires_ns: Option<i64>,
        now: i64,
    ) -> Result<(i64, String)> {
        let name = name.trim();
        if name.is_empty() || name.len() > 100 {
            return Err(AuthError::Invalid(
                "token name must be 1-100 characters".into(),
            ));
        }
        let secret = format!("{TOKEN_PREFIX}{}", random_secret());
        let db = self.db();
        db.execute(
            "INSERT INTO api_tokens (name, token_hash, created_ns, expires_ns) VALUES (?1, ?2, ?3, ?4)",
            params![name, sha256_hex(&secret), now, expires_ns],
        )?;
        Ok((db.last_insert_rowid(), secret))
    }

    /// The token for a bearer secret, if it is valid now.
    pub fn token(&self, secret: &str, now: i64) -> Result<Option<Token>> {
        if !secret.starts_with(TOKEN_PREFIX) {
            return Ok(None);
        }
        let h = sha256_hex(secret);
        let db = self.db();
        let t = db
            .query_row(
                "SELECT id, name, created_ns, expires_ns, last_used_ns, revoked
                 FROM api_tokens WHERE token_hash = ?1",
                [&h],
                token_row,
            )
            .optional()?;
        let Some(t) = t.filter(|t| !t.revoked && t.expires_ns.is_none_or(|e| now < e)) else {
            return Ok(None);
        };
        if t.last_used_ns.is_none_or(|u| now - u > 60_000_000_000) {
            db.execute(
                "UPDATE api_tokens SET last_used_ns = ?2 WHERE id = ?1",
                params![t.id, now],
            )?;
        }
        Ok(Some(t))
    }

    pub fn tokens(&self) -> Result<Vec<Token>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT id, name, created_ns, expires_ns, last_used_ns, revoked
             FROM api_tokens ORDER BY id",
        )?;
        let rows = st.query_map([], token_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Revoke a token; false if there is no such token.
    pub fn revoke_token(&self, id: i64) -> Result<bool> {
        Ok(self
            .db()
            .execute("UPDATE api_tokens SET revoked = 1 WHERE id = ?1", [id])?
            > 0)
    }

    // ---- audit ----

    pub fn audit(&self, now: i64, actor: &str, action: &str, detail: &str, client: &str) {
        // Auditing must never take a request down with it.
        let _ = self.db().execute(
            "INSERT INTO audit (ts_ns, actor, action, detail, client) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now, actor, action, detail, client],
        );
    }

    pub fn recent_audit(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT ts_ns, actor, action, detail, client FROM audit ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = st.query_map([limit as i64], |r| {
            Ok(AuditEntry {
                ts_ns: r.get(0)?,
                actor: r.get(1)?,
                action: r.get(2)?,
                detail: r.get(3)?,
                client: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

fn token_row(r: &rusqlite::Row) -> rusqlite::Result<Token> {
    Ok(Token {
        id: r.get(0)?,
        name: r.get(1)?,
        created_ns: r.get(2)?,
        expires_ns: r.get(3)?,
        last_used_ns: r.get(4)?,
        revoked: r.get::<_, i64>(5)? != 0,
    })
}

/// Slows password guessing. Each key (a client address, a username) gets
/// [`LoginLimiter::FREE_FAILURES`] failures per window; after that each
/// further failure doubles a lockout, up to [`LoginLimiter::MAX_LOCK_NS`].
#[derive(Debug, Default)]
pub struct LoginLimiter {
    entries: HashMap<String, Failures>,
}

#[derive(Debug, Clone, Copy)]
struct Failures {
    count: u32,
    first_ns: i64,
    locked_until_ns: i64,
}

impl LoginLimiter {
    pub const FREE_FAILURES: u32 = 5;
    pub const WINDOW_NS: i64 = 15 * 60 * 1_000_000_000;
    pub const MAX_LOCK_NS: i64 = 15 * 60 * 1_000_000_000;

    /// Seconds to wait if any key is locked out now.
    pub fn check(&self, keys: &[String], now: i64) -> Option<u64> {
        keys.iter()
            .filter_map(|k| self.entries.get(k))
            .map(|f| f.locked_until_ns - now)
            .filter(|&w| w > 0)
            .max()
            .map(|w| (w as u64).div_ceil(1_000_000_000))
    }

    pub fn failed(&mut self, keys: &[String], now: i64) {
        for k in keys {
            let f = self.entries.entry(k.clone()).or_insert(Failures {
                count: 0,
                first_ns: now,
                locked_until_ns: 0,
            });
            if now - f.first_ns > Self::WINDOW_NS && now >= f.locked_until_ns {
                *f = Failures {
                    count: 0,
                    first_ns: now,
                    locked_until_ns: 0,
                };
            }
            f.count += 1;
            if f.count > Self::FREE_FAILURES {
                let exp = (f.count - Self::FREE_FAILURES - 1).min(20);
                let lock = (1_000_000_000i64 << exp).min(Self::MAX_LOCK_NS);
                f.locked_until_ns = now + lock;
            }
        }
        // Keep memory bounded under a spray of addresses.
        if self.entries.len() > 10_000 {
            self.entries
                .retain(|_, f| now - f.first_ns <= Self::WINDOW_NS || now < f.locked_until_ns);
        }
    }

    pub fn succeeded(&mut self, keys: &[String]) {
        for k in keys {
            self.entries.remove(k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;

    #[test]
    fn admin_passwords_are_hashed_and_checked() {
        let a = AuthStore::in_memory().unwrap();
        assert!(!a.has_admins().unwrap());
        assert!(a.create_admin("masen", "short", 0).is_err());
        assert!(a.create_admin("bad name", "long enough pw", 0).is_err());
        let id = a.create_admin("masen", "correct horse battery", 0).unwrap();
        assert!(a.create_admin("masen", "another long one", 0).is_err());
        assert!(a.has_admins().unwrap());
        let stored: String = a
            .db()
            .query_row("SELECT password_hash FROM admins", [], |r| r.get(0))
            .unwrap();
        assert!(stored.starts_with("$argon2id$"), "{stored}");
        assert_eq!(
            a.verify_password("masen", "correct horse battery").unwrap(),
            Some(id)
        );
        assert_eq!(a.verify_password("masen", "wrong password!").unwrap(), None);
        assert_eq!(
            a.verify_password("nobody", "correct horse battery")
                .unwrap(),
            None
        );
    }

    #[test]
    fn sessions_expire_and_end_on_password_change() {
        let a = AuthStore::in_memory().unwrap();
        let id = a.create_admin("masen", "correct horse battery", 0).unwrap();
        let s = a.create_session(id, 0).unwrap();
        // Only the hash is stored.
        let stored: String = a
            .db()
            .query_row("SELECT id_hash FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_ne!(stored, s.secret);
        let got = a.session(&s.secret, 10 * S).unwrap().unwrap();
        assert_eq!((got.admin_id, got.username.as_str()), (id, "masen"));
        assert_eq!(got.csrf, s.csrf);
        assert!(a.session("not-a-session", 10 * S).unwrap().is_none());
        // Idle expiry.
        assert!(
            a.session(&s.secret, SESSION_IDLE_NS + 20 * S)
                .unwrap()
                .is_none()
        );
        // Kept alive by use, but not past the absolute limit.
        let s = a.create_session(id, 0).unwrap();
        let mut t = 0;
        while t < SESSION_MAX_NS - SESSION_IDLE_NS / 2 {
            t += SESSION_IDLE_NS / 2;
            assert!(a.session(&s.secret, t).unwrap().is_some(), "{t}");
        }
        assert!(a.session(&s.secret, SESSION_MAX_NS + S).unwrap().is_none());
        // Password change ends sessions.
        let s = a.create_session(id, 0).unwrap();
        a.set_password("masen", "a brand new password").unwrap();
        assert!(a.session(&s.secret, S).unwrap().is_none());
        // Deleting the admin ends sessions too.
        let s = a.create_session(id, 0).unwrap();
        a.delete_admin("masen").unwrap();
        assert!(a.session(&s.secret, S).unwrap().is_none());
    }

    #[test]
    fn tokens_are_hashed_revocable_and_expire() {
        let a = AuthStore::in_memory().unwrap();
        let (id, secret) = a.create_token("backup script", Some(100 * S), 0).unwrap();
        assert!(secret.starts_with(TOKEN_PREFIX));
        let stored: String = a
            .db()
            .query_row("SELECT token_hash FROM api_tokens", [], |r| r.get(0))
            .unwrap();
        assert!(!stored.contains(&secret[4..]));
        assert_eq!(a.token(&secret, S).unwrap().unwrap().id, id);
        assert!(a.token(&secret, 100 * S).unwrap().is_none(), "expired");
        assert!(a.token("scn_nope", S).unwrap().is_none());
        let (id2, s2) = a.create_token("forever", None, 0).unwrap();
        assert!(a.token(&s2, 1_000_000 * S).unwrap().is_some());
        assert!(a.revoke_token(id2).unwrap());
        assert!(a.token(&s2, S).unwrap().is_none());
        assert!(!a.revoke_token(999).unwrap());
        assert_eq!(a.tokens().unwrap().len(), 2);
    }

    #[test]
    fn limiter_backs_off_then_forgives() {
        let mut l = LoginLimiter::default();
        let keys = vec!["ip:1.2.3.4".to_string(), "user:masen".to_string()];
        for _ in 0..LoginLimiter::FREE_FAILURES {
            assert_eq!(l.check(&keys, 0), None);
            l.failed(&keys, 0);
        }
        assert_eq!(l.check(&keys, 0), None);
        l.failed(&keys, 0);
        assert_eq!(l.check(&keys, 0), Some(1));
        l.failed(&keys, 0);
        assert_eq!(l.check(&keys, 0), Some(2));
        for _ in 0..30 {
            l.failed(&keys, 0);
        }
        assert_eq!(l.check(&keys, 0), Some(900), "capped at 15 minutes");
        assert_eq!(l.check(&keys, LoginLimiter::MAX_LOCK_NS + 1), None);
        // Another address and user are unaffected.
        assert_eq!(l.check(&["ip:5.6.7.8".into(), "user:bob".into()], 0), None);
        l.succeeded(&keys);
        assert_eq!(l.check(&keys, 0), None);
    }

    /// The schema every migration adds up to. A change here should come
    /// with a new migration, never an edit to an old one.
    #[test]
    fn schema_snapshot() {
        let a = AuthStore::in_memory().unwrap();
        assert_eq!(
            migrate::schema_sql(&a.db()).unwrap(),
            include_str!("../schema/auth.sql")
        );
    }

    #[test]
    fn adopts_accounts_from_before_migrations_and_refuses_newer() {
        // What PR 5 created: the tables at user_version 0.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("auth.sqlite");
        let a = AuthStore::open(&p).unwrap();
        let id = a.create_admin("masen", "correct horse battery", 0).unwrap();
        a.db()
            .execute_batch("PRAGMA user_version = 0; PRAGMA application_id = 0;")
            .unwrap();
        drop(a);

        let a = AuthStore::open(&p).unwrap();
        assert_eq!(
            a.verify_password("masen", "correct horse battery").unwrap(),
            Some(id)
        );
        let v: i64 = a
            .db()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA.version());
        a.db()
            .pragma_update(None, "user_version", SCHEMA.version() + 1)
            .unwrap();
        drop(a);
        let err = AuthStore::open(&p).err().unwrap().to_string();
        assert!(err.contains("newer scanner"), "{err}");
    }
}
