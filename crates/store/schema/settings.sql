CREATE TABLE channel_overrides (
    freq_hz    INTEGER PRIMARY KEY,
    locked_out INTEGER,
    priority   INTEGER,
    updated_ns INTEGER NOT NULL,
    updated_by TEXT    NOT NULL
);
