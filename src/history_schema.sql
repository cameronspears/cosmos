-- Cosmos suggestion history schema
-- Stored in .cosmos/history.db

-- Main suggestions table
CREATE TABLE IF NOT EXISTS suggestions (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    priority TEXT NOT NULL,
    file TEXT NOT NULL,
    line INTEGER,
    summary TEXT NOT NULL,
    detail TEXT,
    source TEXT NOT NULL,
    created_at TEXT NOT NULL,
    outcome TEXT NOT NULL DEFAULT 'pending',
    outcome_at TEXT,
    file_hash TEXT
);

-- Index for file lookups
CREATE INDEX IF NOT EXISTS idx_suggestions_file ON suggestions(file);

-- Index for outcome queries
CREATE INDEX IF NOT EXISTS idx_suggestions_outcome ON suggestions(outcome);

-- Index for date range queries
CREATE INDEX IF NOT EXISTS idx_suggestions_created ON suggestions(created_at);

-- Occurrence tracking for "previously seen" detection
CREATE TABLE IF NOT EXISTS occurrence_counts (
    summary_hash TEXT NOT NULL,
    file TEXT NOT NULL,
    count INTEGER NOT NULL DEFAULT 1,
    last_seen TEXT NOT NULL,
    PRIMARY KEY (summary_hash, file)
);

-- Session tracking for analytics
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    suggestions_generated INTEGER DEFAULT 0,
    suggestions_applied INTEGER DEFAULT 0,
    suggestions_dismissed INTEGER DEFAULT 0,
    tokens_used INTEGER DEFAULT 0,
    cost_usd REAL DEFAULT 0.0
);

-- Index for session date queries
CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at);


