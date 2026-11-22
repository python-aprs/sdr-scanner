//! Settings changed while the scanner runs (from the web UI or API), kept in
//! `settings.sqlite` next to the recordings so they survive a restart. For
//! now: per-frequency lockout and priority overrides on top of the config
//! file.

use crate::migrate::{self, MigrateError, Migration, Schema, Up};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("settings database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Schema(#[from] MigrateError),
}

/// The settings database's migrations; see [`crate::migrate`] for the rules.
pub static SCHEMA: Schema = Schema {
    name: "settings database",
    application_id: 0x5343_4E53, // "SCNS"
    migrations: &[Migration {
        description: "channel_overrides table",
        up: Up::Sql(
            "CREATE TABLE channel_overrides (
                freq_hz    INTEGER PRIMARY KEY,
                locked_out INTEGER,
                priority   INTEGER,
                updated_ns INTEGER NOT NULL,
                updated_by TEXT    NOT NULL
            );",
        ),
    }],
};

/// A lockout or priority setting for one frequency. `None` leaves the config
/// file's value in force.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Override {
    pub freq_hz: i64,
    pub locked_out: Option<bool>,
    pub priority: Option<bool>,
    pub updated_ns: i64,
    pub updated_by: String,
}

pub struct Settings {
    db: Mutex<Connection>,
}

impl Settings {
    pub fn open(path: &Path) -> Result<Self, SettingsError> {
        Self::init(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self, SettingsError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut db: Connection) -> Result<Self, SettingsError> {
        db.pragma_update(None, "journal_mode", "WAL")?;
        migrate::migrate(&mut db, &SCHEMA)?;
        Ok(Settings { db: Mutex::new(db) })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn overrides(&self) -> Result<Vec<Override>, SettingsError> {
        let db = self.db();
        let mut st = db.prepare(
            "SELECT freq_hz, locked_out, priority, updated_ns, updated_by
             FROM channel_overrides ORDER BY freq_hz",
        )?;
        let rows = st
            .query_map([], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get(&self, freq_hz: i64) -> Result<Option<Override>, SettingsError> {
        Ok(self
            .db()
            .query_row(
                "SELECT freq_hz, locked_out, priority, updated_ns, updated_by
                 FROM channel_overrides WHERE freq_hz = ?1",
                [freq_hz],
                row,
            )
            .optional()?)
    }

    /// Set the given fields (others keep their override, if any).
    pub fn set(
        &self,
        freq_hz: i64,
        locked_out: Option<bool>,
        priority: Option<bool>,
        now_ns: i64,
        by: &str,
    ) -> Result<Override, SettingsError> {
        self.db().execute(
            "INSERT INTO channel_overrides (freq_hz, locked_out, priority, updated_ns, updated_by)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (freq_hz) DO UPDATE SET
                locked_out = COALESCE(?2, locked_out),
                priority   = COALESCE(?3, priority),
                updated_ns = ?4,
                updated_by = ?5",
            params![freq_hz, locked_out, priority, now_ns, by],
        )?;
        Ok(self.get(freq_hz)?.expect("just written"))
    }

    /// Back to the config file for this frequency. False if there was no
    /// override.
    pub fn clear(&self, freq_hz: i64) -> Result<bool, SettingsError> {
        Ok(self.db().execute(
            "DELETE FROM channel_overrides WHERE freq_hz = ?1",
            [freq_hz],
        )? > 0)
    }
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Override> {
    Ok(Override {
        freq_hz: r.get(0)?,
        locked_out: r.get(1)?,
        priority: r.get(2)?,
        updated_ns: r.get(3)?,
        updated_by: r.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_merge_and_clear() {
        let s = Settings::in_memory().unwrap();
        assert!(s.overrides().unwrap().is_empty());
        let o = s.set(146_520_000, Some(true), None, 1, "masen").unwrap();
        assert_eq!((o.locked_out, o.priority), (Some(true), None));
        // Setting priority keeps the lockout.
        let o = s.set(146_520_000, None, Some(true), 2, "token:ci").unwrap();
        assert_eq!((o.locked_out, o.priority), (Some(true), Some(true)));
        assert_eq!(o.updated_by, "token:ci");
        s.set(145_212_500, Some(true), None, 3, "masen").unwrap();
        let all = s.overrides().unwrap();
        assert_eq!(
            all.iter().map(|o| o.freq_hz).collect::<Vec<_>>(),
            vec![145_212_500, 146_520_000]
        );
        assert!(s.clear(146_520_000).unwrap());
        assert!(!s.clear(146_520_000).unwrap());
        assert_eq!(s.overrides().unwrap().len(), 1);
    }

    #[test]
    fn schema_snapshot() {
        let s = Settings::in_memory().unwrap();
        assert_eq!(
            migrate::schema_sql(&s.db()).unwrap(),
            include_str!("../schema/settings.sql")
        );
    }
}
