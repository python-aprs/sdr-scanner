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
    status      TEXT    NOT NULL DEFAULT 'recording'
, opus_path TEXT, opus_bytes INTEGER, tone TEXT);
CREATE INDEX packets_time ON packets(time_ns);
CREATE INDEX transmissions_freq ON transmissions(freq_hz, start_ns);
CREATE INDEX transmissions_start ON transmissions(start_ns);
