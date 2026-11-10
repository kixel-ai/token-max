-- Synthetic subset of T3 c4ce7b0's V2 projection columns consumed by the adapter,
-- with T3's own indexes on them so query plans match a real database.
CREATE TABLE orchestration_events(sequence INTEGER PRIMARY KEY,aggregate_kind TEXT,application_event_version INTEGER,stream_id TEXT,event_type TEXT,occurred_at TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_provider_threads(provider_thread_id TEXT PRIMARY KEY,thread_id TEXT,driver TEXT,provider_instance_id TEXT,updated_at TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_provider_turns(provider_turn_id TEXT PRIMARY KEY,thread_id TEXT,provider_thread_id TEXT,run_attempt_id TEXT,status TEXT,started_at TEXT,completed_at TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_run_attempts(attempt_id TEXT PRIMARY KEY,thread_id TEXT,provider_thread_id TEXT,run_id TEXT,provider_instance_id TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_runs(run_id TEXT PRIMARY KEY,thread_id TEXT,ordinal INTEGER,status TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_messages(message_id TEXT PRIMARY KEY,thread_id TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_threads(thread_id TEXT PRIMARY KEY,provider_instance_id TEXT,active_provider_thread_id TEXT,updated_at TEXT,archived_at TEXT,deleted_at TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_runtime_requests(thread_id TEXT,status TEXT);
CREATE TABLE orchestration_v2_projection_context_transfers(target_thread_id TEXT,type TEXT,status TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_turn_items(turn_item_id TEXT PRIMARY KEY,thread_id TEXT,provider_turn_id TEXT,type TEXT,status TEXT,payload_json TEXT);
CREATE INDEX idx_orchestration_events_agent_stream_sequence ON orchestration_events(stream_id, sequence) WHERE application_event_version = 2 AND aggregate_kind = 'thread';
CREATE INDEX orchestration_v2_projection_provider_turns_thread_ordinal_idx ON orchestration_v2_projection_provider_turns(provider_thread_id);
CREATE UNIQUE INDEX orchestration_v2_projection_runs_thread_ordinal_idx ON orchestration_v2_projection_runs(thread_id, ordinal);
CREATE INDEX orchestration_v2_projection_runtime_requests_thread_status_idx ON orchestration_v2_projection_runtime_requests(thread_id, status);
CREATE INDEX orchestration_v2_projection_context_transfers_target_thread_idx ON orchestration_v2_projection_context_transfers(target_thread_id, status);
CREATE INDEX orchestration_v2_projection_turn_items_provider_turn_idx ON orchestration_v2_projection_turn_items(provider_turn_id);
CREATE INDEX orchestration_v2_projection_turn_items_active_run_idx ON orchestration_v2_projection_turn_items(thread_id) WHERE status IN ('pending', 'running', 'waiting');
