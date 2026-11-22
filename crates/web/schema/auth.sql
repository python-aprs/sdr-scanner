CREATE TABLE admins (
    id INTEGER PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    created_ns INTEGER NOT NULL
);
CREATE TABLE api_tokens (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    created_ns INTEGER NOT NULL,
    expires_ns INTEGER,
    last_used_ns INTEGER,
    revoked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE audit (
    id INTEGER PRIMARY KEY,
    ts_ns INTEGER NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    detail TEXT NOT NULL,
    client TEXT NOT NULL
);
CREATE TABLE sessions (
    id_hash TEXT PRIMARY KEY,
    admin_id INTEGER NOT NULL REFERENCES admins(id) ON DELETE CASCADE,
    csrf TEXT NOT NULL,
    created_ns INTEGER NOT NULL,
    last_seen_ns INTEGER NOT NULL
);
