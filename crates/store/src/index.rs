//! SQLite index of recorded transmissions (PLAN.md 3.5). One row per
//! transmission; the web API (Phase 3) queries it.

use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

pub use rusqlite::Error as IndexError;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS transmissions (
    id          INTEGER PRIMARY KEY,
    freq_hz     REAL    NOT NULL,
    channel     TEXT,
    origin      TEXT    NOT NULL,
    mode        TEXT    NOT NULL,
    start_ns    INTEGER NOT NULL,
    end_ns      INTEGER,
    audio_secs  REAL,
    signal_secs REAL,
    peak_snr_db REAL,
    mean_snr_db REAL,
    audio_rate  INTEGER NOT NULL,
    wav_path    TEXT    NOT NULL,
    wav_bytes   INTEGER,
    status      TEXT    NOT NULL DEFAULT 'recording'
);
CREATE INDEX IF NOT EXISTS transmissions_start ON transmissions(start_ns);
CREATE INDEX IF NOT EXISTS transmissions_freq ON transmissions(freq_hz, start_ns);
";

/// A finished or in-progress transmission row.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Row {
    pub id: i64,
    pub freq_hz: f64,
    pub channel: Option<String>,
    pub origin: String,
    pub mode: String,
    pub start_ns: i64,
    pub end_ns: Option<i64>,
    pub audio_secs: Option<f64>,
    pub signal_secs: Option<f64>,
    pub peak_snr_db: Option<f64>,
    pub mean_snr_db: Option<f64>,
    pub audio_rate: u32,
    pub wav_path: String,
    pub wav_bytes: Option<i64>,
    pub status: String,
}

/// Filters for [`Index::search`]. `None` means no constraint.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    pub freq_min: Option<f64>,
    pub freq_max: Option<f64>,
    pub since_ns: Option<i64>,
    pub until_ns: Option<i64>,
    pub min_secs: Option<f64>,
    /// Paging: only rows with a smaller id.
    pub before_id: Option<i64>,
    pub limit: usize,
}

pub struct NewRow<'a> {
    pub freq_hz: f64,
    pub channel: Option<&'a str>,
    pub origin: &'a str,
    pub mode: &'a str,
    pub start_ns: i64,
    pub audio_rate: u32,
    pub wav_path: &'a str,
}

pub struct Index {
    db: Connection,
}

impl Index {
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        Self::init(Connection::open(path)?)
    }

    pub fn in_memory() -> Result<Self, IndexError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(db: Connection) -> Result<Self, IndexError> {
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        db.execute_batch(SCHEMA)?;
        let v: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if v == 0 {
            db.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        // Rows left 'recording' by a crash have no finished file.
        db.execute(
            "UPDATE transmissions SET status = 'interrupted' WHERE status = 'recording'",
            [],
        )?;
        Ok(Index { db })
    }

    pub fn insert(&self, r: &NewRow) -> Result<i64, IndexError> {
        self.db.execute(
            "INSERT INTO transmissions (freq_hz, channel, origin, mode, start_ns, audio_rate, wav_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![r.freq_hz, r.channel, r.origin, r.mode, r.start_ns, r.audio_rate, r.wav_path],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn complete(
        &self,
        id: i64,
        end_ns: i64,
        audio_secs: f64,
        signal_secs: f64,
        peak_snr_db: f32,
        mean_snr_db: f32,
        wav_path: &str,
        wav_bytes: u64,
    ) -> Result<(), IndexError> {
        self.db.execute(
            "UPDATE transmissions SET end_ns = ?2, audio_secs = ?3, signal_secs = ?4,
                 peak_snr_db = ?5, mean_snr_db = ?6, wav_path = ?7, wav_bytes = ?8,
                 status = 'complete'
             WHERE id = ?1",
            params![
                id,
                end_ns,
                audio_secs,
                signal_secs,
                peak_snr_db,
                mean_snr_db,
                wav_path,
                wav_bytes as i64
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, id: i64) -> Result<Option<Row>, IndexError> {
        self.db
            .query_row("SELECT * FROM transmissions WHERE id = ?1", [id], row)
            .optional()
    }

    /// Most recent first.
    pub fn recent(&self, limit: usize) -> Result<Vec<Row>, IndexError> {
        let mut st = self
            .db
            .prepare("SELECT * FROM transmissions ORDER BY start_ns DESC, id DESC LIMIT ?1")?;
        st.query_map([limit as i64], row)?.collect()
    }

    /// Completed transmissions matching `q`, newest first.
    pub fn search(&self, q: &Query) -> Result<Vec<Row>, IndexError> {
        let mut st = self.db.prepare(
            "SELECT * FROM transmissions
             WHERE status = 'complete'
               AND (?1 IS NULL OR freq_hz >= ?1) AND (?2 IS NULL OR freq_hz <= ?2)
               AND (?3 IS NULL OR start_ns >= ?3) AND (?4 IS NULL OR start_ns < ?4)
               AND (?5 IS NULL OR audio_secs >= ?5)
               AND (?6 IS NULL OR id < ?6)
             ORDER BY start_ns DESC, id DESC LIMIT ?7",
        )?;
        st.query_map(
            params![
                q.freq_min,
                q.freq_max,
                q.since_ns,
                q.until_ns,
                q.min_secs,
                q.before_id,
                q.limit.clamp(1, 1000) as i64
            ],
            row,
        )?
        .collect()
    }

    pub fn count(&self) -> Result<i64, IndexError> {
        self.db
            .query_row("SELECT COUNT(*) FROM transmissions", [], |r| r.get(0))
    }
}

fn row(r: &rusqlite::Row) -> Result<Row, IndexError> {
    Ok(Row {
        id: r.get("id")?,
        freq_hz: r.get("freq_hz")?,
        channel: r.get("channel")?,
        origin: r.get("origin")?,
        mode: r.get("mode")?,
        start_ns: r.get("start_ns")?,
        end_ns: r.get("end_ns")?,
        audio_secs: r.get("audio_secs")?,
        signal_secs: r.get("signal_secs")?,
        peak_snr_db: r.get("peak_snr_db")?,
        mean_snr_db: r.get("mean_snr_db")?,
        audio_rate: r.get("audio_rate")?,
        wav_path: r.get("wav_path")?,
        wav_bytes: r.get("wav_bytes")?,
        status: r.get("status")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_complete_and_query() {
        let ix = Index::in_memory().unwrap();
        let id = ix
            .insert(&NewRow {
                freq_hz: 146.52e6,
                channel: Some("National simplex calling"),
                origin: "listed",
                mode: "nbfm",
                start_ns: 1_795_330_443_123_000_000,
                audio_rate: 8000,
                wav_path: "2026/11/22/x.wav",
            })
            .unwrap();
        assert_eq!(ix.get(id).unwrap().unwrap().status, "recording");
        ix.complete(
            id,
            1_795_330_450_000_000_000,
            7.1,
            6.5,
            31.0,
            25.0,
            "2026/11/22/x.wav",
            113_644,
        )
        .unwrap();
        let r = ix.get(id).unwrap().unwrap();
        assert_eq!(r.status, "complete");
        assert_eq!(r.wav_bytes, Some(113_644));
        assert_eq!(ix.recent(10).unwrap().len(), 1);
    }

    #[test]
    fn search_filters_and_pages() {
        let ix = Index::in_memory().unwrap();
        for (k, f) in [146.52e6, 146.84e6, 147.06e6, 146.52e6].iter().enumerate() {
            let id = ix
                .insert(&NewRow {
                    freq_hz: *f,
                    channel: None,
                    origin: "grid",
                    mode: "nbfm",
                    start_ns: k as i64 * 1_000_000_000,
                    audio_rate: 8000,
                    wav_path: "x",
                })
                .unwrap();
            ix.complete(id, 0, 1.0 + k as f64, 1.0, 20.0, 15.0, "x", 1)
                .unwrap();
        }
        let all = ix
            .search(&Query {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            all.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![4, 3, 2, 1]
        );
        let q = Query {
            freq_min: Some(146.5e6),
            freq_max: Some(146.6e6),
            limit: 10,
            ..Default::default()
        };
        assert_eq!(ix.search(&q).unwrap().len(), 2);
        let page = ix
            .search(&Query {
                before_id: Some(3),
                min_secs: Some(2.0),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.iter().map(|r| r.id).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn reopening_marks_unfinished_rows_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("index.sqlite");
        let ix = Index::open(&p).unwrap();
        let id = ix
            .insert(&NewRow {
                freq_hz: 1.0,
                channel: None,
                origin: "grid",
                mode: "nbfm",
                start_ns: 0,
                audio_rate: 8000,
                wav_path: "x",
            })
            .unwrap();
        drop(ix);
        let ix = Index::open(&p).unwrap();
        assert_eq!(ix.get(id).unwrap().unwrap().status, "interrupted");
    }
}
