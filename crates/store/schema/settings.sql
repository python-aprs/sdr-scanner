CREATE TABLE channel_overrides (
    freq_hz    INTEGER PRIMARY KEY,
    locked_out INTEGER,
    priority   INTEGER,
    updated_ns INTEGER NOT NULL,
    updated_by TEXT    NOT NULL
);
CREATE TABLE egress (
    month TEXT    NOT NULL,
    role  TEXT    NOT NULL,
    bytes INTEGER NOT NULL,
    PRIMARY KEY (month, role)
);
