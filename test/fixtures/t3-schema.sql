-- Synthetic subset of T3 c4ce7b0's V2 projection columns consumed by the adapter, with
-- every index that revision's migrations create over only these columns, copied verbatim,
-- so query plans match a real database. A new pin re-copies them from its migrations.
CREATE TABLE orchestration_events(sequence INTEGER PRIMARY KEY,aggregate_kind TEXT,application_event_version INTEGER,stream_id TEXT,event_type TEXT,occurred_at TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_provider_threads(provider_thread_id TEXT PRIMARY KEY,thread_id TEXT,driver TEXT,provider_instance_id TEXT,updated_at TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_provider_turns(provider_turn_id TEXT PRIMARY KEY,thread_id TEXT,provider_thread_id TEXT,run_attempt_id TEXT,status TEXT,started_at TEXT,completed_at TEXT,payload_json TEXT,ordinal INTEGER NOT NULL);
CREATE TABLE orchestration_v2_projection_run_attempts(attempt_id TEXT PRIMARY KEY,thread_id TEXT,provider_thread_id TEXT,run_id TEXT,provider_instance_id TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_runs(run_id TEXT PRIMARY KEY,thread_id TEXT,ordinal INTEGER,status TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_messages(message_id TEXT PRIMARY KEY,thread_id TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_threads(thread_id TEXT PRIMARY KEY,provider_instance_id TEXT,active_provider_thread_id TEXT,updated_at TEXT,archived_at TEXT,deleted_at TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_runtime_requests(runtime_request_id TEXT PRIMARY KEY,thread_id TEXT,status TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_context_transfers(context_transfer_id TEXT PRIMARY KEY,target_thread_id TEXT,type TEXT,status TEXT,payload_json TEXT);
CREATE TABLE orchestration_v2_projection_turn_items(turn_item_id TEXT PRIMARY KEY,thread_id TEXT,provider_turn_id TEXT,type TEXT,status TEXT,payload_json TEXT,updated_at TEXT,run_id TEXT);
CREATE TABLE orchestration_v2_projection_metadata(projection_name TEXT PRIMARY KEY,schema_version INTEGER,last_sequence INTEGER,updated_at TEXT);
CREATE INDEX idx_orch_events_stream_sequence ON orchestration_events(aggregate_kind, stream_id, sequence);
CREATE INDEX idx_orchestration_events_agent_stream_sequence ON orchestration_events(stream_id, sequence) WHERE application_event_version = 2 AND aggregate_kind = 'thread';
CREATE INDEX idx_orchestration_events_application_high_water ON orchestration_events(sequence) WHERE aggregate_kind = 'project' OR (application_event_version = 2 AND aggregate_kind = 'thread');
CREATE INDEX idx_orchestration_events_application_sequence ON orchestration_events(application_event_version, sequence);
CREATE INDEX orchestration_events_v2_created_threads_idx ON orchestration_events(stream_id) WHERE application_event_version = 2 AND aggregate_kind = 'thread' AND event_type = 'thread.created';
CREATE INDEX orchestration_v2_projection_provider_threads_thread_idx ON orchestration_v2_projection_provider_threads(thread_id);
CREATE INDEX orchestration_v2_projection_provider_turns_thread_idx ON orchestration_v2_projection_provider_turns(thread_id);
CREATE UNIQUE INDEX orchestration_v2_projection_provider_turns_thread_ordinal_idx ON orchestration_v2_projection_provider_turns(provider_thread_id, ordinal);
CREATE INDEX orchestration_v2_provider_turns_codex_native_id_idx ON orchestration_v2_projection_provider_turns (json_extract(payload_json, '$.nativeTurnRef.nativeId')) WHERE json_extract(payload_json, '$.nativeTurnRef.driver') = 'codex';
CREATE INDEX orchestration_v2_projection_run_attempts_thread_idx ON orchestration_v2_projection_run_attempts(thread_id, run_id);
CREATE INDEX orchestration_v2_projection_runs_delegated_delivery_idx ON orchestration_v2_projection_runs(thread_id) WHERE CASE WHEN json_valid(payload_json) THEN json_type(payload_json, '$.delegatedCompletion.delivery') = 'object' ELSE 0 END;
CREATE INDEX orchestration_v2_projection_runs_recovery_idx ON orchestration_v2_projection_runs(status, thread_id) WHERE status IN ('queued', 'preparing', 'starting', 'running', 'waiting');
CREATE UNIQUE INDEX orchestration_v2_projection_runs_thread_ordinal_idx ON orchestration_v2_projection_runs(thread_id, ordinal);
CREATE INDEX orchestration_v2_projection_runs_thread_status_idx ON orchestration_v2_projection_runs(thread_id, status);
CREATE INDEX orchestration_v2_projection_requests_recovery_idx ON orchestration_v2_projection_runtime_requests(thread_id) WHERE status = 'pending';
CREATE INDEX orchestration_v2_projection_runtime_requests_thread_status_idx ON orchestration_v2_projection_runtime_requests(thread_id, status);
CREATE INDEX orchestration_v2_projection_context_transfers_target_thread_idx ON orchestration_v2_projection_context_transfers(target_thread_id, status);
CREATE INDEX orchestration_v2_projection_turn_items_active_run_idx ON orchestration_v2_projection_turn_items(thread_id, run_id) WHERE status IN ('pending', 'running', 'waiting');
CREATE INDEX orchestration_v2_projection_turn_items_provider_turn_idx ON orchestration_v2_projection_turn_items(provider_turn_id);
CREATE INDEX orchestration_v2_projection_turn_items_recovery_idx ON orchestration_v2_projection_turn_items(thread_id) WHERE type IN ('command_execution', 'dynamic_tool', 'subagent') AND status IN ('pending', 'running', 'waiting');
CREATE INDEX orchestration_v2_projection_turn_items_shell_pending_idx ON orchestration_v2_projection_turn_items(thread_id, run_id) WHERE type IN ('command_execution', 'dynamic_tool', 'subagent') AND status NOT IN ('completed', 'interrupted', 'failed', 'cancelled');
CREATE INDEX orchestration_v2_projection_turn_items_stop_request_idx ON orchestration_v2_projection_turn_items(thread_id, run_id) WHERE type = 'run_interrupt_request';
CREATE INDEX orchestration_v2_projection_turn_items_thread_run_idx ON orchestration_v2_projection_turn_items(thread_id, run_id);
CREATE INDEX orchestration_v2_projection_turn_items_waiting_link_idx ON orchestration_v2_projection_turn_items(thread_id, updated_at DESC, turn_item_id DESC) WHERE type = 'link_request' AND status = 'waiting';
