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
        Migration {
            description: "starred recordings",
            up: Up::Sql("ALTER TABLE transmissions ADD COLUMN starred INTEGER NOT NULL DEFAULT 0;"),
        },
        Migration {
            description: "digital voice calls",
            up: Up::Sql(
                "ALTER TABLE transmissions ADD COLUMN timeslot INTEGER;
                 ALTER TABLE transmissions ADD COLUMN color_code INTEGER;
                 ALTER TABLE transmissions ADD COLUMN call_type TEXT;
                 ALTER TABLE transmissions ADD COLUMN src_id INTEGER;
                 ALTER TABLE transmissions ADD COLUMN dst_id INTEGER;
                 ALTER TABLE transmissions ADD COLUMN alias TEXT;
                 ALTER TABLE transmissions ADD COLUMN encrypted INTEGER;
                 ALTER TABLE transmissions ADD COLUMN amb_bytes INTEGER;
                 CREATE INDEX transmissions_dst ON transmissions(dst_id, start_ns);
                 CREATE INDEX transmissions_src ON transmissions(src_id, start_ns);",
            ),
        },
        Migration {
            description: "mode-specific tags in their own tables",
            up: Up::Sql(V8),
        },
        Migration {
            description: "System Fusion calls",
            up: Up::Sql(
                "CREATE TABLE ysf_calls (
                    transmission_id INTEGER PRIMARY KEY REFERENCES transmissions(id) ON DELETE CASCADE,
                    data_type       TEXT,
                    src             TEXT,
                    dst             TEXT,
                    downlink        TEXT,
                    uplink          TEXT,
                    dg_id           INTEGER,
                    amb_bytes       INTEGER
                 );
                 CREATE INDEX ysf_calls_src ON ysf_calls(src);",
            ),
        },
    ],
};

/// Moves what only some recordings have out of `transmissions` into tables
/// keyed by the transmission's id: CTCSS/DCS tones and DTMF digits (heard in
/// analog FM audio) and DMR call details. A tag row exists only where there
/// is something to say, and goes when its transmission is deleted.
/// `transmissions` is rebuilt rather than altered column by column, so its
/// definition reads as one table again; ids are kept.
const V8: &str = "
ALTER TABLE transmissions RENAME TO transmissions_v7;
CREATE TABLE transmissions (
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
    opus_path   TEXT,
    opus_bytes  INTEGER,
    status      TEXT    NOT NULL DEFAULT 'recording',
    starred     INTEGER NOT NULL DEFAULT 0
);
INSERT INTO transmissions (id, freq_hz, channel, origin, mode, start_ns, end_ns, audio_secs,
                           signal_secs, peak_snr_db, mean_snr_db, audio_rate, wav_path,
                           wav_bytes, opus_path, opus_bytes, status, starred)
    SELECT id, freq_hz, channel, origin, mode, start_ns, end_ns, audio_secs,
           signal_secs, peak_snr_db, mean_snr_db, audio_rate, wav_path,
           wav_bytes, opus_path, opus_bytes, status, starred
    FROM transmissions_v7;

CREATE TABLE tones (
    transmission_id INTEGER PRIMARY KEY REFERENCES transmissions(id) ON DELETE CASCADE,
    tone            TEXT    NOT NULL
);
INSERT INTO tones (transmission_id, tone)
    SELECT id, tone FROM transmissions_v7 WHERE tone IS NOT NULL;

CREATE TABLE dtmf (
    transmission_id INTEGER PRIMARY KEY REFERENCES transmissions(id) ON DELETE CASCADE,
    digits          TEXT    NOT NULL
);
INSERT INTO dtmf (transmission_id, digits)
    SELECT id, dtmf FROM transmissions_v7 WHERE dtmf IS NOT NULL;

CREATE TABLE dmr_calls (
    transmission_id INTEGER PRIMARY KEY REFERENCES transmissions(id) ON DELETE CASCADE,
    timeslot        INTEGER,
    color_code      INTEGER,
    call_type       TEXT,
    src_id          INTEGER,
    dst_id          INTEGER,
    alias           TEXT,
    encrypted       INTEGER NOT NULL DEFAULT 0,
    amb_bytes       INTEGER
);
INSERT INTO dmr_calls (transmission_id, timeslot, color_code, call_type, src_id, dst_id,
                       alias, encrypted, amb_bytes)
    SELECT id, timeslot, color_code, call_type, src_id, dst_id, alias,
           COALESCE(encrypted, 0), amb_bytes
    FROM transmissions_v7
    WHERE timeslot IS NOT NULL OR color_code IS NOT NULL OR call_type IS NOT NULL
       OR src_id IS NOT NULL OR dst_id IS NOT NULL OR alias IS NOT NULL
       OR encrypted IS NOT NULL OR amb_bytes IS NOT NULL;

DROP TABLE transmissions_v7;
CREATE INDEX transmissions_start ON transmissions(start_ns);
CREATE INDEX transmissions_freq ON transmissions(freq_hz, start_ns);
CREATE INDEX dmr_calls_dst ON dmr_calls(dst_id);
CREATE INDEX dmr_calls_src ON dmr_calls(src_id);
";

/// A transmission with its tags, as [`row`] reads it. Every query that
/// returns [`Row`]s selects from this.
const ROWS: &str = concat!(
    "SELECT t.*, tn.tone, d.digits AS dtmf,
        c.timeslot, c.color_code, c.call_type, c.src_id, c.dst_id, c.alias, c.encrypted,
        COALESCE(c.amb_bytes, y.amb_bytes) AS amb_bytes,
        y.transmission_id AS ysf_id, y.data_type AS ysf_data_type, y.src AS ysf_src,
        y.dst AS ysf_dst, y.downlink AS ysf_downlink, y.uplink AS ysf_uplink,
        y.dg_id AS ysf_dg_id ",
    tagged!()
);

/// `transmissions t` with its tag tables `tn` (tones), `d` (DTMF), `c`
/// (DMR calls) and `y` (System Fusion calls) joined on.
macro_rules! tagged {
    () => {
        "FROM transmissions t
         LEFT JOIN tones tn ON tn.transmission_id = t.id
         LEFT JOIN dtmf d ON d.transmission_id = t.id
         LEFT JOIN dmr_calls c ON c.transmission_id = t.id
         LEFT JOIN ysf_calls y ON y.transmission_id = t.id"
    };
}
use tagged;

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
    /// Kept by retention however old (set from the web UI).
    pub starred: bool,
    /// Digital voice (DMR) calls: timeslot (none for a radio heard
    /// directly), colour code, `group`/`private`, source and destination
    /// ids (the destination of a group call is its talkgroup), talker
    /// alias, and whether it was encrypted.
    pub timeslot: Option<i64>,
    pub color_code: Option<i64>,
    pub call_type: Option<String>,
    pub src_id: Option<i64>,
    pub dst_id: Option<i64>,
    pub alias: Option<String>,
    pub encrypted: Option<bool>,
    /// Size of the raw voice frames kept beside the audio (`.amb`).
    pub amb_bytes: Option<i64>,
    /// A System Fusion call's details.
    pub ysf: Option<YsfCall>,
}

/// What a System Fusion call said about itself.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct YsfCall {
    /// `dn`, `vw`, `vd1` or `data`.
    pub data_type: Option<String>,
    /// Calling and called callsigns (the called one is usually `ALL`).
    pub src: Option<String>,
    pub dst: Option<String>,
    /// The repeater heard and the one talked into, as the radio set them.
    pub downlink: Option<String>,
    pub uplink: Option<String>,
    pub dg_id: Option<i64>,
}

/// What a digital call said about itself, for [`Index::set_digital`].
#[derive(Debug, Clone, PartialEq)]
pub enum Digital {
    Dmr(DmrCall),
    Ysf(YsfCall),
}

impl Digital {
    pub fn from_call(i: &scanner_core::demod::CallInfo) -> Self {
        use scanner_core::demod::CallInfo;
        match i {
            CallInfo::Dmr(i) => Digital::Dmr(DmrCall::from_call(i)),
            CallInfo::Ysf(i) => Digital::Ysf(YsfCall {
                data_type: i.mode().map(Into::into),
                src: i.src.clone(),
                dst: i.dst.clone(),
                downlink: i.downlink.clone(),
                uplink: i.uplink.clone(),
                dg_id: i.dg_id.map(Into::into),
            }),
        }
    }
}

/// What a DMR call said about itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DmrCall {
    pub timeslot: Option<u8>,
    pub color_code: Option<u8>,
    pub call_type: Option<&'static str>,
    pub src_id: Option<u32>,
    pub dst_id: Option<u32>,
    pub alias: Option<String>,
    pub encrypted: bool,
}

impl DmrCall {
    pub fn from_call(i: &scanner_core::dmr::CallInfo) -> Self {
        DmrCall {
            timeslot: i.timeslot,
            color_code: i.color_code,
            call_type: i.call_type(),
            src_id: i.lc.map(|l| l.src),
            dst_id: i.lc.map(|l| l.dst),
            alias: i.alias.clone(),
            encrypted: i.encrypted,
        }
    }
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
    /// Only starred (`true`) or only unstarred (`false`) recordings.
    pub starred: Option<bool>,
    /// Digital calls to this destination (talkgroup or radio id).
    pub dst_id: Option<i64>,
    /// Digital calls from this radio id.
    pub src_id: Option<i64>,
    /// System Fusion calls from this callsign (any case).
    pub callsign: Option<String>,
    /// Paging: only rows with a smaller id.
    pub before_id: Option<i64>,
    pub limit: usize,
}

/// The filters [`Index::search`] and [`Index::timeline`] share, over
/// [`ROWS`]' tables; parameters as [`search_params`] binds them (`?7` is
/// the limit).
const SEARCH_WHERE: &str = "t.status = 'complete'
    AND (?1 IS NULL OR t.freq_hz >= ?1) AND (?2 IS NULL OR t.freq_hz <= ?2)
    AND (?3 IS NULL OR t.start_ns >= ?3) AND (?4 IS NULL OR t.start_ns < ?4)
    AND (?5 IS NULL OR t.audio_secs >= ?5)
    AND (?6 IS NULL OR t.id < ?6)
    AND (?8 IS NULL OR tn.tone = ?8)
    AND (?9 IS NULL OR instr(d.digits, ?9) > 0)
    AND (?10 IS NULL OR t.starred = ?10)
    AND (?11 IS NULL OR c.dst_id = ?11)
    AND (?12 IS NULL OR c.src_id = ?12)
    AND (?13 IS NULL OR y.src = ?13 COLLATE NOCASE)";

fn search_params<'a>(q: &'a Query, limit: &'a i64) -> [&'a dyn rusqlite::ToSql; 13] {
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
        &q.starred,
        &q.dst_id,
        &q.src_id,
        &q.callsign,
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
        let db = migrate::open_current(path, &SCHEMA)?;
        // A hand delete from the web UI takes the row's tags with it.
        db.pragma_update(None, "foreign_keys", true)?;
        Ok(Index { db })
    }

    pub fn in_memory() -> Result<Self, IndexError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut db: Connection) -> Result<Self, IndexError> {
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        migrate::migrate(&mut db, &SCHEMA)?;
        db.pragma_update(None, "foreign_keys", true)?;
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

    /// Star or unstar a recording. Returns false if there is no such row.
    pub fn set_starred(&self, id: i64, starred: bool) -> Result<bool, IndexError> {
        let n = self.db.execute(
            "UPDATE transmissions SET starred = ?2 WHERE id = ?1",
            params![id, starred],
        )?;
        Ok(n > 0)
    }

    /// Remove a row (a recording that was thrown away).
    /// Its tags go with it (foreign keys are on for every connection).
    pub fn delete(&self, id: i64) -> Result<(), IndexError> {
        self.db
            .execute("DELETE FROM transmissions WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Record the tone a transmission carried.
    pub fn set_tone(&self, id: i64, tone: &str) -> Result<(), IndexError> {
        self.db.execute(
            "INSERT INTO tones (transmission_id, tone) VALUES (?1, ?2)
             ON CONFLICT (transmission_id) DO UPDATE SET tone = excluded.tone",
            params![id, tone],
        )?;
        Ok(())
    }

    /// Record what a digital call said about itself, and the size of its
    /// raw voice frames file.
    pub fn set_digital(&self, id: i64, d: &Digital, amb_bytes: u64) -> Result<(), IndexError> {
        let d = match d {
            Digital::Dmr(d) => d,
            Digital::Ysf(y) => {
                self.db.execute(
                    "INSERT OR REPLACE INTO ysf_calls (transmission_id, data_type, src, dst,
                         downlink, uplink, dg_id, amb_bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        id,
                        y.data_type,
                        y.src,
                        y.dst,
                        y.downlink,
                        y.uplink,
                        y.dg_id,
                        (amb_bytes > 0).then_some(amb_bytes as i64)
                    ],
                )?;
                return Ok(());
            }
        };
        self.db.execute(
            "INSERT OR REPLACE INTO dmr_calls (transmission_id, timeslot, color_code, call_type,
                 src_id, dst_id, alias, encrypted, amb_bytes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                d.timeslot,
                d.color_code,
                d.call_type,
                d.src_id,
                d.dst_id,
                d.alias,
                d.encrypted,
                amb_bytes as i64
            ],
        )?;
        Ok(())
    }

    /// Record the DTMF digits heard in a transmission (`None` clears them).
    pub fn set_dtmf(&self, id: i64, digits: Option<&str>) -> Result<(), IndexError> {
        match digits {
            Some(d) => self.db.execute(
                "INSERT INTO dtmf (transmission_id, digits) VALUES (?1, ?2)
                 ON CONFLICT (transmission_id) DO UPDATE SET digits = excluded.digits",
                params![id, d],
            )?,
            None => self
                .db
                .execute("DELETE FROM dtmf WHERE transmission_id = ?1", [id])?,
        };
        Ok(())
    }

    /// Complete rows with audio still on disk, oldest first, after `after_id`;
    /// for re-scanning recordings.
    pub fn complete_after(&self, after_id: i64, limit: usize) -> Result<Vec<Row>, IndexError> {
        let mut st = self.db.prepare(&format!(
            "{ROWS} WHERE t.status = 'complete' AND t.id > ?1 ORDER BY t.id LIMIT ?2"
        ))?;
        Ok(st
            .query_map(params![after_id, limit as i64], row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn get(&self, id: i64) -> Result<Option<Row>, IndexError> {
        self.db
            .query_row(&format!("{ROWS} WHERE t.id = ?1"), [id], row)
            .optional()
            .map_err(Into::into)
    }

    /// Most recent first.
    pub fn recent(&self, limit: usize) -> Result<Vec<Row>, IndexError> {
        let mut st = self.db.prepare(&format!(
            "{ROWS} ORDER BY t.start_ns DESC, t.id DESC LIMIT ?1"
        ))?;
        Ok(st
            .query_map([limit as i64], row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Completed transmissions matching `q`, newest first.
    pub fn search(&self, q: &Query) -> Result<Vec<Row>, IndexError> {
        let mut st = self.db.prepare(&format!(
            "{ROWS} WHERE {SEARCH_WHERE}
             ORDER BY t.start_ns DESC, t.id DESC LIMIT ?7"
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
            "SELECT t.id, t.freq_hz, t.start_ns, t.end_ns, t.audio_secs {}
             WHERE {SEARCH_WHERE}
             ORDER BY t.start_ns DESC, t.id DESC LIMIT ?7",
            tagged!()
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
            "SELECT COALESCE(SUM(COALESCE(t.wav_bytes, 0) + COALESCE(t.opus_bytes, 0)
                 + COALESCE(c.amb_bytes, y.amb_bytes, 0)), 0)
             FROM transmissions t LEFT JOIN dmr_calls c ON c.transmission_id = t.id
                 LEFT JOIN ysf_calls y ON y.transmission_id = t.id
             WHERE t.status = 'complete'",
            [],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }

    /// The oldest rows whose files retention may delete: complete ones,
    /// and ones a crash interrupted (which may have left `.part` files).
    /// Starred rows are never among them.
    pub fn oldest_expirable(&self, limit: usize) -> Result<Vec<Row>, IndexError> {
        let mut st = self.db.prepare(&format!(
            "{ROWS} WHERE t.status IN ('complete', 'interrupted') AND t.starred = 0
             ORDER BY t.start_ns, t.id LIMIT ?1"
        ))?;
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
        starred: r.get("starred")?,
        timeslot: r.get("timeslot")?,
        color_code: r.get("color_code")?,
        call_type: r.get("call_type")?,
        src_id: r.get("src_id")?,
        dst_id: r.get("dst_id")?,
        alias: r.get("alias")?,
        encrypted: r.get("encrypted")?,
        amb_bytes: r.get("amb_bytes")?,
        ysf: match r.get::<_, Option<i64>>("ysf_id")? {
            None => None,
            Some(_) => Some(YsfCall {
                data_type: r.get("ysf_data_type")?,
                src: r.get("ysf_src")?,
                dst: r.get("ysf_dst")?,
                downlink: r.get("ysf_downlink")?,
                uplink: r.get("ysf_uplink")?,
                dg_id: r.get("ysf_dg_id")?,
            }),
        },
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

    /// Version 8 moves tones, DTMF digits and DMR call details out of
    /// `transmissions`; what a version 7 index held reads back the same.
    #[test]
    fn moves_mode_tags_out_of_transmissions() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("index.sqlite");
        let v7 = Schema {
            migrations: &SCHEMA.migrations[..7],
            ..SCHEMA
        };
        let mut old = Connection::open(&p).unwrap();
        migrate::migrate(&mut old, &v7).unwrap();
        old.execute_batch(
            "INSERT INTO transmissions (id, freq_hz, origin, mode, start_ns, audio_rate, wav_path,
                 wav_bytes, opus_path, opus_bytes, status, tone, dtmf, starred)
             VALUES (1, 146.52e6, 'listed', 'nbfm', 10, 8000, 'a.wav', 100, 'a.opus', 10,
                 'complete', '100.0', '*73 1234#', 1);
             INSERT INTO transmissions (id, freq_hz, origin, mode, start_ns, audio_rate, wav_path,
                 wav_bytes, status, timeslot, color_code, call_type, src_id, dst_id, alias,
                 encrypted, amb_bytes)
             VALUES (2, 145.79e6, 'listed', 'dmr', 20, 8000, 'b.wav', 200, 'complete', 1, 1,
                 'group', 3171501, 99, 'KF7HVM', 0, 54);
             INSERT INTO transmissions (id, freq_hz, origin, mode, start_ns, audio_rate, wav_path,
                 wav_bytes, status)
             VALUES (3, 147.52e6, 'grid', 'nbfm', 30, 8000, 'c.wav', 300, 'expired');",
        )
        .unwrap();
        drop(old);

        let ix = Index::open(&p).unwrap();
        let bak = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".v7-"));
        assert!(bak, "backup of the version 7 index");
        let cols: Vec<String> = ix
            .db
            .prepare("SELECT name FROM pragma_table_info('transmissions')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for gone in ["tone", "dtmf", "timeslot", "dst_id", "amb_bytes"] {
            assert!(!cols.iter().any(|c| c == gone), "{gone} still there");
        }
        let count = |t: &str| -> i64 {
            ix.db
                .query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            (count("tones"), count("dtmf"), count("dmr_calls")),
            (1, 1, 1)
        );

        let fm = ix.get(1).unwrap().unwrap();
        assert_eq!(fm.tone.as_deref(), Some("100.0"));
        assert_eq!(fm.dtmf.as_deref(), Some("*73 1234#"));
        assert_eq!(fm.opus_path.as_deref(), Some("a.opus"));
        assert!(fm.starred);
        assert_eq!((fm.dst_id, fm.encrypted, fm.amb_bytes), (None, None, None));
        let dmr = ix.get(2).unwrap().unwrap();
        assert_eq!(
            (dmr.timeslot, dmr.color_code, dmr.call_type.as_deref()),
            (Some(1), Some(1), Some("group"))
        );
        assert_eq!((dmr.src_id, dmr.dst_id), (Some(3_171_501), Some(99)));
        assert_eq!(dmr.alias.as_deref(), Some("KF7HVM"));
        assert_eq!((dmr.encrypted, dmr.amb_bytes), (Some(false), Some(54)));
        assert_eq!((dmr.tone, dmr.dtmf), (None, None));
        assert_eq!(ix.get(3).unwrap().unwrap().status, "expired");

        // Filters on the moved tags find the same rows, and the DMR call's
        // voice frames still count towards disk use.
        let ids = |q: Query| -> Vec<i64> {
            let q = Query { limit: 10, ..q };
            ix.search(&q).unwrap().iter().map(|r| r.id).collect()
        };
        assert_eq!(ids(Query::default()), vec![2, 1]);
        assert_eq!(
            ids(Query {
                tone: Some("100.0".into()),
                ..Default::default()
            }),
            vec![1]
        );
        assert_eq!(
            ids(Query {
                dtmf: Some(String::new()),
                ..Default::default()
            }),
            vec![1]
        );
        assert_eq!(
            ids(Query {
                dst_id: Some(99),
                ..Default::default()
            }),
            vec![2]
        );
        assert_eq!(
            ids(Query {
                src_id: Some(3_171_501),
                ..Default::default()
            }),
            vec![2]
        );
        assert_eq!(
            ids(Query {
                starred: Some(true),
                ..Default::default()
            }),
            vec![1]
        );
        assert_eq!(ix.usage_bytes().unwrap(), 100 + 10 + 200 + 54);
    }

    #[test]
    fn tags_follow_their_transmission() {
        let ix = Index::in_memory().unwrap();
        let id = ix
            .insert(&NewRow {
                freq_hz: 145.79e6,
                channel: None,
                origin: "listed",
                mode: "dmr",
                start_ns: 0,
                audio_rate: 8000,
                wav_path: "x.wav",
            })
            .unwrap();
        ix.set_tone(id, "100.0").unwrap();
        ix.set_tone(id, "D023N").unwrap();
        ix.set_dtmf(id, Some("5")).unwrap();
        let d = DmrCall {
            color_code: Some(1),
            call_type: Some("group"),
            src_id: Some(1),
            dst_id: Some(99),
            ..Default::default()
        };
        ix.set_digital(id, &Digital::Dmr(d.clone()), 10).unwrap();
        ix.set_digital(
            id,
            &Digital::Dmr(DmrCall {
                dst_id: Some(9),
                ..d
            }),
            20,
        )
        .unwrap();
        let r = ix.get(id).unwrap().unwrap();
        assert_eq!(r.tone.as_deref(), Some("D023N"));
        assert_eq!((r.dst_id, r.amb_bytes), (Some(9), Some(20)));
        ix.set_dtmf(id, None).unwrap();
        assert_eq!(ix.get(id).unwrap().unwrap().dtmf, None);

        ix.delete(id).unwrap();
        for t in ["tones", "dtmf", "dmr_calls", "ysf_calls"] {
            let n: i64 = ix
                .db
                .query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{t} kept a deleted transmission's tag");
        }
    }

    #[test]
    fn fusion_calls_have_their_own_table() {
        let ix = Index::in_memory().unwrap();
        let add = |wav: &str| {
            let id = ix
                .insert(&NewRow {
                    freq_hz: 442.175e6,
                    channel: None,
                    origin: "listed",
                    mode: "ysf",
                    start_ns: 0,
                    audio_rate: 8000,
                    wav_path: wav,
                })
                .unwrap();
            ix.complete(id, 1, 1.0, 1.0, 30.0, 20.0, wav, 100).unwrap();
            id
        };
        let (a, b) = (add("a.wav"), add("b.wav"));
        let call = YsfCall {
            data_type: Some("dn".into()),
            src: Some("KF7HVM".into()),
            dst: Some("ALL".into()),
            downlink: None,
            uplink: Some("W7ABC".into()),
            dg_id: Some(21),
        };
        ix.set_digital(a, &Digital::Ysf(call.clone()), 40).unwrap();
        let r = ix.get(a).unwrap().unwrap();
        assert_eq!(r.ysf.as_ref(), Some(&call));
        assert_eq!((r.amb_bytes, r.src_id), (Some(40), None));
        assert_eq!(ix.get(b).unwrap().unwrap().ysf, None);
        let found = ix
            .search(&Query {
                callsign: Some("kf7hvm".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(found.iter().map(|r| r.id).collect::<Vec<_>>(), vec![a]);
        assert_eq!(ix.usage_bytes().unwrap(), 100 + 100 + 40);
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["ysf"]["src"], "KF7HVM");
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
