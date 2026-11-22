//! Versioned schema migrations for the scanner's SQLite databases.
//!
//! Each database has a [`Schema`]: an ordered list of migrations. The
//! database's `PRAGMA user_version` records how many have run, and its
//! `PRAGMA application_id` says which kind of database it is, so pointing
//! one kind's path at another's file fails instead of corrupting it.
//!
//! Rules for adding a migration:
//!
//! - Append it to the end of the list. Never edit, reorder or remove one
//!   that has shipped: databases in the field have already run it.
//! - Write it for the database as the previous migration left it. Only the
//!   first (baseline) migration may use `IF NOT EXISTS`, because it also
//!   adopts databases made before versioning existed.
//! - Update the schema snapshot test next to the schema, so the change shows
//!   up in review as a diff of the resulting tables.
//!
//! [`migrate`] runs every pending migration in one `BEGIN IMMEDIATE`
//! transaction: a failure leaves the database exactly as it was, and two
//! processes opening the same file (for example `scanner serve` and
//! `scanner admin`) can't both migrate it. Before changing a database that
//! already holds data it writes a copy next to it
//! (`<file>.v<old version>-<unix seconds>.bak`).

use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long to wait for another process's lock before giving up.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// One schema step.
pub enum Up {
    /// SQL statements, run with `execute_batch`.
    Sql(&'static str),
    /// A data migration that needs code. It runs inside the migration
    /// transaction.
    Code(fn(&Connection) -> rusqlite::Result<()>),
}

pub struct Migration {
    /// What the step does, for the log line (e.g. "add transmissions.talkgroup").
    pub description: &'static str,
    pub up: Up,
}

/// The migrations of one kind of database.
pub struct Schema {
    /// Human name for messages ("recordings index").
    pub name: &'static str,
    /// Written to `PRAGMA application_id`.
    pub application_id: i32,
    /// Migration `n` (counting from 1) takes the database to version `n`.
    pub migrations: &'static [Migration],
}

impl Schema {
    /// The version this build creates and expects.
    pub fn version(&self) -> i64 {
        self.migrations.len() as i64
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error(
        "{path} has {name} schema version {found}, newer than the version {known} this scanner \
         knows. Run a newer scanner, or restore the .bak copy saved before it was upgraded."
    )]
    TooNew {
        path: String,
        name: &'static str,
        found: i64,
        known: i64,
    },
    #[error(
        "{path} has {name} schema version {found}, older than this scanner's {known}; \
         `scanner serve` or `scanner admin` upgrades it when it opens it"
    )]
    Outdated {
        path: String,
        name: &'static str,
        found: i64,
        known: i64,
    },
    #[error("{path} is not the scanner's {name} (application_id {found:#010x})")]
    WrongFile {
        path: String,
        name: &'static str,
        found: i32,
    },
    #[error("backing up {path} before upgrading it: {source}")]
    Backup {
        path: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("migration {version} ({description}) left broken foreign keys in {path}")]
    ForeignKeys {
        path: String,
        version: i64,
        description: &'static str,
    },
}

/// What [`migrate`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub from: i64,
    pub to: i64,
    pub backup: Option<PathBuf>,
}

fn path_of(conn: &Connection) -> Option<&str> {
    conn.path().filter(|p| !p.is_empty())
}

fn display(conn: &Connection) -> String {
    path_of(conn).unwrap_or(":memory:").to_string()
}

fn state(conn: &Connection) -> rusqlite::Result<(i32, i64)> {
    let app: i32 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
    let ver: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    Ok((app, ver))
}

/// Refuses files of another kind and versions newer than this build knows.
fn check_kind(conn: &Connection, s: &Schema, app: i32, ver: i64) -> Result<(), MigrateError> {
    // 0 is a database made before application ids were set.
    if app != 0 && app != s.application_id {
        return Err(MigrateError::WrongFile {
            path: display(conn),
            name: s.name,
            found: app,
        });
    }
    if ver > s.version() {
        return Err(MigrateError::TooNew {
            path: display(conn),
            name: s.name,
            found: ver,
            known: s.version(),
        });
    }
    Ok(())
}

/// Bring the database up to `s.version()`. Call it after setting
/// connection pragmas such as `journal_mode`, which can't change inside a
/// transaction.
pub fn migrate(conn: &mut Connection, s: &Schema) -> Result<Outcome, MigrateError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let (app, ver) = state(conn)?;
    check_kind(conn, s, app, ver)?;
    let mut out = Outcome {
        from: ver,
        to: ver,
        backup: None,
    };
    if ver == s.version() && app == s.application_id {
        return Ok(out);
    }

    // Back up a database that holds anything before changing its tables.
    // VACUUM INTO can't run inside a transaction, so this comes first; if
    // another process migrates in between, the copy is merely older.
    let has_tables: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type = 'table')",
        [],
        |r| r.get(0),
    )?;
    if ver < s.version()
        && has_tables
        && let Some(p) = path_of(conn)
    {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let failed = |source: Box<dyn std::error::Error + Send + Sync>| MigrateError::Backup {
            path: p.to_string(),
            source,
        };
        let bak = backup_file(Path::new(p), ver, secs).map_err(|e| failed(e.into()))?;
        if let Err(e) = conn.execute("VACUUM INTO ?1", [bak.to_string_lossy()]) {
            let _ = std::fs::remove_file(&bak);
            return Err(failed(e.into()));
        }
        out.backup = Some(bak);
    }

    let path = display(conn);
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Someone else may have migrated while we waited for the lock.
    let (app, ver) = state(&tx)?;
    check_kind(&tx, s, app, ver)?;
    out.from = ver;
    // Lets a step rebuild a table that others reference; checked below.
    tx.pragma_update(None, "defer_foreign_keys", true)?;
    for (i, m) in s.migrations.iter().enumerate().skip(ver as usize) {
        let version = i as i64 + 1;
        match m.up {
            Up::Sql(sql) => tx.execute_batch(sql)?,
            Up::Code(f) => f(&tx)?,
        }
        let broken: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_foreign_key_check)",
            [],
            |r| r.get(0),
        )?;
        if broken {
            return Err(MigrateError::ForeignKeys {
                path,
                version,
                description: m.description,
            });
        }
        tx.pragma_update(None, "user_version", version)?;
    }
    tx.pragma_update(None, "application_id", s.application_id)?;
    tx.commit()?;
    out.to = s.version();
    if has_tables && out.from < out.to {
        eprintln!(
            "upgraded {} {path} from schema version {} to {}{}",
            s.name,
            out.from,
            out.to,
            out.backup
                .as_ref()
                .map(|b| format!(" (backup: {})", b.display()))
                .unwrap_or_default()
        );
    }
    Ok(out)
}

/// Create the empty backup file `<db>.v<ver>-<secs>[-n].bak`, with the
/// database's own permissions (the accounts database is owner-only), for
/// VACUUM INTO to fill.
fn backup_file(db: &Path, ver: i64, secs: u64) -> std::io::Result<PathBuf> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        opts.mode(std::fs::metadata(db)?.permissions().mode() & 0o777);
    }
    for n in 0..100 {
        let suffix = if n == 0 {
            String::new()
        } else {
            format!("-{n}")
        };
        let bak = PathBuf::from(format!("{}.v{ver}-{secs}{suffix}.bak", db.display()));
        match opts.open(&bak) {
            Ok(_) => return Ok(bak),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("too many backups with the same name"))
}

/// Open an existing database for a reader that must not migrate it (for
/// example the web API next to a running recorder): it has to be at exactly
/// this build's version.
pub fn open_current(path: &Path, s: &Schema) -> Result<Connection, MigrateError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let (app, ver) = state(&conn)?;
    check_kind(&conn, s, app, ver)?;
    if ver < s.version() {
        return Err(MigrateError::Outdated {
            path: display(&conn),
            name: s.name,
            found: ver,
            known: s.version(),
        });
    }
    Ok(conn)
}

/// The tables, indexes and columns a database has, one SQL statement per
/// line in a stable order, for schema snapshot tests.
pub fn schema_sql(conn: &Connection) -> rusqlite::Result<String> {
    let mut st = conn.prepare(
        "SELECT sql FROM sqlite_schema
         WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'
         ORDER BY type DESC, name",
    )?;
    let rows: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    // Normalise indentation so the snapshot doesn't depend on how the
    // migration's SQL was laid out.
    let mut out = String::new();
    for sql in rows {
        let lines: Vec<&str> = sql
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        for (i, l) in lines.iter().enumerate() {
            let inner = i > 0 && i + 1 < lines.len();
            out += if inner { "    " } else { "" };
            out += l;
            out += "\n";
        }
        out.pop();
        out += ";\n";
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &str = "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, a TEXT);";

    static ONE: Schema = Schema {
        name: "test database",
        application_id: 0x7465_7374,
        migrations: &[Migration {
            description: "baseline",
            up: Up::Sql(V1),
        }],
    };

    fn upper(c: &Connection) -> rusqlite::Result<()> {
        c.execute("UPDATE t SET a = upper(a)", []).map(|_| ())
    }

    static THREE: Schema = Schema {
        name: "test database",
        application_id: 0x7465_7374,
        migrations: &[
            Migration {
                description: "baseline",
                up: Up::Sql(V1),
            },
            Migration {
                description: "add t.b",
                up: Up::Sql("ALTER TABLE t ADD COLUMN b INTEGER NOT NULL DEFAULT 7;"),
            },
            Migration {
                description: "uppercase a",
                up: Up::Code(upper),
            },
        ],
    };

    static BROKEN: Schema = Schema {
        name: "test database",
        application_id: 0x7465_7374,
        migrations: &[
            Migration {
                description: "baseline",
                up: Up::Sql(V1),
            },
            Migration {
                description: "add t.b",
                up: Up::Sql("ALTER TABLE t ADD COLUMN b INTEGER NOT NULL DEFAULT 7;"),
            },
            Migration {
                description: "fails halfway",
                up: Up::Sql("CREATE TABLE u (x); INSERT INTO nowhere VALUES (1);"),
            },
        ],
    };

    fn version(c: &Connection) -> i64 {
        state(c).unwrap().1
    }

    #[test]
    fn new_database_gets_every_migration() {
        let mut c = Connection::open_in_memory().unwrap();
        let o = migrate(&mut c, &THREE).unwrap();
        assert_eq!((o.from, o.to, o.backup), (0, 3, None));
        assert_eq!(state(&c).unwrap(), (THREE.application_id, 3));
        // Running again does nothing.
        assert_eq!(migrate(&mut c, &THREE).unwrap().from, 3);
    }

    #[test]
    fn upgrade_keeps_data_runs_code_steps_and_backs_up() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.sqlite");
        let mut c = Connection::open(&p).unwrap();
        migrate(&mut c, &ONE).unwrap();
        c.execute("INSERT INTO t (a) VALUES ('kf7hvm')", [])
            .unwrap();
        drop(c);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let mut c = Connection::open(&p).unwrap();
        let o = migrate(&mut c, &THREE).unwrap();
        assert_eq!((o.from, o.to), (1, 3));
        let row: (String, i64) = c
            .query_row("SELECT a, b FROM t", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(row, ("KF7HVM".into(), 7));

        // The backup is the version-1 database, data included, and no more
        // readable than the database itself.
        let bak_path = o.backup.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&bak_path), mode(&p));
        }
        let bak = Connection::open(&bak_path).unwrap();
        assert_eq!(version(&bak), 1);
        let a: String = bak.query_row("SELECT a FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(a, "kf7hvm");
    }

    #[test]
    fn adopts_a_database_made_before_versioning() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.sqlite");
        let c = Connection::open(&p).unwrap();
        c.execute_batch(V1).unwrap();
        c.execute("INSERT INTO t (a) VALUES ('x')", []).unwrap();
        drop(c);
        let mut c = Connection::open(&p).unwrap();
        let o = migrate(&mut c, &THREE).unwrap();
        assert_eq!((o.from, o.to), (0, 3));
        assert!(o.backup.is_some());
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn failed_migration_changes_nothing() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c, &ONE).unwrap();
        assert!(migrate(&mut c, &BROKEN).is_err());
        assert_eq!(version(&c), 1);
        let cols: i64 = c
            .query_row("SELECT COUNT(*) FROM pragma_table_info('t')", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cols, 2, "column b from step 2 was rolled back");
        let u: bool = c
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE name = 'u')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!u);
    }

    #[test]
    fn refuses_newer_and_foreign_databases() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c, &THREE).unwrap();
        assert!(matches!(
            migrate(&mut c, &ONE),
            Err(MigrateError::TooNew {
                found: 3,
                known: 1,
                ..
            })
        ));
        let other = Schema {
            name: "other database",
            application_id: 1,
            migrations: ONE.migrations,
        };
        assert!(matches!(
            migrate(&mut c, &other),
            Err(MigrateError::WrongFile { .. })
        ));
    }

    #[test]
    fn readers_need_the_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.sqlite");
        assert!(
            open_current(&p, &ONE).is_err(),
            "missing file isn't created"
        );
        let mut c = Connection::open(&p).unwrap();
        migrate(&mut c, &ONE).unwrap();
        assert!(open_current(&p, &ONE).is_ok());
        assert!(matches!(
            open_current(&p, &THREE),
            Err(MigrateError::Outdated { found: 1, .. })
        ));
        migrate(&mut c, &THREE).unwrap();
        assert!(matches!(
            open_current(&p, &ONE),
            Err(MigrateError::TooNew { .. })
        ));
    }
}
