CREATE TABLE runs (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL CHECK (kind IN ('bug_report', 'staff_request')),
  title TEXT NOT NULL,
  request TEXT NOT NULL,
  bug_report_id TEXT,
  discord_guild_id TEXT NOT NULL,
  discord_channel_id TEXT NOT NULL,
  discord_message_id TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed')),
  evidence_manifest TEXT,
  final_report TEXT,
  error TEXT,
  created_at_ms INTEGER NOT NULL,
  started_at_ms INTEGER,
  finished_at_ms INTEGER,
  next_event_seq INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX runs_recent ON runs(created_at_ms DESC);

CREATE TABLE run_events (
  run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  occurred_at_ms INTEGER NOT NULL,
  kind TEXT NOT NULL,
  event_json TEXT NOT NULL CHECK (json_valid(event_json)),
  PRIMARY KEY (run_id, seq)
) WITHOUT ROWID;

