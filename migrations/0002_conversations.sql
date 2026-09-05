CREATE TABLE message_claims (
  guild_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  message_id TEXT NOT NULL,
  claimed_at_ms INTEGER NOT NULL,
  PRIMARY KEY (guild_id, channel_id, message_id)
) WITHOUT ROWID;
CREATE INDEX message_claims_by_age ON message_claims(claimed_at_ms);

CREATE TABLE run_message_links (
  guild_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  message_id TEXT NOT NULL,
  -- Keep reply linkage after the ephemeral run record is pruned.
  run_id TEXT NOT NULL,
  conversation_id TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  PRIMARY KEY (guild_id, channel_id, message_id)
) WITHOUT ROWID;
CREATE INDEX run_message_links_by_conversation ON run_message_links(conversation_id, created_at_ms DESC, run_id);
CREATE INDEX run_message_links_by_age ON run_message_links(created_at_ms);

CREATE TABLE run_progress (
  run_id TEXT PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE,
  note TEXT,
  activity TEXT,
  note_updated_at_ms INTEGER,
  activity_updated_at_ms INTEGER,
  updated_at_ms INTEGER NOT NULL
) WITHOUT ROWID;

-- Cases intentionally do not reference runs: diagnostic evidence expires before the durable staff-facing record does.
CREATE TABLE case_records (
  run_id TEXT PRIMARY KEY,
  conversation_id TEXT NOT NULL,
  title TEXT NOT NULL,
  summary TEXT NOT NULL,
  source_url TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX case_records_by_conversation ON case_records(conversation_id, created_at_ms DESC);
CREATE INDEX case_records_by_age ON case_records(created_at_ms);

CREATE TABLE case_observations (
  conversation_id TEXT NOT NULL,
  source_url TEXT NOT NULL,
  text TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  PRIMARY KEY (conversation_id, source_url)
) WITHOUT ROWID;
CREATE INDEX case_observations_by_conversation ON case_observations(conversation_id, created_at_ms DESC);
CREATE INDEX case_observations_by_age ON case_observations(created_at_ms);

CREATE TABLE staff_messages (
  guild_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  message_id TEXT NOT NULL,
  author TEXT NOT NULL,
  content TEXT NOT NULL,
  reply_to TEXT,
  created_at_ms INTEGER NOT NULL,
  PRIMARY KEY (guild_id, channel_id, message_id)
) WITHOUT ROWID;
CREATE INDEX staff_messages_by_channel ON staff_messages(guild_id, channel_id, created_at_ms DESC, message_id);
CREATE INDEX staff_messages_by_age ON staff_messages(created_at_ms DESC, guild_id, channel_id, message_id);