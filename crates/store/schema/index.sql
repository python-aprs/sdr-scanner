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
CREATE TABLE dstar_calls (
    transmission_id INTEGER PRIMARY KEY REFERENCES transmissions(id) ON DELETE CASCADE,
    my              TEXT,
    suffix          TEXT,
    your            TEXT,
    rpt1            TEXT,
    rpt2            TEXT,
    message         TEXT,
    amb_bytes       INTEGER
);
CREATE TABLE dtmf (
    transmission_id INTEGER PRIMARY KEY REFERENCES transmissions(id) ON DELETE CASCADE,
    digits          TEXT    NOT NULL
);
CREATE TABLE packets (
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
CREATE TABLE tones (
    transmission_id INTEGER PRIMARY KEY REFERENCES transmissions(id) ON DELETE CASCADE,
    tone            TEXT    NOT NULL
);
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
CREATE TABLE ysf_calls (
    transmission_id INTEGER PRIMARY KEY REFERENCES transmissions(id) ON DELETE CASCADE,
    data_type       TEXT,
    src             TEXT,
    dst             TEXT,
    downlink        TEXT,
    uplink          TEXT,
    dg_id           INTEGER,
    amb_bytes       INTEGER
);
CREATE INDEX dmr_calls_dst ON dmr_calls(dst_id);
CREATE INDEX dmr_calls_src ON dmr_calls(src_id);
CREATE INDEX dstar_calls_my ON dstar_calls(my);
CREATE INDEX packets_time ON packets(time_ns);
CREATE INDEX transmissions_freq ON transmissions(freq_hz, start_ns);
CREATE INDEX transmissions_start ON transmissions(start_ns);
CREATE INDEX ysf_calls_src ON ysf_calls(src);
