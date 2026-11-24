//! SQLite index of recorded transmissions (PLAN.md 3.5). One row per
//! transmission; the web API (Phase 3) queries it.

use crate::migrate::{self, MigrateError, Migration, Schema, Up};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("recordings index: {0}")]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Schema(#[from] MigrateError),
}

/// The recordings index's migrations; see [`crate::migrate`] for the rules.
/// Version 1 is the schema from before versioning, so it keeps
/// `IF NOT EXISTS`; later steps must not.
pub static SCHEMA: Schema = Schema {
    name: "recordings index",
    application_id: 0x5343_4E49, // "SCNI"
    migrations: &[
        Migration {
            description: "transmissions table",
            up: Up::Sql(V1),
        },
        Migration {
            description: "Opus copies",
            up: Up::Sql(
                "ALTER TABLE transmissions ADD COLUMN opus_path TEXT;
                 ALTER TABLE transmissions ADD COLUMN opus_bytes INTEGER;",
            ),
        },
        Migration {
            description: "CTCSS/DCS tone tags",
            up: Up::Sql("ALTER TABLE transmissions ADD COLUMN tone TEXT;"),
        },
        Migration {
            description: "AX.25 packets",
            up: Up::Sql(
                "CREATE TABLE packets (
                    id      INTEGER PRIMARY KEY,
                    time_ns INTEGER NOT NULL,
                    freq_hz REAL    NOT NULL,
                    channel TEXT,
                    source  TEXT    NOT NULL,
                    dest    TEXT    NOT NULL,
                    path    TEXT    NOT NULL,
                    text    TEXT    NOT NULL,
                    raw     BLOB    NOT NULL
                 );
                 CREATE INDEX packets_time ON packets(time_ns);",
            ),
        },
        Migration {
            description: "DTMF digits",
            up: Up::Sql("ALTER TABLE transmissions ADD COLUMN dtmf TEXT;"),
        },
    ],
};

const V1: &str = "
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
    /// The compressed copy the web UI plays, when there is one.
    pub opus_path: Option<String>,
    pub opus_bytes: Option<i64>,
    /// CTCSS tone (`100.0`) or DCS code (`D023N`) heard, if any.
    pub tone: Option<String>,
    /// DTMF digits heard, if any, groups split by a space (`*73 1234#`).
    pub dtmf: Option<String>,
}

/// Filters for [`Index::search`]. `None` means no constraint.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    pub freq_min: Option<f64>,
    pub freq_max: Option<f64>,
    pub since_ns: Option<i64>,
    pub until_ns: Option<i64>,
    pub min_secs: Option<f64>,
    /// Exact tone label.
    pub tone: Option<String>,
    /// DTMF digits that must appear in the recording's digits; an empty
    /// string matches any recording with digits.
    pub dtmf: Option<String>,
    /// Paging: only rows with a smaller id.
    pub before_id: Option<i64>,
    pub limit: usize,
}

/// The filters [`Index::search`] and [`Index::timeline`] share; parameters
/// as [`search_params`] binds them (`?7` is the limit).
const SEARCH_WHERE: &str = "status = 'complete'
    AND (?1 IS NULL OR freq_hz >= ?1) AND (?2 IS NULL OR freq_hz <= ?2)
    AND (?3 IS NULL OR start_ns >= ?3) AND (?4 IS NULL OR start_ns < ?4)
    AND (?5 IS NULL OR audio_secs >= ?5)
    AND (?6 IS NULL OR id < ?6)
    AND (?8 IS NULL OR tone = ?8)
    AND (?9 IS NULL OR instr(dtmf, ?9) > 0)";

fn search_params<'a>(q: &'a Query, limit: &'a i64) -> [&'a dyn rusqlite::ToSql; 9] {
    [
        &q.freq_min,
        &q.freq_max,
        &q.since_ns,
        &q.until_ns,
        &q.min_secs,
        &q.before_id,
        limit,
        &q.tone,
        &q.dtmf,
    ]
}

/// One transmission on the timeline: where, when and for how long.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub id: i64,
    pub freq_hz: f64,
    pub start_ns: i64,
    /// Time on air.
    pub secs: f64,
}

/// A decoded AX.25 frame.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PacketRow {
    pub id: i64,
    pub time_ns: i64,
    pub freq_hz: f64,
    pub channel: Option<String>,
    pub source: String,
    pub dest: String,
    /// Digipeaters, comma-separated, `*` on those that repeated it.
    pub path: String,
    /// `SRC>DEST,PATH:info` (non-printable bytes as `<0xNN>`).
    pub text: String,
    /// The frame without its FCS, as KISS carries it.
    #[serde(skip)]
    pub raw: Vec<u8>,
}

impl PacketRow {
    /// The row for a packet the scanner decoded (`id` 0 until it is stored).
    pub fn from_decoded(p: &scanner_core::scanner::Packet) -> Self {
        PacketRow {
            id: 0,
            time_ns: scanner_core::clock::unix_ns(p.time),
            freq_hz: p.slot.freq,
            channel: p.slot.name.clone(),
            source: p.frame.source.clone(),
            dest: p.frame.dest.clone(),
            path: p.frame.path.join(","),
            text: p.frame.tnc2(),
            raw: p.frame.raw.clone(),
        }
    }
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
    /// Open (creating if needed) the index as its writer: upgrades the
    /// schema and marks rows a crash left unfinished. One process records
    /// into a directory at a time.
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        Self::init(Connection::open(path)?)
    }

    /// Open an existing index to read it while a recorder may be writing
    /// it. Doesn't migrate or touch rows; fails if the index is missing or
    /// at another schema version.
    pub fn open_reader(path: &Path) -> Result<Self, IndexError> {
        Ok(Index {
            db: migrate::open_current(path, &SCHEMA)?,
        })
    }

    pub fn in_memory() -> Result<Self, IndexError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut db: Connection) -> Result<Self, IndexError> {
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        migrate::migrate(&mut db, &SCHEMA)?;
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

    /// Record the Opus copy of a transmission.
    pub fn set_opus(&self, id: i64, path: &str, bytes: u64) -> Result<(), IndexError> {
        self.db.execute(
            "UPDATE transmissions SET opus_path = ?2, opus_bytes = ?3 WHERE id = ?1",
            params![id, path, bytes as i64],
        )?;
        Ok(())
    }

    pub fn insert_packet(&self, p: &PacketRow) -> Result<i64, IndexError> {
        self.db.execute(
            "INSERT INTO packets (time_ns, freq_hz, channel, source, dest, path, text, raw)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                p.time_ns, p.freq_hz, p.channel, p.source, p.dest, p.path, p.text, p.raw
            ],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    /// Packets newest first, optionally only those with a smaller id.
    pub fn packets(
        &self,
        before_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<PacketRow>, IndexError> {
        let mut st = self.db.prepare(
            "SELECT * FROM packets WHERE (?1 IS NULL OR id < ?1)
             ORDER BY id DESC LIMIT ?2",
        )?;
        st.query_map(params![before_id, limit.clamp(1, 1000) as i64], |r| {
            Ok(PacketRow {
                id: r.get("id")?,
                time_ns: r.get("time_ns")?,
                freq_hz: r.get("freq_hz")?,
                channel: r.get("channel")?,
                source: r.get("source")?,
                dest: r.get("dest")?,
                path: r.get("path")?,
                text: r.get("text")?,
                raw: r.get("raw")?,
            })
        })?
        .collect::<rusqlite::Result<_>>()
        .map_err(Into::into)
    }

    /// Remove a row (a recording that was thrown away).
    pub fn delete(&self, id: i64) -> Result<(), IndexError> {
        self.db
            .execute("DELETE FROM transmissions WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Record the tone a transmission carried.
    pub fn set_tone(&self, id: i64, tone: &str) -> Result<(), IndexError> {
        self.db.execute(
            "UPDATE transmissions SET tone = ?2 WHERE id = ?1",
            params![id, tone],
        )?;
        Ok(())
    }

    /// Record the DTMF digits heard in a transmission (`None` clears them).
    pub fn set_dtmf(&self, id: i64, digits: Option<&str>) -> Result<(), IndexError> {
        self.db.execute(
            "UPDATE transmissions SET dtmf = ?2 WHERE id = ?1",
            params![id, digits],
        )?;
        Ok(())
    }

    /// Complete rows with audio still on disk, oldest first, after `after_id`;
    /// for re-scanning recordings.
    pub fn complete_after(&self, after_id: i64, limit: usize) -> Result<Vec<Row>, IndexError> {
        let mut st = self.db.prepare(
            "SELECT * FROM transmissions WHERE status = 'complete' AND id > ?1
             ORDER BY id LIMIT ?2",
        )?;
        Ok(st
            .query_map(params![after_id, limit as i64], row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn get(&self, id: i64) -> Result<Option<Row>, IndexError> {
        self.db
            .query_row("SELECT * FROM transmissions WHERE id = ?1", [id], row)
            .optional()
            .map_err(Into::into)
    }

    /// Most recent first.
    pub fn recent(&self, limit: usize) -> Result<Vec<Row>, IndexError> {
        let mut st = self
            .db
            .prepare("SELECT * FROM transmissions ORDER BY start_ns DESC, id DESC LIMIT ?1")?;
        Ok(st
            .query_map([limit as i64], row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Completed transmissions matching `q`, newest first.
    pub fn search(&self, q: &Query) -> Result<Vec<Row>, IndexError> {
        let mut st = self.db.prepare(&format!(
            "SELECT * FROM transmissions WHERE {SEARCH_WHERE}
             ORDER BY start_ns DESC, id DESC LIMIT ?7"
        ))?;
        let limit = q.limit.clamp(1, 1000) as i64;
        st.query_map(search_params(q, &limit), row)?
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }

    /// Just when and where each transmission matching `q` was, newest
    /// first: enough to draw them on a timeline. `q.limit` up to
    /// [`Self::TIMELINE_MAX`].
    pub fn timeline(&self, q: &Query) -> Result<Vec<Span>, IndexError> {
        let mut st = self.db.prepare(&format!(
            "SELECT id, freq_hz, start_ns, end_ns, audio_secs FROM transmissions
             WHERE {SEARCH_WHERE}
             ORDER BY start_ns DESC, id DESC LIMIT ?7"
        ))?;
        let limit = q.limit.clamp(1, Self::TIMELINE_MAX) as i64;
        st.query_map(search_params(q, &limit), |r| {
            let start_ns: i64 = r.get(2)?;
            let end_ns: Option<i64> = r.get(3)?;
            let audio: Option<f64> = r.get(4)?;
            // The time on air; the audio length when the end is missing.
            let secs = match end_ns {
                Some(e) if e > start_ns => (e - start_ns) as f64 / 1e9,
                _ => audio.unwrap_or(0.0),
            };
            Ok(Span {
                id: r.get(0)?,
                freq_hz: r.get(1)?,
                start_ns,
                secs,
            })
        })?
        .collect::<rusqlite::Result<_>>()
        .map_err(Into::into)
    }

    /// The most rows [`Self::timeline`] returns at once.
    pub const TIMELINE_MAX: usize = 20_000;

    /// Bytes of audio (WAV and Opus) the complete rows hold on disk.
    pub fn usage_bytes(&self) -> Result<u64, IndexError> {
        let n: i64 = self.db.query_row(
            "SELECT COALESCE(SUM(COALESCE(wav_bytes, 0) + COALESCE(opus_bytes, 0)), 0)
             FROM transmissions WHERE status = 'complete'",
            [],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }

    /// The oldest rows whose files retention may delete: complete ones,
    /// and ones a crash interrupted (which may have left `.part` files).
    pub fn oldest_expirable(&self, limit: usize) -> Result<Vec<Row>, IndexError> {
        let mut st = self.db.prepare(
            "SELECT * FROM transmissions WHERE status IN ('complete', 'interrupted')
             ORDER BY start_ns, id LIMIT ?1",
        )?;
        Ok(st
            .query_map([limit as i64], row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Start of the oldest recording still on disk.
    pub fn oldest_complete_ns(&self) -> Result<Option<i64>, IndexError> {
        Ok(self.db.query_row(
            "SELECT MIN(start_ns) FROM transmissions WHERE status = 'complete'",
            [],
            |r| r.get(0),
        )?)
    }

    /// Mark rows whose files are gone. The rows stay, so the log of what
    /// was heard outlives the audio.
    pub fn expire(&self, ids: &[i64]) -> Result<(), IndexError> {
        let tx = self.db.unchecked_transaction()?;
        {
            let mut st = tx.prepare("UPDATE transmissions SET status = 'expired' WHERE id = ?1")?;
            for id in ids {
                st.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn count(&self) -> Result<i64, IndexError> {
        self.db
            .query_row("SELECT COUNT(*) FROM transmissions", [], |r| r.get(0))
            .map_err(Into::into)
    }
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Row> {
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
        opus_path: r.get("opus_path")?,
        opus_bytes: r.get("opus_bytes")?,
        tone: r.get("tone")?,
        dtmf: r.get("dtmf")?,
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
        assert_eq!(r.opus_path, None);
        ix.set_opus(id, "2026/11/22/x.opus", 14_002).unwrap();
        let r = ix.get(id).unwrap().unwrap();
        assert_eq!(r.opus_path.as_deref(), Some("2026/11/22/x.opus"));
        assert_eq!(r.opus_bytes, Some(14_002));
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
        // The timeline takes the same filters.
        let spans = ix.timeline(&q).unwrap();
        assert_eq!(spans.iter().map(|s| s.id).collect::<Vec<_>>(), vec![4, 1]);
        assert_eq!(spans[0].freq_hz, 146.52e6);
        assert_eq!(spans[0].start_ns, 3_000_000_000);
        // No end time recorded, so the audio length stands in.
        assert_eq!(spans[0].secs, 4.0);
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

    /// The schema every migration adds up to. A change here should come
    /// with a new migration, never an edit to an old one.
    #[test]
    fn schema_snapshot() {
        let ix = Index::in_memory().unwrap();
        assert_eq!(
            migrate::schema_sql(&ix.db).unwrap(),
            include_str!("../schema/index.sql")
        );
    }

    #[test]
    fn adopts_an_index_from_before_migrations() {
        // What PR 1 to PR 5 created: the same table, user_version 1 and no
        // application id.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("index.sqlite");
        let old = Connection::open(&p).unwrap();
        old.execute_batch(V1).unwrap();
        old.pragma_update(None, "user_version", 1).unwrap();
        old.execute(
            "INSERT INTO transmissions (freq_hz, origin, mode, start_ns, audio_rate, wav_path, status)
             VALUES (147.52e6, 'grid', 'nbfm', 1, 8000, 'a.wav', 'complete')",
            [],
        )
        .unwrap();
        drop(old);
        let ix = Index::open(&p).unwrap();
        assert_eq!(ix.count().unwrap(), 1);
        let r = ix.get(1).unwrap().unwrap();
        assert_eq!((r.wav_path.as_str(), r.opus_path), ("a.wav", None));
        let app: i32 = ix
            .db
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .unwrap();
        assert_eq!(app, SCHEMA.application_id);
    }

    #[test]
    fn readers_leave_rows_being_recorded_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("index.sqlite");
        assert!(Index::open_reader(&p).is_err());
        let w = Index::open(&p).unwrap();
        let id = w
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
        let r = Index::open_reader(&p).unwrap();
        assert_eq!(r.get(id).unwrap().unwrap().status, "recording");
    }

    #[test]
    fn dtmf_tags_and_search() {
        let ix = Index::in_memory().unwrap();
        let mut ids = Vec::new();
        for (i, digits) in [Some("*73 1234#"), None, Some("5")].into_iter().enumerate() {
            let id = ix
                .insert(&NewRow {
                    freq_hz: 147.52e6,
                    channel: None,
                    origin: "listed",
                    mode: "nbfm",
                    start_ns: 1_795_330_443_000_000_000 + i as i64,
                    audio_rate: 8000,
                    wav_path: "x.wav",
                })
                .unwrap();
            ix.complete(id, 0, 1.0, 1.0, 20.0, 10.0, "x.wav", 100)
                .unwrap();
            ix.set_dtmf(id, digits).unwrap();
            ids.push(id);
        }
        let find = |d: &str| -> Vec<i64> {
            let q = Query {
                dtmf: Some(d.into()),
                limit: 10,
                ..Default::default()
            };
            ix.search(&q).unwrap().iter().map(|r| r.id).collect()
        };
        assert_eq!(find("1234"), vec![ids[0]]);
        assert_eq!(find(""), vec![ids[2], ids[0]]);
        assert_eq!(find("9"), Vec::<i64>::new());
        assert_eq!(
            ix.get(ids[0]).unwrap().unwrap().dtmf.as_deref(),
            Some("*73 1234#")
        );
        assert_eq!(ix.complete_after(ids[0], 10).unwrap().len(), 2);
    }
}
