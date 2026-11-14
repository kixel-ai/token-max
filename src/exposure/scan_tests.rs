//! Exposure derived through the whole reader, replay and report pipeline.

use serde_json::json;

use super::*;
use crate::context::control_text;
use crate::controls::Env;
use crate::discovery::DiscoveryOptions;
use crate::model::{ContextCost, ContextKind, MemoryAccess, Report, Scan, Verdict};
use crate::reader::{ScanOptions, scan};
use crate::replay::{AnalyzeOptions, analyze};
use crate::report::{ReportOptions, format_report};
use crate::test_support::{Workspace, prices, report_line};

/// A fixed report time; none of these assertions depend on it.
const NOW: Millis = 1_800_000_000_000;

fn at(second: u32) -> String {
    format!("2026-11-01T00:00:{second:02}.000Z")
}

fn millis(second: u32) -> Millis {
    safe::parse_instant(&at(second)).unwrap()
}

fn codex_meta() -> Value {
    json!({
        "type": "session_meta",
        "timestamp": at(0),
        "payload": {
            "id": "session",
            "model_provider": "openai",
            "cli_version": "0.167.0",
            "originator": "codex_cli_rs",
            "source": "cli",
        },
    })
}

fn turn() -> Value {
    json!({
        "type": "turn_context",
        "timestamp": at(0),
        "payload": { "model": "gpt-6.1-sol", "turn_id": "turn", "service_tier": "standard" },
    })
}

fn billed(second: u32) -> Value {
    json!({
        "type": "token_usage_record",
        "timestamp": at(second),
        "payload": {
            "response_id": format!("response-{second}"),
            "turn_id": "turn",
            "usage": {
                "input_tokens": 1000,
                "output_tokens": 10,
                "cached_input_tokens": 500,
                "cache_write_input_tokens": 0,
            },
        },
    })
}

fn world(second: u32, state: Value, full: bool) -> Value {
    json!({ "type": "world_state", "timestamp": at(second), "payload": { "full": full, "state": state } })
}

fn attachment(second: u32, value: Value) -> Value {
    json!({
        "type": "attachment",
        "timestamp": at(second),
        "sessionId": "session",
        "uuid": format!("attachment-{second}"),
        "version": "2.1.314",
        "attachment": value,
    })
}

fn assistant(second: u32, content: Value) -> Value {
    json!({
        "type": "assistant",
        "timestamp": at(second),
        "sessionId": "session",
        "version": "2.1.314",
        "entrypoint": "sdk-ts",
        "requestRef": format!("api-{second}"),
        "message": {
            "id": format!("message-{second}"),
            "model": "claude-opus-5-5",
            "content": content,
            "usage": {
                "input_tokens": 1000,
                "output_tokens": 10,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
                "service_tier": "standard",
                "speed": "standard",
                "inference_geo": "global",
            },
        },
    })
}

/// Merge `fields` into a copy of the object `base`, like a JavaScript spread.
fn with(base: Value, fields: Value) -> Value {
    let mut merged = base;
    let object = merged.as_object_mut().unwrap();
    for (key, value) in fields.as_object().unwrap() {
        object.insert(key.clone(), value.clone());
    }
    merged
}

fn jsonl(records: &[Value]) -> String {
    records.iter().map(|r| format!("{r}\n")).collect()
}

/// JSON text length in UTF-16 units, like JavaScript's `JSON.stringify(v).length`.
fn length(value: &Value) -> usize {
    serde_json::to_string(value).unwrap().encode_utf16().count()
}

fn options(w: &Workspace, since: Option<Millis>) -> ScanOptions {
    let text = |p: &std::path::Path| p.to_string_lossy().into_owned();
    ScanOptions {
        discovery: DiscoveryOptions {
            home: text(&w.home),
            env: Env::default(),
            claude_dirs: vec![],
            codex_dirs: vec![],
            t3_dirs: vec![],
            managed_settings_file: text(&w.managed_settings_file()),
            codex_system_config_file: text(&w.home.join("system.toml")),
            codex_requirements_file: text(&w.home.join("requirements.toml")),
        },
        since,
    }
}

/// Scan one synthetic main-session transcript in a fresh workspace.
fn transcript(
    harness: Harness,
    records: &[Value],
    since: Option<Millis>,
) -> Result<Scan, ScanError> {
    let w = Workspace::new();
    let file = match harness {
        Harness::Codex => w.codex.join("sessions/main.jsonl"),
        Harness::Claude => w.claude.join("projects/project/main.jsonl"),
    };
    std::fs::write(file, jsonl(records)).unwrap();
    scan(&options(&w, since))
}

fn report(data: &Scan, max_cost_per_use_usd: f64) -> Report {
    analyze(AnalyzeOptions {
        scan: data.clone(),
        catalog: &prices(),
        since: None,
        now: NOW,
        max_cost_per_use_usd,
    })
    .unwrap()
}

fn text(report: &Report) -> String {
    format_report(
        report,
        ReportOptions {
            width: None,
            color: false,
            unicode: false,
            details: false,
            host: None,
        },
    )
}

/// Whether any field of the value holds any of `needles`.
fn leaks(value: &impl std::fmt::Debug, needles: &[&str]) -> bool {
    let text = format!("{value:?}");
    needles.iter().any(|n| text.contains(n))
}

fn has(ids: &Ids, id: &str) -> bool {
    ids.iter().any(|i| &**i == id)
}

fn row<'a>(report: &'a Report, name: &str) -> &'a ContextCost {
    report.context.iter().find(|c| c.name == name).expect(name)
}

fn kinds(e: &ExposureScan, kind: InvocationKind) -> Vec<&Invocation> {
    e.invocations.iter().filter(|i| i.kind == kind).collect()
}

fn categories(e: &ExposureScan, category: Category) -> usize {
    e.components
        .iter()
        .filter(|c| c.category == category)
        .count()
}

/// Whether a report line's heading starts with `head` and its evidence holds every one of `parts`.
fn shows(text: &str, mark: &str, head: &str, parts: &[&str]) -> bool {
    let line = report_line(text, head);
    let ok = line.mark == mark && parts.iter().all(|p| line.evidence.contains(p));
    if !ok {
        eprintln!("{line:#?}");
    }
    ok
}

#[test]
fn world_state_deltas_keep_identities_removals_inheritance_and_compaction_uncertainty() {
    let data = transcript(
        Harness::Codex,
        &[
            codex_meta(),
            turn(),
            world(
                0,
                json!({
                    "host_skills": { "body": "SYNTHETIC_PRIVATE_LISTING" },
                    "tools": { "mcp__example": "SYNTHETIC_PRIVATE_DESCRIPTION" },
                    "top_level_tools": { "functions": "0123456789012345678901234567890123456789" },
                }),
                true,
            ),
            billed(1),
            world(
                2,
                json!({
                    "tools": { "mcp__example": null },
                    "host_skills": { "body": "SYNTHETIC_PRIVATE_EXPANDED_LISTING" },
                }),
                false,
            ),
            billed(3),
            json!({ "type": "compacted", "timestamp": at(4), "payload": {} }),
            billed(5),
        ],
        Some(millis(1)),
    )
    .unwrap();
    let e = &data.exposure;
    assert_eq!(e.requests.len(), 3);
    assert_eq!(e.requests[0].view.inherited.len(), 3);
    assert_eq!(e.requests[1].view.removed.len(), 2);
    assert_eq!(e.requests[1].view.candidates.len(), 2);
    assert_eq!(e.requests[2].view.coverage, TextCoverage::Unknown);
    let hash = e
        .components
        .iter()
        .find(|c| &*c.provenance == "opaque-schema-identity")
        .unwrap();
    assert_eq!(hash.size.characters, None);
    assert_eq!(hash.size.recorded_tokens, None);
    let listing = e
        .components
        .iter()
        .find(|c| c.category == Category::SkillListing)
        .unwrap();
    assert_eq!(listing.owner, Owner::Skills);
    assert!(
        e.visibility
            .iter()
            .all(|v| v.nested_calls == Visible::Unknown)
    );
    assert!(!leaks(e, &["SYNTHETIC_PRIVATE", "0123456789"]));
}

#[test]
fn claude_attachment_deltas_track_deferred_definitions_and_listing_replacement_unpriced() {
    let data = transcript(
        Harness::Claude,
        &[
            attachment(
                0,
                json!({
                    "type": "skill_listing",
                    "content": "SYNTHETIC_PRIVATE_FIRST",
                    "names": ["one"],
                    "isInitial": true,
                }),
            ),
            attachment(
                1,
                json!({
                    "type": "instructions",
                    "files": [{ "path": "/synthetic/CLAUDE.md", "content": "SYNTHETIC_PRIVATE_RULE" }],
                }),
            ),
            attachment(
                2,
                json!({
                    "type": "deferred_tools_record",
                    "entries": [{
                        "name": "mcp__example__search",
                        "description": "SYNTHETIC_PRIVATE_SCHEMA",
                        "defer_loading": true,
                        "input_schema": { "type": "object" },
                    }],
                }),
            ),
            attachment(
                3,
                json!({
                    "type": "deferred_tools_delta",
                    "addedNames": ["mcp__example__search"],
                    "addedLines": ["SYNTHETIC_PRIVATE_SHORT"],
                    "removedNames": [],
                }),
            ),
            json!({
                "type": "api-request",
                "timestamp": at(4),
                "sessionId": "session",
                "id": "api-5",
                "requestDigest": "digest-one",
                "keep": 0,
                "tail": ["message-digest"],
                "params": { "model": "claude-opus-5-5" },
            }),
            assistant(5, json!([])),
            attachment(
                6,
                json!({
                    "type": "skill_listing",
                    "content": "SYNTHETIC_PRIVATE_BACKFILL_LONGER",
                    "names": ["two", "three"],
                }),
            ),
            attachment(
                7,
                json!({
                    "type": "deferred_tools_delta",
                    "addedNames": [],
                    "addedLines": [],
                    "removedNames": ["mcp__example__search"],
                    "surfacedDefinitions": [{
                        "name": "mcp__example__search",
                        "definition": {
                            "name": "mcp__example__search",
                            "description": "SYNTHETIC_PRIVATE_EXPANDED",
                            "input_schema": { "type": "object" },
                        },
                    }],
                }),
            ),
            json!({
                "type": "api-request",
                "timestamp": at(8),
                "sessionId": "session",
                "id": "api-9",
                "base": "digest-one",
                "keep": 1,
                "tail": ["next-digest"],
                "requestDigest": "digest-two",
                "params": { "model": "claude-opus-5-5" },
            }),
            assistant(9, json!([])),
        ],
        None,
    )
    .unwrap();
    let e = &data.exposure;
    assert_eq!(e.requests[0].view.deferred.len(), 1);
    assert_eq!(e.requests[0].view.components.len(), 0);
    let references = e.requests[1].view.references.as_ref().unwrap();
    assert_eq!(references.items.as_ref().unwrap().len(), 2);
    assert_eq!(e.requests[1].view.removed.len(), 2);
    let report = report(&data, 1.0);
    assert_eq!(report.exposure.calibration.len(), 0);
    // Digest-only requests prove no exact shape, so nothing is priced per piece.
    assert_eq!(report.context.len(), 0);
    assert!(
        e.components
            .iter()
            .all(|c| c.current == crate::model::Current::Unresolved)
    );
    assert!(!leaks(&report, &["SYNTHETIC_PRIVATE"]));
}

#[test]
fn text_only_wire_requests_calibrate_matching_cohorts_and_keep_unknown_net_effects() {
    let data = transcript(
        Harness::Claude,
        &[
            json!({
                "type": "api-request",
                "id": "api-1",
                "timestamp": at(0),
                "sessionId": "session",
                "params": {
                    "model": "claude-opus-5-5",
                    "system": "SYNTHETIC_PRIVATE_SYSTEM",
                    "messages": [{ "role": "user", "content": "SYNTHETIC_PRIVATE_PROMPT" }],
                    "tools": [{
                        "name": "mcp__example__search",
                        "description": "SYNTHETIC_PRIVATE_TOOL",
                        "input_schema": { "type": "object" },
                    }],
                },
            }),
            assistant(1, json!([])),
        ],
        None,
    )
    .unwrap();
    let report = report(&data, 1.0);
    let calibration = &report.exposure.calibration[0];
    assert_eq!(calibration.samples.len(), 1);
    assert_eq!(calibration.samples[0].tokens, 1000);
    assert_eq!(measure_size(Some("🌲木abc")).characters, Some(5));
    // Full requests list tools before the system prompt, matching the cache prefix.
    let order: Vec<Category> = data.exposure.requests[0]
        .view
        .components
        .iter()
        .map(|id| {
            let component = data.exposure.components.iter().find(|c| *c.id == **id);
            component.unwrap().category
        })
        .collect();
    assert_eq!(order, [Category::McpSchema, Category::Other]);
    let server = report
        .context
        .iter()
        .find(|c| c.kind == ContextKind::McpServer)
        .unwrap();
    assert_eq!(server.name, "example");
    assert_eq!(server.priced, 1);
    assert!(server.cost_usd.to_f64() > 0.0);
    let doubled: Vec<Request> = data
        .requests
        .iter()
        .map(|r| {
            let mut r = r.clone();
            r.parts.extend(r.parts.clone());
            r
        })
        .collect();
    assert_eq!(calibrate(&data.exposure, &doubled).len(), 0);
    assert!(!leaks(&report, &["SYNTHETIC_PRIVATE"]));
}

#[test]
fn image_inputs_and_unlinked_requests_never_calibrate_from_partial_text() {
    let data = transcript(
        Harness::Claude,
        &[
            json!({
                "type": "api-request",
                "id": "api-1",
                "timestamp": at(0),
                "sessionId": "session",
                "params": {
                    "model": "claude-opus-5-5",
                    "system": "synthetic",
                    "messages": [{
                        "role": "user",
                        "content": [{
                            "type": "image",
                            "source": { "type": "base64", "data": "SYNTHETIC_PRIVATE_IMAGE" },
                        }],
                    }],
                    "tools": [{ "name": "Artifact", "description": "a", "input_schema": { "type": "object" } }],
                },
            }),
            assistant(1, json!([])),
            assistant(2, json!([])),
        ],
        None,
    )
    .unwrap();
    assert_eq!(calibrate(&data.exposure, &data.requests).len(), 0);
    assert_ne!(data.exposure.requests[1].view.linkage, Linkage::Wire);
    assert!(!leaks(&data, &["SYNTHETIC_PRIVATE"]));
    // Without a calibration nothing is priced, and nothing is shown as $0.
    let report = report(&data, 1.0);
    assert!(report.context.iter().all(|c| c.estimated_usd.is_none()));
    let text = text(&report);
    let artifact = report_line(&text, "Artifact tool");
    assert!(
        artifact.head.starts_with("Artifact tool on ["),
        "{artifact:?}"
    );
    assert!(
        artifact
            .evidence
            .starts_with("Not enough evidence: none of its requests could be priced;")
    );
    assert_eq!(
        report_line(&text, "System prompt").head,
        "System prompt and core tools costs unknown"
    );
}

#[test]
fn typed_skill_aliases_deduplicate_while_nested_calls_and_script_references_stay_distinct() {
    let data = transcript(
        Harness::Codex,
        &[
            codex_meta(),
            turn(),
            json!({
                "type": "event_msg",
                "timestamp": at(1),
                "payload": {
                    "type": "item_completed",
                    "item": {
                        "type": "UserMessage",
                        "id": "user-one",
                        "content": [
                            { "type": "skill", "name": "synthetic-skill", "path": "/synthetic/SKILL.md" },
                            { "type": "Text", "text": "Use $synthetic-skill SYNTHETIC_PRIVATE_PROMPT" },
                        ],
                    },
                },
            }),
            json!({
                "type": "response_item",
                "timestamp": at(2),
                "payload": {
                    "type": "function_call",
                    "id": "call-one",
                    "call_id": "call-one",
                    "name": "exec",
                    "arguments": "await mcp__example__nested({secret: 'SYNTHETIC_PRIVATE_ARGS'})",
                },
            }),
            json!({
                "type": "event_msg",
                "timestamp": at(3),
                "payload": {
                    "type": "item_completed",
                    "item": {
                        "type": "McpToolCall",
                        "id": "nested-one",
                        "server": "example",
                        "tool": "nested",
                        "status": "completed",
                        "result": { "content": [{ "type": "text", "text": "SYNTHETIC_PRIVATE_RESULT" }] },
                    },
                },
            }),
            billed(4),
        ],
        None,
    )
    .unwrap();
    let e = &data.exposure;
    assert_eq!(kinds(e, InvocationKind::SkillRequest).len(), 1);
    assert_eq!(kinds(e, InvocationKind::SkillRequest)[0].via, Via::Typed);
    assert_eq!(
        kinds(e, InvocationKind::SkillRequest)[0].target,
        target("skill", "/synthetic/SKILL.md")
    );
    assert_eq!(
        kinds(e, InvocationKind::McpCall)[0].status,
        InvocationStatus::Completed
    );
    assert_eq!(
        kinds(e, InvocationKind::ScriptReference)[0].status,
        InvocationStatus::Reference
    );
    assert!(
        e.visibility
            .iter()
            .all(|v| v.nested_calls == Visible::Partial && v.typed_skills == Observed::Observed)
    );
    assert!(!leaks(&data, &["SYNTHETIC_PRIVATE"]));
}

#[test]
fn skill_and_file_loads_keep_derived_bodies_and_completion_without_call_arguments() {
    let data = transcript(
        Harness::Claude,
        &[
            assistant(
                1,
                json!([
                    { "type": "tool_use", "id": "skill-call", "name": "Skill",
                      "input": { "skill": "example:skill", "args": "SYNTHETIC_PRIVATE_ARGS" } },
                    { "type": "tool_use", "id": "read-call", "name": "Read",
                      "input": { "file_path": "/synthetic/SKILL.md" } },
                ]),
            ),
            json!({
                "type": "user",
                "timestamp": at(2),
                "sessionId": "session",
                "message": { "content": [
                    { "type": "tool_result", "tool_use_id": "skill-call", "content": "SYNTHETIC_PRIVATE_BODY" },
                    { "type": "tool_result", "tool_use_id": "read-call", "content": "SYNTHETIC_PRIVATE_FILE" },
                ] },
            }),
            assistant(3, json!([])),
        ],
        None,
    )
    .unwrap();
    let e = &data.exposure;
    assert_eq!(e.invocations.len(), 2);
    assert!(
        e.invocations
            .iter()
            .all(|i| i.status == InvocationStatus::Completed)
    );
    assert_eq!(categories(e, Category::SkillBody), 2);
    assert!(!leaks(&data, &["SYNTHETIC_PRIVATE", "example:skill"]));
}

#[test]
fn copied_history_is_one_request_exposure_and_conflicting_copies_stay_unknown() {
    let w = Workspace::new();
    let mut records = vec![
        codex_meta(),
        turn(),
        world(
            0,
            json!({ "host_skills": { "body": "synthetic listing" } }),
            true,
        ),
        billed(1),
    ];
    let sessions = w.codex.join("sessions");
    std::fs::write(sessions.join("a.jsonl"), jsonl(&records)).unwrap();
    std::fs::write(sessions.join("b.jsonl"), jsonl(&records)).unwrap();
    let data = scan(&options(&w, None)).unwrap();
    assert_eq!(data.exposure.requests.len(), 1);
    assert_eq!(data.exposure.requests[0].view.candidates.len(), 1);
    records[2] = world(
        0,
        json!({ "host_skills": { "body": "different listing" } }),
        true,
    );
    std::fs::write(sessions.join("b.jsonl"), jsonl(&records)).unwrap();
    let data = scan(&options(&w, None)).unwrap();
    let view = &data.exposure.requests[0].view;
    assert_eq!(view.linkage, Linkage::Unknown);
    assert!(view.reasons.join(",").contains("disagree"));
    assert_eq!(
        *data.requests[0].id,
        format!("codex:{}", identity("response-1"))
    );
}

#[test]
fn malformed_selected_context_and_digest_chains_fail_atomically_with_safe_diagnostics() {
    let error = |harness, records: &[Value]| transcript(harness, records, None).unwrap_err();
    let world_error = error(
        Harness::Codex,
        &[
            codex_meta(),
            world(0, json!({ "tools": [] }), true),
            billed(1),
        ],
    );
    assert!(world_error.message.contains("invalid context object"));
    let listing_error = error(
        Harness::Claude,
        &[
            attachment(
                0,
                json!({ "type": "skill_listing", "content": { "private": "SYNTHETIC_PRIVATE" } }),
            ),
            assistant(1, json!([])),
        ],
    );
    assert!(listing_error.message.contains("invalid context text"));
    let digest_error = error(
        Harness::Claude,
        &[
            json!({
                "type": "api-request", "id": "one", "timestamp": at(0), "sessionId": "session",
                "requestDigest": "digest", "keep": 0, "tail": [], "params": {},
            }),
            json!({
                "type": "api-request", "id": "two", "timestamp": at(1), "sessionId": "session",
                "base": "digest", "keep": 1, "tail": [], "params": {},
            }),
        ],
    );
    assert!(
        digest_error
            .message
            .contains("invalid request digest prefix")
    );
}

#[test]
fn recorded_request_shapes_price_each_piece_at_its_cache_position_and_count_its_uses() {
    let tool = |name: &str, size: usize| json!({ "name": name, "description": "x".repeat(size), "input_schema": { "type": "object" } });
    let tools = [
        tool("Artifact", 4000),
        tool("Bash", 1000),
        tool("mcp__docs__read", 500),
    ];
    let system = json!([{ "type": "text", "text": "s".repeat(2000) }]);
    let message = json!({ "role": "user", "content": "SYNTHETIC_PRIVATE_PROMPT" });
    let call =
        |id: &str, name: &str| json!({ "type": "tool_use", "id": id, "name": name, "input": {} });
    let used = assistant(2, json!([call("t1", "mcp__docs__read")]));
    let mut reply = assistant(1, json!([call("t2", "Bash")]));
    let usage = &mut reply["message"]["usage"];
    usage["input_tokens"] = json!(100);
    usage["cache_read_input_tokens"] = json!(2000);
    usage["cache_creation_input_tokens"] = json!(900);
    usage["cache_creation"] =
        json!({ "ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 900 });
    let data = transcript(
        Harness::Claude,
        &[
            attachment(
                0,
                json!({
                    "type": "skill_listing",
                    "content": "- review: SYNTHETIC_PRIVATE_DESCRIPTION",
                    "names": ["review"],
                    "isInitial": true,
                }),
            ),
            // Seen first as a surfaced definition, then sent in the request's tools: it
            // is priced where the request puts it, not where it first appeared.
            attachment(
                0,
                json!({
                    "type": "deferred_tools_delta",
                    "addedNames": [],
                    "addedLines": [],
                    "removedNames": [],
                    "surfacedDefinitions": [{ "name": tools[2]["name"], "definition": tools[2] }],
                }),
            ),
            json!({
                "type": "api-request-shape",
                "sessionId": "session",
                "timestamp": at(0),
                "shapeHash": "shape",
                "shape": { "system": system, "tools": tools },
            }),
            json!({ "type": "api-request-blob", "sessionId": "session", "hash": "m1", "message": message }),
            json!({
                "type": "api-request",
                "id": "api-1",
                "timestamp": at(0),
                "sessionId": "session",
                "shapeHash": "shape",
                "keep": 0,
                "tail": ["m1"],
                "requestDigest": "d1",
                "params": { "model": "claude-opus-5-5" },
            }),
            reply.clone(),
            // A repeated snapshot of the same call is one use.
            reply,
            used,
        ],
        None,
    )
    .unwrap();
    let report = report(&data, 1.0);
    let listing = data
        .exposure
        .components
        .iter()
        .find(|c| c.category == Category::SkillListing && c.scope == Scope::Aggregate)
        .unwrap();
    let tools_length: usize = tools.iter().map(length).sum();
    let characters = length(&system) + tools_length + length(&message);
    let ratio = 3000.0 / characters as f64;
    assert!((report.exposure.calibration[0].tokens_per_character - ratio).abs() < 1e-12);
    let artifact = row(&report, "Artifact");
    assert_eq!(
        control_text(artifact.control.as_ref().unwrap(), Harness::Claude),
        r#""enableArtifact": false"#
    );
    assert_eq!(artifact.calls, Some(0));
    assert!(artifact.current);
    // Tools lead the prefix, so Artifact sits entirely inside the 2,000 cached-read tokens.
    let artifact_tokens = length(&tools[0]) as f64 * ratio;
    assert!(artifact_tokens < 2000.0);
    assert!((artifact.cost_usd.to_f64() - (artifact_tokens * 0.2) / 1e6).abs() < 1e-12);
    assert_eq!(row(&report, "Bash").control, None);
    assert_eq!(row(&report, "Bash").calls, Some(1));
    let docs = row(&report, "docs");
    assert_eq!(docs.calls, Some(1));
    assert_eq!(docs.kind, ContextKind::McpServer);
    assert_eq!(docs.priced, 1);
    assert_eq!(data.exposure.requests[0].view.prefix, tools.len() + 1);
    let review = row(&report, "review");
    assert_eq!(review.kind, ContextKind::Skill);
    assert_eq!(
        control_text(review.control.as_ref().unwrap(), Harness::Claude),
        r#""skillOverrides": {"review": "user-invocable-only"}"#
    );
    assert!(listing.size.characters.unwrap() > 0);
    // With no earlier request, the listing could sit anywhere after the system
    // prompt. Tools and system fill the cached reads and 1h writes, so every
    // possible offset pays the ordinary input price.
    let prefix = (length(&system) + tools_length) as f64 * ratio;
    assert!(prefix > 2900.0);
    assert_eq!(review.priced, 1);
    // Displayed tokens are rounded, so compare the per-token rate.
    assert!((review.cost_usd.to_f64() / review.tokens.unwrap() as f64 / 4e-6 - 1.0).abs() < 0.01);
    assert_eq!(row(&report, "Claude Code system prompt").control, None);
    assert!(!leaks(&report, &["SYNTHETIC_PRIVATE"]));
    // A priced piece never used is off, however little it cost; the used ones
    // cost well under the default $1 per use. An unpriced one gets no verdict.
    assert!(report.context.iter().all(|c| {
        let priced = c.control.is_some() && c.estimated_usd.is_some();
        let expected = if c.calls == Some(0) {
            Verdict::Off
        } else {
            Verdict::Keep
        };
        c.verdict == priced.then_some(expected)
    }));

    // A used piece above the limit per use is only worth considering off. Typed
    // skill requests would not count as uses.
    let strict = analyze(AnalyzeOptions {
        scan: data.clone(),
        catalog: &prices(),
        since: None,
        now: NOW,
        max_cost_per_use_usd: artifact.cost_usd.to_f64() / 2.0,
    })
    .unwrap();
    let verdict = |name: &str| row(&strict, name).verdict;
    assert_eq!(verdict("Artifact"), Some(Verdict::Off));
    assert_eq!(verdict("docs"), Some(Verdict::Consider));
    assert_eq!(verdict("review"), Some(Verdict::Off));
    assert_eq!(verdict("Bash"), None);
    let text = text(&strict);
    let line = |head: &str| report_line(&text, head);
    assert!(
        line("Artifact tool")
            .head
            .starts_with("Artifact tool on -> off costs ")
    );
    assert!(line("Artifact tool").evidence.starts_with("Never used."));
    assert!(
        line("Skill review")
            .head
            .starts_with("Skill review on -> typed only costs ")
    );
    assert!(
        line("Skill review")
            .evidence
            .starts_with("The model never used it.")
    );
    assert_eq!(line("MCP server docs").mark, "consider");
    assert!(
        line("MCP server docs")
            .evidence
            .contains(r#"Turn off: "permissions": {"deny": ["mcp__docs"]}."#)
    );
    let fixes = crate::doctor::fixes(&strict, false);
    let snippet = crate::report::format_changes(&strict, &fixes, ReportOptions::default());
    let off = &snippet[..snippet.find("worth considering").unwrap()];
    assert!(off.contains(r#""enableArtifact": false"#), "{snippet}");
    assert!(
        off.contains(r#""review": "user-invocable-only""#),
        "{snippet}"
    );
    assert!(!off.contains("mcp__docs"), "{snippet}");
}

#[test]
fn attachments_take_their_latest_cache_position_and_media_or_copied_calls_do_not_skew_counts() {
    let tools = [
        json!({ "name": "Artifact", "description": "a".repeat(2000), "input_schema": { "type": "object" } }),
        json!({ "name": "mcp__docs__read", "description": "r".repeat(200), "input_schema": { "type": "object" } }),
    ];
    let system = json!([{ "type": "text", "text": "s".repeat(1000) }]);
    // Tool input named data is billed text; only thinking signatures are opaque.
    let first = json!({
        "role": "assistant",
        "content": [
            { "type": "thinking", "thinking": "t", "signature": "z".repeat(5000) },
            {
                "type": "tool_use",
                "id": "c1",
                "name": "Artifact",
                // Tool input can hold fields named like opaque ones; they are billed text.
                "input": { "type": "thinking", "signature": "q".repeat(100), "data": "d".repeat(4000) },
            },
        ],
    });
    let image = json!({
        "role": "user",
        "content": [{ "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } }],
    });
    let usage = |input: i64, read: i64, write: i64| {
        json!({
            "input_tokens": input,
            "cache_read_input_tokens": read,
            "cache_creation_input_tokens": write,
            "cache_creation": { "ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": write },
        })
    };
    let request = |n: u32, tail: &[&str]| {
        json!({
            "type": "api-request",
            "id": format!("api-{n}"),
            "timestamp": at(n),
            "sessionId": "session",
            "shapeHash": "shape",
            "keep": 0,
            "tail": tail,
            "requestDigest": format!("d{n}"),
            "params": { "model": "claude-opus-5-5" },
        })
    };
    let reply = |n: u32, u: Value| {
        let mut a = assistant(
            n,
            json!([{ "type": "tool_use", "id": "c1", "name": "Artifact", "input": {} }]),
        );
        let fields = a["message"]["usage"].take();
        a["message"]["usage"] = with(fields, u);
        a
    };
    let instructions = |second: u32, path: &str, content: &str| {
        attachment(
            second,
            json!({ "type": "instructions", "files": [{ "path": path, "content": content }] }),
        )
    };
    let records = [
        attachment(
            0,
            json!({
                "type": "skill_listing",
                "content": format!("- review: {}", "r".repeat(300)),
                "names": ["review"],
                "isInitial": true,
            }),
        ),
        // Server instructions ride in messages while the server's tool leads the
        // prompt, so one row mixes a known and an attachment position.
        attachment(
            0,
            json!({
                "type": "mcp_instructions_delta",
                "addedNames": ["docs"],
                "addedBlocks": ["m".repeat(300)],
            }),
        ),
        json!({
            "type": "api-request-shape",
            "sessionId": "session",
            "timestamp": at(0),
            "shapeHash": "shape",
            "shape": { "system": system, "tools": tools },
        }),
        json!({ "type": "api-request-blob", "sessionId": "session", "hash": "m1", "message": first }),
        request(1, &["m1"]),
        reply(1, usage(100, 0, 2900)),
        instructions(2, "/repo/CLAUDE.md", &"i".repeat(600)),
        json!({ "type": "api-request-blob", "sessionId": "session", "hash": "m2", "message": image }),
        request(3, &["m1", "m2"]),
        reply(3, usage(0, 3000, 500)),
        // A changed file at the same path is new content, not carried content.
        instructions(4, "/repo/CLAUDE.md", &"j".repeat(600)),
        request(5, &["m1", "m2"]),
        reply(5, usage(0, 3500, 500)),
        // Fewer cached reads than the previous input: a carried attachment could
        // sit in the reads or the writes, so this request leaves it unpriced.
        request(7, &["m1", "m2"]),
        reply(7, usage(0, 1500, 2500)),
        // The tool list shrinks, so the previous input no longer says where
        // messages start: a new file could sit in the reads or the writes.
        json!({
            "type": "api-request-shape",
            "sessionId": "session",
            "timestamp": at(8),
            "shapeHash": "smaller",
            "shape": { "system": system, "tools": [tools[0]] },
        }),
        instructions(8, "/repo/sub/CLAUDE.md", &"k".repeat(600)),
        with(request(9, &["m1", "m2"]), json!({ "shapeHash": "smaller" })),
        reply(9, usage(0, 1500, 3500)),
        // A model switch changes the tokenizer, so the previous input, counted by
        // the other model, cannot bound this request's offsets either.
        instructions(10, "/repo/other/CLAUDE.md", &"o".repeat(600)),
        with(
            request(11, &["m1"]),
            json!({ "shapeHash": "smaller", "params": { "model": "claude-haiku-5-5" } }),
        ),
        {
            let mut a = reply(11, usage(0, 3000, 2500));
            a["message"]["model"] = json!("claude-haiku-5-5");
            a
        },
    ];
    let w = Workspace::new();
    for file in ["a", "b"] {
        let path = w.claude.join(format!("projects/project/{file}.jsonl"));
        std::fs::write(path, jsonl(&records)).unwrap();
    }
    // A second profile holds a copy without the call, so the request belongs to
    // both profiles while the call was recorded in one.
    let other = w.home.join(".claude-work/projects/project");
    std::fs::create_dir_all(&other).unwrap();
    let uncalled: Vec<Value> = records
        .iter()
        .map(|r| {
            let mut r = r.clone();
            if r["type"] == "assistant" {
                r["message"]["content"] = json!([]);
            }
            r
        })
        .collect();
    std::fs::write(other.join("c.jsonl"), jsonl(&uncalled)).unwrap();
    let data = scan(&options(&w, None)).unwrap();
    let report = report(&data, 1.0);
    let calibration = &report.exposure.calibration[0];
    // Only the text-only request calibrates, with its tool input counted.
    assert_eq!(calibration.samples.len(), 1);
    let mut unsigned = first.clone();
    unsigned["content"][0]
        .as_object_mut()
        .unwrap()
        .remove("signature");
    let expected = length(&system) + tools.iter().map(length).sum::<usize>() + length(&unsigned);
    assert_eq!(calibration.samples[0].characters, expected as i64);
    let ratio = calibration.tokens_per_character;
    // The same call copied into a second file counts once, and still counts for
    // a row shared with a profile whose copy lacks it.
    assert_eq!(row(&report, "Artifact").profiles.len(), 2);
    assert_eq!(row(&report, "Artifact").calls, Some(1));
    // The listing came with the first request, so the second reads it from cache.
    let review = row(&report, "review");
    let instructions = row(&report, "/repo/CLAUDE.md");
    assert_eq!(instructions.priced, 2);
    let tokens = 600.0 * ratio;
    assert!(
        tokens < 500.0,
        "the new file fits in the second request's writes"
    );
    // The file is new in the second request, and its changed revision is new
    // in the third, so both pay the 1h write price rather than a cached read.
    assert!((instructions.cost_usd.to_f64() / (tokens * (8.0 + 8.0) * 1e-6) - 1.0).abs() < 0.001);
    // Claude Code caches through the last message, so the first request's
    // listing lies in its 1h writes rather than the ordinary input after them;
    // the next two read it from cache.
    let write = instructions.cost_usd.to_f64() / (2.0 * tokens);
    assert_eq!(review.requests, 6);
    assert_eq!(review.priced, 3);
    let listing = review.tokens.unwrap() as f64;
    assert!((review.cost_usd.to_f64() / (listing * (write + 2.0 * 0.2e-6)) - 1.0).abs() < 0.01);
    assert_eq!(
        review.estimated_usd,
        Some((review.cost_usd.to_f64() * 6.0) / 3.0)
    );
    // The server row is priced where both its tool and its instructions were.
    let docs = row(&report, "docs");
    assert_eq!(docs.requests, 6);
    assert_eq!(docs.priced, 3);
    let nested = row(&report, "/repo/sub/CLAUDE.md");
    assert_eq!(nested.requests, 2);
    assert_eq!(nested.priced, 0);
    let switched = row(&report, "/repo/other/CLAUDE.md");
    assert_eq!(switched.requests, 1);
    assert_eq!(switched.priced, 0);
}

#[test]
fn provided_schemas_listing_members_and_identified_command_file_reads_are_derived_once() {
    let data = transcript(
        Harness::Codex,
        &[
            codex_meta(),
            turn(),
            world(
                0,
                json!({ "host_skills": {
                    "body": "Available skills\n- example:skill: SYNTHETIC_PRIVATE_DESCRIPTION\n\nHow to use skills\nSYNTHETIC_PRIVATE_GUIDANCE",
                } }),
                true,
            ),
            json!({
                "type": "response_item",
                "timestamp": at(1),
                "payload": {
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [{
                        "type": "function",
                        "name": "mcp__example__search",
                        "description": "SYNTHETIC_PRIVATE_SCHEMA",
                        "parameters": { "type": "object" },
                    }],
                },
            }),
            json!({
                "type": "response_item",
                "timestamp": at(2),
                "payload": {
                    "type": "function_call",
                    "namespace": "mcp__example",
                    "name": "search",
                    "call_id": "tool-call",
                    "id": "mcp-one",
                    "arguments": "{}",
                },
            }),
            json!({
                "type": "event_msg",
                "timestamp": at(3),
                "payload": {
                    "type": "item_completed",
                    "item": {
                        "type": "McpToolCall",
                        "id": "mcp-one",
                        "server": "example",
                        "tool": "search",
                        "status": "completed",
                        "result": { "content": [] },
                    },
                },
            }),
            json!({
                "type": "event_msg",
                "timestamp": at(4),
                "payload": {
                    "type": "item_completed",
                    "item": {
                        "type": "CommandExecution",
                        "id": "read-one",
                        "exit_code": 0,
                        "status": "completed",
                        "parsed_cmd": [{
                            "type": "read",
                            "name": "SKILL.md",
                            "path": "/synthetic/skill/SKILL.md",
                            "cmd": "cat /synthetic/skill/SKILL.md",
                        }],
                        "aggregated_output": "SYNTHETIC_PRIVATE_BODY",
                    },
                },
            }),
            billed(5),
        ],
        None,
    )
    .unwrap();
    let e = &data.exposure;
    let aggregate = e
        .components
        .iter()
        .find(|c| c.scope == Scope::Aggregate)
        .unwrap();
    assert_eq!(aggregate.members.len(), 1);
    let member = e
        .components
        .iter()
        .find(|c| *c.id == *aggregate.members[0])
        .unwrap();
    assert_eq!(
        member.size.characters,
        Some("- example:skill: SYNTHETIC_PRIVATE_DESCRIPTION".len() as i64)
    );
    assert!(!has(&e.requests[0].view.candidates, &member.id));
    let calls = kinds(e, InvocationKind::McpCall);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].status, InvocationStatus::Completed);
    // The call and its nested completion share `mcp-one`, so the server has one use.
    let uses: u64 = e
        .tool_calls
        .iter()
        .filter(|c| c.name.starts_with("mcp__example"))
        .map(|c| c.calls)
        .sum();
    assert_eq!(uses, 1);
    assert_eq!(
        kinds(e, InvocationKind::SkillLoad)[0].status,
        InvocationStatus::Completed
    );
    assert!(
        e.components
            .iter()
            .any(|c| &*c.provenance == "provided-additional-tools" && c.size.characters.is_some())
    );
    assert!(!leaks(e, &["SYNTHETIC_PRIVATE"]));
}

#[test]
fn invalid_pre_window_exposure_records_invalidate_context_without_aborting_selected_billing() {
    let data = transcript(
        Harness::Claude,
        &[
            attachment(
                0,
                json!({ "type": "skill_listing", "content": "old listing", "names": ["old"] }),
            ),
            json!({
                "type": "api-request",
                "timestamp": at(1),
                "sessionId": "session",
                "tail": ["digest"],
                "params": {},
            }),
            attachment(
                2,
                json!({ "type": "skill_listing", "content": { "invalid": true } }),
            ),
            json!({
                "type": "api-request",
                "id": "old",
                "timestamp": at(3),
                "sessionId": "session",
                "tail": [{ "invalid": true }],
                "params": {},
            }),
            assistant(5, json!([])),
        ],
        Some(millis(4)),
    )
    .unwrap();
    assert_eq!(data.requests.len(), 1);
    let view = &data.exposure.requests[0].view;
    assert_eq!(view.coverage, TextCoverage::Unknown);
    assert!(
        data.coverage
            .issues
            .join(",")
            .contains("invalid pre-window component")
    );
    assert!(view.reasons.join(",").contains("invalid pre-window"));
}

#[test]
fn cohort_calibration_outside_measured_request_support_leaves_that_comparison_unknown() {
    let wire = |second: u32| {
        json!({
            "type": "api-request",
            "id": format!("api-{second}"),
            "timestamp": at(second - 1),
            "sessionId": "session",
            "params": {
                "model": "claude-opus-5-5",
                "system": "SYNTHETIC_PRIVATE_".repeat(100),
                "messages": [{ "role": "user", "content": "synthetic" }],
                "tools": [],
            },
        })
    };
    let mut low = assistant(1, json!([]));
    low["message"]["usage"]["input_tokens"] = json!(10);
    let mut high = assistant(3, json!([]));
    high["message"]["usage"]["input_tokens"] = json!(10000);
    let data = transcript(Harness::Claude, &[wire(1), low, wire(3), high], None).unwrap();
    let report = report(&data, 1.0);
    // The pooled ratio puts the low request's pieces above its recorded input.
    let system = report
        .context
        .iter()
        .find(|c| c.kind == ContextKind::System)
        .unwrap();
    assert_eq!(system.requests, 2);
    assert_eq!(system.priced, 1);
    // The unpriced request is scaled from the priced one, never counted as free.
    assert_eq!(system.estimated_usd, Some(system.cost_usd.to_f64() * 2.0));
    assert!(!leaks(&report, &["SYNTHETIC_PRIVATE"]));
}

#[test]
fn deferred_definitions_keep_active_revisions_and_repeated_results_stay_in_history() {
    let result = |second: u32, call: &str| {
        json!({
            "type": "user",
            "timestamp": at(second),
            "sessionId": "session",
            "message": { "content": [{
                "type": "tool_result",
                "tool_use_id": call,
                "content": format!("SYNTHETIC_PRIVATE_{second}"),
            }] },
        })
    };
    let call = |id: &str| json!([{ "type": "tool_use", "id": id, "name": "mcp__example__search", "input": {} }]);
    let data = transcript(
        Harness::Claude,
        &[
            attachment(
                0,
                json!({
                    "type": "prompt_snapshot",
                    "systemPrompt": ["synthetic"],
                    "tools": [{
                        "name": "mcp__example__search",
                        "description": "active",
                        "schema": { "type": "object" },
                    }],
                }),
            ),
            assistant(1, call("one")),
            result(2, "one"),
            attachment(
                3,
                json!({
                    "type": "deferred_tools_record",
                    "entries": [{
                        "name": "mcp__example__search",
                        "defer_loading": true,
                        "description": "deferred",
                        "input_schema": { "type": "object" },
                    }],
                }),
            ),
            assistant(4, call("two")),
            result(5, "two"),
            assistant(6, json!([])),
        ],
        None,
    )
    .unwrap();
    let e = &data.exposure;
    let active = e
        .components
        .iter()
        .find(|c| &*c.provenance == "prompt-snapshot-tool")
        .unwrap();
    assert!(has(&e.requests[1].view.candidates, &active.id));
    assert!(!has(&e.requests[1].view.removed, &active.id));
    let results: Vec<&Component> = e
        .components
        .iter()
        .filter(|c| c.category == Category::McpResult)
        .collect();
    assert_eq!(results.len(), 2);
    let last = &e.requests[2].view;
    assert!(
        results
            .iter()
            .all(|c| has(&last.candidates, &c.id) && !has(&last.removed, &c.id))
    );
}

#[test]
fn skill_references_need_a_known_catalog_and_native_user_aliases_keep_repeated_messages_apart() {
    let content = "Use $known-skill, inspect $HOME, costs $20";
    let response = json!({
        "type": "response_item",
        "timestamp": at(1),
        "payload": {
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": content }],
        },
    });
    let event = json!({
        "type": "event_msg",
        "timestamp": at(1),
        "payload": { "type": "user_message", "message": content },
    });
    let data = transcript(
        Harness::Codex,
        &[
            codex_meta(),
            turn(),
            world(
                0,
                json!({ "host_skills": { "body": "- known-skill: synthetic description" } }),
                true,
            ),
            response.clone(),
            event.clone(),
            response,
            event,
            billed(2),
        ],
        None,
    )
    .unwrap();
    let requests = kinds(&data.exposure, InvocationKind::SkillRequest);
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|i| i.identity_basis == IdentityBasis::ContentOccurrence)
    );
    assert_eq!(
        kinds(&data.exposure, InvocationKind::SkillReference).len(),
        4
    );
    let claude = transcript(
        Harness::Claude,
        &[
            attachment(
                0,
                json!({ "type": "skill_listing", "names": ["known-skill"], "content": "synthetic" }),
            ),
            json!({
                "type": "user",
                "timestamp": at(1),
                "sessionId": "session",
                "isMeta": true,
                "message": { "content": "$known-skill $ARGUMENTS" },
            }),
            json!({
                "type": "user",
                "timestamp": at(2),
                "sessionId": "session",
                "message": { "content": "<command-name>/compact</command-name>" },
            }),
            assistant(3, json!([])),
        ],
        None,
    )
    .unwrap();
    assert_eq!(
        kinds(&claude.exposure, InvocationKind::SkillRequest).len(),
        0
    );
}

#[test]
fn copied_mcp_calls_deduplicate_across_inherited_session_aliases() {
    let w = Workspace::new();
    let records = [
        codex_meta(),
        turn(),
        json!({
            "type": "event_msg",
            "timestamp": at(1),
            "payload": {
                "type": "item_completed",
                "item": {
                    "type": "McpToolCall",
                    "id": "shared-call",
                    "server": "example",
                    "tool": "search",
                    "status": "completed",
                    "result": { "content": [] },
                },
            },
        }),
        billed(2),
    ];
    let sessions = w.codex.join("sessions");
    std::fs::write(sessions.join("a.jsonl"), jsonl(&records)).unwrap();
    let mut fork = records.clone();
    fork[0]["payload"]["id"] = json!("fork");
    std::fs::write(sessions.join("b.jsonl"), jsonl(&fork)).unwrap();
    let data = scan(&options(&w, None)).unwrap();
    assert_eq!(data.exposure.invocations.len(), 1);
    assert_eq!(data.exposure.invocations[0].sessions.len(), 2);
}

#[test]
fn current_record_shapes_request_blobs_rendered_attachments_same_as_definitions_null_bodies() {
    let call =
        json!({ "type": "tool_use", "id": "tool-1", "name": "mcp__example__search", "input": {} });
    let definition = json!({
        "name": "mcp__example__search",
        "definition": { "name": "mcp__example__search", "input_schema": {} },
    });
    let surfaced = |definitions: Value| {
        json!({
            "type": "deferred_tools_delta",
            "addedNames": [],
            "addedLines": [],
            "removedNames": [],
            "surfacedDefinitions": definitions,
        })
    };
    let same_as = |reference: &str| json!([{ "name": definition["name"], "sameAs": reference, "listing": "" }]);
    let data = transcript(
        Harness::Claude,
        &[
            attachment(0, surfaced(json!([definition]))),
            attachment(1, surfaced(same_as("attachment-0"))),
            with(
                attachment(2, json!({ "type": "queued_command", "prompt": [] })),
                json!({ "rendered": [{ "content": [{ "type": "text", "text": "SYNTHETIC_PRIVATE_QUEUED" }] }] }),
            ),
            // Hash-addressed copy of a turn: no timestamp, never a second invocation.
            json!({
                "type": "api-request-blob",
                "hash": "blob",
                "sessionId": "session",
                "message": { "role": "assistant", "content": [call] },
            }),
            assistant(3, json!([call])),
        ],
        None,
    )
    .unwrap();
    let e = &data.exposure;
    assert_eq!(kinds(e, InvocationKind::McpCall).len(), 1);
    assert_eq!(categories(e, Category::McpSchema), 1);
    assert!(
        e.components
            .iter()
            .any(|c| &*c.provenance == "unclassified-rendered-attachment")
    );
    let error = transcript(
        Harness::Claude,
        &[attachment(0, surfaced(same_as("attachment-9")))],
        None,
    )
    .unwrap_err();
    assert!(
        error
            .message
            .contains("unresolved surfaced definition reference")
    );
    let codex = transcript(
        Harness::Codex,
        &[
            codex_meta(),
            turn(),
            world(
                1,
                json!({ "host_skills": { "body": null, "version": 1 } }),
                false,
            ),
            billed(2),
        ],
        None,
    )
    .unwrap();
    assert!(
        codex
            .exposure
            .components
            .iter()
            .any(|c| &*c.provenance == "world-state-metadata-only")
    );
}

fn skill_load(second: u32, id: &str, path: &str, exit: i32) -> Value {
    json!({"type": "event_msg", "timestamp": at(second), "payload": {
        "type": "item_completed", "item": {"type": "CommandExecution", "id": id,
        "exit_code": exit, "parsed_cmd": [{"type": "read", "path": path}],
        "aggregated_output": "SYNTHETIC_PRIVATE_SKILL_BODY"}
    }})
}

fn skill_row<'a>(report: &'a Report, path: &str) -> &'a ContextCost {
    report
        .context
        .iter()
        .find(|c| {
            c.skill
                .as_ref()
                .is_some_and(|s| s.path.as_deref() == Some(path))
        })
        .unwrap()
}

#[test]
fn codex_catalog_allocation_preserves_aggregate_charges_and_priceability() {
    let body = "# Skills\n- `r0` = `/remote/skills`\n- one: SYNTHETIC_PRIVATE_DESCRIPTION (file: r0/one/SKILL.md)\n- plugin:two: x (file: r0/two/SKILL.md)\nUse skills when relevant.\n";
    let usage = |second, cached: i64| {
        let mut record = billed(second);
        record["payload"]["usage"]["cached_input_tokens"] = json!(cached);
        record
    };
    let mut data = transcript(Harness::Codex, &[
        codex_meta(), turn(), world(0, json!({"host_skills": {"body": body}}), true),
        // Mixed cache regions leave the whole first catalog unpriced.
        billed(1), usage(2, 1000),
        world(3, json!({"host_skills": {"body": body.replace("SYNTHETIC_PRIVATE_DESCRIPTION", "expanded description with unicode λ")}}), false),
        usage(4, 0), usage(5, 1000),
    ], None).unwrap();
    // Keep this allocation case independent of conditional tier pricing.
    for r in &mut data.requests {
        r.parts[0].tier = Some("standard".into());
    }
    let split = report(&data, 0.0);
    let mut aggregate_data = data.clone();
    for c in &mut aggregate_data.exposure.components {
        c.members.clear();
    }
    let aggregate = report(&aggregate_data, 0.0);
    let before = row(&aggregate, "Skill list");
    let parts: Vec<_> = split
        .context
        .iter()
        .filter(|c| c.skill.is_some() || c.name == "Skill list framing")
        .collect();
    assert_eq!(parts.len(), 3);
    assert!(
        before.priced > 0 && before.priced < before.requests,
        "{before:?}"
    );
    assert!(
        parts
            .iter()
            .all(|c| c.priced == before.priced && c.requests == before.requests)
    );
    assert!(
        (parts.iter().map(|c| c.cost_usd.to_f64()).sum::<f64>() - before.cost_usd.to_f64()).abs()
            < 1e-12
    );
    assert!(
        (parts.iter().map(|c| c.estimated_usd.unwrap()).sum::<f64>()
            - before.estimated_usd.unwrap())
        .abs()
            < 1e-12
    );
    let control = skill_row(&split, "/remote/skills/two/SKILL.md")
        .control
        .as_ref()
        .unwrap();
    let patch = crate::doctor::apply_toml("", &[control.into(), control.into()]).unwrap();
    let parsed: toml::Value = toml::from_str(&patch).unwrap();
    assert_eq!(parsed["skills"]["config"].as_array().unwrap().len(), 1);
    assert_eq!(
        parsed["skills"]["config"][0]["path"].as_str(),
        Some("/remote/skills/two/SKILL.md")
    );
    assert_eq!(
        parsed["skills"]["config"][0]["enabled"].as_bool(),
        Some(false)
    );
    let inline = control_text(control, Harness::Codex);
    assert!(!inline.contains('\n'));
    let inline: toml::Value = toml::from_str(&inline).unwrap();
    assert_eq!(
        inline["skills"]["config"][0]["enabled"].as_bool(),
        Some(false)
    );
    assert!(!leaks(&split, &["SYNTHETIC_PRIVATE"]));
}

#[test]
fn codex_catalog_conditional_prices_preserve_evidence_and_duplicate_controls() {
    let body = "- same: a (file: /skills/a/SKILL.md)\n- same: b (file: /skills/b/SKILL.md)\n- unique: c (file: /skills/c/SKILL.md)";
    let mut usage = billed(3);
    // Uniform cache geometry makes every catalog share priceable.
    usage["payload"]["usage"]["cached_input_tokens"] = json!(1000);
    let data = transcript(Harness::Codex, &[
        codex_meta(), turn(), world(0, json!({"host_skills":{"body":body}}), true),
        json!({"type":"response_item", "timestamp":at(1), "payload":{"type":"message", "role":"user",
            "content":[{"type":"skill", "name":"same", "path":"/skills/a/SKILL.md"}]}}),
        skill_load(2, "load", "/skills/a/SKILL.md", 0), usage,
    ], None).unwrap();
    let split = report(&data, 0.0);
    let preferred = skill_row(&split, "/skills/a/SKILL.md");
    assert_eq!(preferred.calls, Some(1));
    let evidence = preferred.skill.as_ref().unwrap();
    assert_eq!((evidence.requested, evidence.loaded), (1, 1));
    assert_eq!(preferred.verdict, None);
    assert_eq!(
        skill_row(&split, "/skills/b/SKILL.md").verdict,
        Some(Verdict::Off)
    );
    assert_eq!(skill_row(&split, "/skills/c/SKILL.md").verdict, None);

    let mut aggregate_data = data.clone();
    for c in &mut aggregate_data.exposure.components {
        c.members.clear();
    }
    let aggregate = report(&aggregate_data, 0.0);
    let before = row(&aggregate, "Skill list");
    assert_eq!(
        (
            before.priced,
            before.conditional_requests,
            before.tier_unknown_requests
        ),
        (1, 1, 1)
    );
    assert!(!before.assumptions.is_empty());
    let parts: Vec<_> = split
        .context
        .iter()
        .filter(|c| c.skill.is_some() || c.name == "Skill list framing")
        .collect();
    assert_eq!(parts.len(), 4);
    for c in &parts {
        assert_eq!(
            (c.priced, c.conditional_requests, c.tier_unknown_requests),
            (1, 1, 1)
        );
        assert_eq!(c.assumptions, before.assumptions);
    }
    assert!(
        (parts.iter().map(|c| c.estimated_usd.unwrap()).sum::<f64>()
            - before.estimated_usd.unwrap())
        .abs()
            < 1e-12
    );

    let output = format_report(
        &split,
        ReportOptions {
            width: None,
            ..Default::default()
        },
    );
    let line = report_line(&output, "Skill same (/skills/a/SKILL.md)");
    assert!(line.head.contains(" costs <$0.01"), "{output}");
    let unresolved = line.evidence;
    assert!(unresolved.contains("tier sensitivity could reverse keep or off"));
    assert!(
        unresolved
            .contains("1 requested; 1 confirmed SKILL.md loads; successful invocation unknown")
    );
    assert!(
        unresolved.contains("[[skills.config]], path = \"/skills/a/SKILL.md\", enabled = false")
    );
    assert!(output.contains("Duplicate skill same"));
}

#[test]
fn codex_skills_and_mcp_details_keep_independent_evidence_controls_and_prices() {
    let mut usage = billed(4);
    usage["payload"]["usage"]["cached_input_tokens"] = json!(1000);
    let mut data = transcript(Harness::Codex, &[
        codex_meta(), turn(),
        // MCP rows precede skills, so nesting their details changes the flat row indices.
        world(0, json!({"tools":{"mcp__docs":"SYNTHETIC_PRIVATE_DISCOVERY"}}), true),
        json!({"type":"response_item", "timestamp":at(0), "payload":{
            "type":"additional_tools", "role":"developer", "tools":[{
                "type":"function", "name":"mcp__docs__read", "description":"SYNTHETIC_PRIVATE_SCHEMA",
                "parameters":{"type":"object"}
            }]
        }}),
        world(1, json!({
            "host_skills":{"body":"- same: a (file: /skills/a/SKILL.md)\n- same: b (file: /skills/b/SKILL.md)"},
            "agents_md":{"text":"SYNTHETIC_PRIVATE_INSTRUCTIONS\nλ"}
        }), false),
        json!({"type":"response_item", "timestamp":at(2), "payload":{"type":"message", "role":"user",
            "content":[{"type":"skill", "name":"same", "path":"/skills/b/SKILL.md"}]}}),
        skill_load(3, "load", "/skills/b/SKILL.md", 0),
        json!({"type":"event_msg", "timestamp":at(3), "payload":{
            "type":"item_completed", "item":{"type":"McpToolCall", "id":"read",
                "server":"docs", "tool":"read", "status":"completed", "result":{"content":[]}}
        }}),
        usage,
    ], None).unwrap();
    for profile in &mut data.profiles {
        if profile.harness == Harness::Codex {
            profile.mcp_servers = vec!["docs".into()];
        }
    }
    for conditional in [true, false] {
        if !conditional {
            data.requests[0].parts[0].tier = Some("standard".into());
        }
        let report = report(&data, 1.0);
        let retained = skill_row(&report, "/skills/b/SKILL.md");
        let redundant = skill_row(&report, "/skills/a/SKILL.md");
        let evidence = retained.skill.as_ref().unwrap();
        assert_eq!(
            (retained.calls, evidence.requested, evidence.loaded),
            (Some(1), 1, 1)
        );
        assert_eq!(
            evidence.retained_path.as_deref(),
            Some("/skills/b/SKILL.md")
        );
        assert_eq!(redundant.verdict, Some(Verdict::Off));
        assert_eq!(retained.verdict, (!conditional).then_some(Verdict::Keep));
        let server = row(&report, "docs");
        assert_eq!(server.calls, Some(1));
        assert_eq!(server.details.len(), 2);
        let tool = server
            .details
            .iter()
            .find(|c| c.kind == ContextKind::McpTool)
            .unwrap();
        assert_eq!(tool.calls, Some(1));
        assert!(
            (server.cost_usd.to_f64()
                - server
                    .details
                    .iter()
                    .map(|c| c.cost_usd.to_f64())
                    .sum::<f64>())
            .abs()
                < 1e-12
        );
        for c in [retained, redundant, server, tool] {
            assert_eq!(
                (c.priced, c.conditional_requests, c.tier_unknown_requests),
                (1, u64::from(conditional), u64::from(conditional))
            );
            assert!(c.estimated_usd.is_some());
            assert_eq!(c.assumptions, retained.assumptions);
            if conditional {
                assert!(!c.assumptions.is_empty());
            }
        }
        let patch = crate::doctor::apply_toml(
            "",
            &[
                retained.control.as_ref().unwrap().into(),
                tool.control.as_ref().unwrap().into(),
            ],
        )
        .unwrap();
        let parsed: toml::Value = toml::from_str(&patch).unwrap();
        assert_eq!(
            parsed["skills"]["config"][0]["path"].as_str(),
            Some("/skills/b/SKILL.md")
        );
        assert_eq!(
            parsed["mcp_servers"]["docs"]["disabled_tools"][0].as_str(),
            Some("read")
        );
        assert_eq!(row(&report, "AGENTS.md").instruction_size.unwrap().lines, 2);
        let output = format_report(
            &report,
            ReportOptions {
                width: None,
                ..Default::default()
            },
        );
        assert!(output.contains("1 requested; 1 confirmed SKILL.md loads"));
        assert!(output.contains("Codex MCP loading:"));
        assert!(
            report_line(&output, "AGENTS.md")
                .evidence
                .starts_with("Under the 32,768 bytes Codex loads")
        );
        assert!(output.contains("Duplicate skill same"));
        assert!(!leaks(&report, &["SYNTHETIC_PRIVATE"]));
    }
}

#[test]
fn codex_path_usage_deduplicates_copies_and_separates_requests_loads_and_success() {
    let w = Workspace::new();
    let body = "- `r0` = `/skills`\n- same: a (file: r0/a/SKILL.md)\n- same: b (file: r0/b/SKILL.md)\n- plugin:other: c (file: r0/c/SKILL.md)";
    let message = json!({"type": "response_item", "timestamp": at(1), "payload": {
    "type": "message", "role": "user", "content": [
        {"type": "input_text", "text": "$same"},
        {"type": "skill", "name": "same", "path": "/skills/b/SKILL.md"}
    ]}});
    let alias = json!({"type": "event_msg", "timestamp": at(1), "payload": {"type": "user_message", "message": "$same"}});
    let mut compound = skill_load(4, "compound", "/skills/a/SKILL.md", 0);
    compound["payload"]["item"]["parsed_cmd"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "unknown", "cmd": "true"}));
    let records = vec![
        codex_meta(),
        turn(),
        world(0, json!({"host_skills": {"body": body}}), true),
        message,
        alias,
        skill_load(2, "load-b", "/skills/b/SKILL.md", 0),
        skill_load(3, "failed-a", "/skills/a/SKILL.md", 1),
        compound,
        json!({"type":"event_msg", "timestamp":at(5), "payload":{"type":"user_message", "message":"$same"}}),
        billed(6),
        billed(7),
    ];
    for file in ["a.jsonl", "b.jsonl"] {
        std::fs::write(w.codex.join("sessions").join(file), jsonl(&records)).unwrap();
    }
    let data = scan(&options(&w, None)).unwrap();
    let r = report(&data, 1.0);
    let a = skill_row(&r, "/skills/a/SKILL.md");
    let b = skill_row(&r, "/skills/b/SKILL.md");
    assert_eq!(a.calls, Some(0));
    assert_eq!(b.calls, Some(1));
    let evidence = b.skill.as_ref().unwrap();
    assert_eq!(
        (
            evidence.requested,
            evidence.loaded,
            evidence.ambiguous_requests
        ),
        (1, 1, 1)
    );
    assert_eq!(evidence.successful_invocations, None);
    assert_eq!(a.skill.as_ref().unwrap().ambiguous_requests, 1);
    assert_eq!(evidence.duplicate_entries, 2);
    assert_eq!(
        evidence.retained_path.as_deref(),
        Some("/skills/b/SKILL.md")
    );
    assert_eq!((a.verdict, b.verdict), (Some(Verdict::Off), None));
    let mut priced = data.clone();
    for request in &mut priced.requests {
        request.parts[0].tier = Some("standard".into());
        request.parts[0].usage.read = Some(1000);
    }
    let cost = skill_row(&report(&priced, 1.0), "/skills/b/SKILL.md")
        .estimated_usd
        .unwrap();
    let threshold = report(&priced, cost * 0.75);
    assert_eq!(
        skill_row(&threshold, "/skills/b/SKILL.md").verdict,
        Some(Verdict::Consider)
    );
    assert_eq!(
        skill_row(&threshold, "/skills/b/SKILL.md").cost_per_use_usd,
        Some(cost)
    );
    let output = text(&r);
    assert!(output.contains("Duplicate skill same"), "{output}");
    assert!(output.contains("successful invocation unknown"));
    assert!(output.contains("not promised savings"));
    assert!(output.contains("Visibility is incomplete"));
    assert!(!leaks(&r, &["SYNTHETIC_PRIVATE"]));
    assert!(
        r.context
            .iter()
            .any(|c| c.skill.as_ref().is_some_and(|s| s.loaded == 1))
    );
}

#[test]
fn codex_duplicates_are_actionable_without_prices_and_same_path_repeats_keep_one_control() {
    let body = "- same: a (file: /skills/a/SKILL.md)\n- same: b (file: /skills/b/SKILL.md)\n- repeated: c (file: /skills/c/SKILL.md)\n- repeated: c (file: /skills/c/SKILL.md)\n- unresolved: x (file: r9/x/SKILL.md)\n- unresolved: y (file: r9/y/SKILL.md)";
    let data = transcript(
        Harness::Codex,
        &[
            codex_meta(),
            turn(),
            world(0, json!({"host_skills":{"body":body}}), true),
            json!({"type":"event_msg", "timestamp":at(1), "payload":{"type":"user_message", "message":"$unresolved"}}),
            billed(2),
        ],
        None,
    )
    .unwrap();
    let r = report(&data, 0.0);
    assert_eq!(skill_row(&r, "/skills/a/SKILL.md").verdict, None);
    let off = skill_row(&r, "/skills/b/SKILL.md");
    assert_eq!((off.estimated_usd, off.verdict), (None, Some(Verdict::Off)));
    assert_eq!(skill_row(&r, "/skills/c/SKILL.md").verdict, None);
    assert_eq!(
        skill_row(&r, "/skills/c/SKILL.md")
            .skill
            .as_ref()
            .unwrap()
            .duplicate_entries,
        2
    );
    assert!(row(&r, "unresolved").control.is_none());
    assert_eq!(row(&r, "unresolved").calls, Some(0));
    assert_eq!(
        row(&r, "unresolved")
            .skill
            .as_ref()
            .unwrap()
            .ambiguous_requests,
        1
    );
    let output = text(&r);
    assert!(
        report_line(&output, "Skill same (/skills/b/SKILL.md)")
            .head
            .starts_with("Skill same (/skills/b/SKILL.md) on -> off costs unknown"),
        "{output}"
    );
    assert!(output.contains("does not resolve an absolute SKILL.md path"));
    assert!(!output.contains("Costs $0; 0 use observations"));
    assert!(!output.contains("Skill unresolved: costs"));
}

#[test]
fn codex_catalog_replacement_resolves_requests_chronologically_and_retires_old_duplicates() {
    let body = "- `r0` = `/old`\n- same: a (file: r0/a/SKILL.md)\n- same: b (file: r0/b/SKILL.md)";
    let replacement = "- `r0` = `/new`\n- same: a (file: r0/a/SKILL.md)";
    let mut warm = billed(5);
    warm["payload"]["usage"]["cached_input_tokens"] = json!(1000);
    let mut data = transcript(Harness::Codex, &[
        codex_meta(), turn(), world(0, json!({"host_skills":{"body":body}}), true), billed(1),
        world(2, json!({"host_skills":{"body":replacement}}), false),
        json!({"type":"event_msg", "timestamp":at(3), "payload":{"type":"user_message", "message":"$same"}}),
        billed(4), warm,
    ], None).unwrap();
    // Keep this allocation case independent of conditional tier pricing.
    for r in &mut data.requests {
        r.parts[0].tier = Some("standard".into());
    }
    let r = report(&data, 0.0);
    assert_eq!(skill_row(&r, "/new/a/SKILL.md").calls, Some(1));
    assert_eq!(
        skill_row(&r, "/new/a/SKILL.md").verdict,
        Some(Verdict::Consider)
    );
    for path in ["/old/a/SKILL.md", "/old/b/SKILL.md"] {
        let c = skill_row(&r, path);
        assert_eq!((c.current, c.calls, c.verdict), (false, Some(0), None));
    }
    assert!(!text(&r).contains("Duplicate skill"));
    let keep = report(&data, 1.0);
    assert_eq!(
        skill_row(&keep, "/new/a/SKILL.md").verdict,
        Some(Verdict::Keep)
    );
}

#[test]
fn unresolved_history_and_case_aliases_keep_one_request_in_its_own_catalog() {
    let mut next_turn = turn();
    next_turn["payload"]["turn_id"] = json!("next-user-turn");
    let data = transcript(Harness::Codex, &[
        codex_meta(),
        world(0, json!({"host_skills":{"body":"- pdf: no path recorded"}}), true),
        json!({"type":"response_item", "timestamp":at(1), "payload":{"type":"message", "role":"user", "content":[
            {"type":"input_text", "text":"$PDF"}, {"type":"skill", "name":"pdf"}
        ]}}),
        billed(2),
        world(3, json!({"host_skills":{"body":"- pdf: now resolved (file: /a/pdf/SKILL.md)"}}), false),
        json!({"type":"event_msg", "timestamp":at(4), "payload":{"type":"user_message", "message":"$pdf"}}),
        turn(),
        skill_load(5, "read", "/a/pdf/SKILL.md", 0),
        // A fresh user turn establishes a separate implicit use, even with a repeated context record.
        json!({"type":"event_msg", "timestamp":at(6), "payload":{"type":"user_message", "message":"continue"}}),
        next_turn,
        skill_load(7, "again", "/a/pdf/SKILL.md", 0), billed(8),
    ], None).unwrap();
    let r = report(&data, 1.0);
    let old = r
        .context
        .iter()
        .find(|c| c.name == "pdf" && c.skill.as_ref().is_some_and(|s| s.path.is_none()))
        .unwrap();
    assert_eq!(old.calls, Some(1));
    assert_eq!(old.skill.as_ref().unwrap().ambiguous_requests, 0);
    let new = skill_row(&r, "/a/pdf/SKILL.md");
    assert_eq!(new.calls, Some(2));
    let s = new.skill.as_ref().unwrap();
    assert_eq!((s.requested, s.loaded, s.ambiguous_requests), (1, 2, 0));
}

#[test]
fn windows_aliases_match_loads_and_detail_controls_remain_single_line() {
    let data = transcript(Harness::Codex, &[
        codex_meta(), turn(),
        world(0, json!({"host_skills":{"body":"- `r0` = `C:\\skills\\`\n- pdf: x (file: r0/pdf/SKILL.md)"}}), true),
        skill_load(1, "read", "c:/skills/pdf/SKILL.md", 0), billed(2),
    ], None).unwrap();
    let r = report(&data, 1.0);
    let c = skill_row(&r, r"C:\skills\pdf\SKILL.md");
    assert_eq!(c.calls, Some(1));
    assert_eq!(c.skill.as_ref().unwrap().loaded, 1);
    assert!(data.exposure.invocations.iter().all(|i| i.group.is_none()));
    let rendered = format_report(
        &r,
        ReportOptions {
            width: None,
            details: true,
            ..Default::default()
        },
    );
    assert!(!rendered.contains("enabled = false in"));
    assert!(rendered.contains("Turn off: skills.config = ["));
    let control = control_text(c.control.as_ref().unwrap(), Harness::Codex);
    assert!(!control.contains('\n'));
    assert_eq!(
        toml::from_str::<toml::Value>(&control).unwrap()["skills"]["config"][0]["path"].as_str(),
        Some(r"C:\skills\pdf\SKILL.md")
    );
}

#[test]
fn injected_codex_user_role_messages_do_not_create_turns_or_skill_requests() {
    let injected = |second, content| {
        json!({"type":"response_item", "timestamp":at(second),
        "payload":{"type":"message", "role":"user", "content": content}})
    };
    let mut next = turn();
    next["payload"]["turn_id"] = json!("next");
    let data = transcript(Harness::Codex, &[
        codex_meta(), turn(),
        world(0, json!({"host_skills":{"body":"- foo: x (file: /skills/foo/SKILL.md)"}}), true),
        json!({"type":"event_msg", "timestamp":at(1), "payload":{"type":"user_message", "message":"$foo"}}),
        // These are model-visible injections, not new user requests.
        injected(2, json!([{"type":"input_text", "text":"<skill>body mentions $foo</skill>"}])),
        injected(3, json!([{"type":"input_image", "image_url":"synthetic"}])),
        injected(4, json!([{"type":"input_text", "text":"<user_shell_command>$foo</user_shell_command>"}])),
        skill_load(5, "load", "/skills/foo/SKILL.md", 0),
        next, billed(6),
    ], None).unwrap();
    let r = report(&data, 1.0);
    let c = skill_row(&r, "/skills/foo/SKILL.md");
    assert_eq!(c.calls, Some(1));
    let skill = c.skill.as_ref().unwrap();
    assert_eq!((skill.requested, skill.loaded), (1, 1));
    let evidence: Vec<_> = data
        .exposure
        .invocations
        .iter()
        .filter(|i| {
            matches!(
                i.kind,
                InvocationKind::SkillRequest | InvocationKind::SkillLoad
            )
        })
        .collect();
    assert_eq!(evidence.len(), 2);
    assert_eq!(evidence[0].turn, evidence[1].turn);
    assert_ne!(evidence[0].turn, Some(identity("next")));
}

#[test]
fn name_only_requests_without_an_active_catalog_use_only_unique_historical_paths() {
    for copies in [1, 2] {
        let mut body = "- foo: x (file: /skills/foo/SKILL.md)".to_string();
        if copies == 2 {
            body += "\n- foo: y (file: /other/foo/SKILL.md)";
        }
        let data = transcript(Harness::Codex, &[
            codex_meta(), turn(), world(0, json!({"host_skills":{"body":body}}), true), billed(1),
            json!({"type":"compacted", "timestamp":at(2), "payload":{}}),
            json!({"type":"response_item", "timestamp":at(3), "payload":{"type":"message", "role":"user",
                "content":[{"type":"input_text", "text":"$foo"}, {"type":"skill", "name":"foo"}]}}), billed(4),
        ], None).unwrap();
        let r = report(&data, 1.0);
        let c = skill_row(&r, "/skills/foo/SKILL.md");
        let skill = c.skill.as_ref().unwrap();
        if copies == 1 {
            assert_eq!(c.calls, Some(1));
            assert_eq!((skill.requested, skill.ambiguous_requests), (1, 0));
        } else {
            assert_eq!(c.calls, Some(0));
            assert_eq!((skill.requested, skill.ambiguous_requests), (0, 1));
        }
    }
}

#[test]
fn explicit_paths_do_not_lose_requests_to_an_unresolved_catalog_alias() {
    let data = transcript(Harness::Codex, &[
        codex_meta(), turn(),
        world(0, json!({"host_skills":{"body":"- foo: x (file: r9/foo/SKILL.md)"}}), true),
        json!({"type":"response_item", "timestamp":at(1), "payload":{"type":"message", "role":"user",
            "content":[{"type":"input_text", "text":"$foo"}, {"type":"skill", "name":"foo", "path":"/skills/foo/SKILL.md"}]}}),
        billed(2),
    ], None).unwrap();
    let r = report(&data, 1.0);
    let c = r.context.iter().find(|c| c.name == "foo").unwrap();
    assert_eq!(c.calls, Some(1));
    assert_eq!(c.skill.as_ref().unwrap().requested, 1);
    assert_eq!(c.skill.as_ref().unwrap().path, None);
    assert!(c.control.is_none());
}

#[test]
fn noncanonical_paths_do_not_produce_duplicate_disable_controls() {
    let body = "- foo: x (file: /a/foo/SKILL.md)\n- foo: x (file: /a/x/../foo/SKILL.md)\n- dot: x (file: /a/./dot/SKILL.md)\n- empty: x (file: /a//empty/SKILL.md)";
    let data = transcript(
        Harness::Codex,
        &[
            codex_meta(),
            turn(),
            world(0, json!({"host_skills":{"body":body}}), true),
            billed(1),
        ],
        None,
    )
    .unwrap();
    let r = report(&data, 1.0);
    for c in r.context.iter().filter(|c| c.skill.is_some()) {
        assert_ne!(c.verdict, Some(Verdict::Off));
        if c.skill.as_ref().unwrap().path.is_none() {
            assert!(c.control.is_none());
        }
    }
}

#[test]
fn resolved_paths_with_colons_preserve_the_skill_plugin_group() {
    let data = transcript(Harness::Codex, &[
        codex_meta(), turn(),
        world(0, json!({"host_skills":{"body":"- tools:foo: x (file: C:\\skills\\foo\\SKILL.md)\n- other: x (file: /home/a:b/other/SKILL.md)"}}), true),
        json!({"type":"event_msg", "timestamp":at(1), "payload":{"type":"user_message", "message":"$tools:foo $other"}}),
        billed(2),
    ], None).unwrap();
    let requests = kinds(&data.exposure, InvocationKind::SkillRequest);
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests
            .iter()
            .filter(|i| i.group == Some(identity("plugin:tools")))
            .count(),
        1
    );
    assert_eq!(requests.iter().filter(|i| i.group.is_none()).count(), 1);
}

#[test]
fn a_one_letter_plugin_name_is_not_a_windows_drive() {
    let data = transcript(Harness::Codex, &[
        codex_meta(), turn(),
        world(0, json!({"host_skills":{"body":"- a:foo: no path recorded"}}), true),
        json!({"type":"event_msg", "timestamp":at(1), "payload":{"type":"user_message", "message":"$a:foo"}}),
        billed(2),
    ], None).unwrap();
    let r = report(&data, 1.0);
    let c = r.context.iter().find(|c| c.name == "a:foo").unwrap();
    assert_eq!(c.calls, Some(1));
    assert_eq!(c.skill.as_ref().unwrap().requested, 1);
    assert_eq!(
        data.exposure.invocations[0].group,
        Some(identity("plugin:a"))
    );
}

/// One Claude session, with auto memory's system prompt section and MEMORY.md when
/// `memory` is on, that reads one memory file and writes another.
fn claude_memory_session(root: &str, memory: bool) -> Vec<Value> {
    let mut prompt = "SYNTHETIC_PRIVATE_INTRO\n".to_string();
    if memory {
        prompt += "# Memory\n\nSYNTHETIC_PRIVATE_MEMORY_GUIDE \"quoted\"\n\n";
    }
    prompt += "# Environment\nSYNTHETIC_PRIVATE_ENV\n";
    let system = json!([{ "type": "text", "text": prompt }]);
    let tools =
        [json!({ "name": "Read", "description": "r", "input_schema": { "type": "object" } })];
    // Without memory, the same calls touch ordinary files.
    let dir = if memory {
        format!("{root}/projects/project/memory")
    } else {
        "/repo/notes".into()
    };
    let mut files = vec![
        json!({ "path": "/repo/CLAUDE.md", "type": "Project", "content": "SYNTHETIC_PRIVATE_RULES" }),
    ];
    if memory {
        files.push(json!({ "path": format!("{dir}/MEMORY.md"), "type": "AutoMem", "content": "SYNTHETIC_PRIVATE_INDEX" }));
    }
    let request = |n: u32| {
        json!({
            "type": "api-request",
            "id": format!("api-{n}"),
            "timestamp": at(n),
            "sessionId": "session",
            "shapeHash": "shape",
            "keep": 0,
            "tail": ["m1"],
            "requestDigest": format!("d{n}"),
            "params": { "model": "claude-opus-5-5" },
        })
    };
    vec![
        attachment(0, json!({ "type": "instructions", "files": files })),
        json!({
            "type": "api-request-shape",
            "sessionId": "session",
            "timestamp": at(0),
            "shapeHash": "shape",
            "shape": { "system": system, "tools": tools },
        }),
        json!({ "type": "api-request-blob", "sessionId": "session", "hash": "m1", "message": { "role": "user", "content": "SYNTHETIC_PRIVATE_PROMPT" } }),
        request(1),
        assistant(
            1,
            json!([
                { "type": "tool_use", "id": "read", "name": "Read", "input": { "file_path": format!("{dir}/note.md") } },
                { "type": "tool_use", "id": "write", "name": "Write",
                  "input": { "file_path": format!("{dir}/new.md"), "content": "SYNTHETIC_PRIVATE_NOTE" } },
                { "type": "tool_use", "id": "other", "name": "Read", "input": { "file_path": "/repo/src/main.rs" } },
            ]),
        ),
        json!({
            "type": "user",
            "timestamp": at(2),
            "sessionId": "session",
            "message": { "content": [
                { "type": "tool_result", "tool_use_id": "read", "content": "SYNTHETIC_PRIVATE_RECALLED" },
                { "type": "tool_result", "tool_use_id": "write", "content": "ok" },
                { "type": "tool_result", "tool_use_id": "other", "content": "SYNTHETIC_PRIVATE_SOURCE" },
            ] },
        }),
        request(3),
        assistant(3, json!([])),
    ]
}

fn memory_check(report: &Report, harness: Harness) -> &crate::model::MemoryCheck {
    report
        .checks
        .iter()
        .find_map(|c| match c {
            crate::model::CheckResult::Memory(m) if m.harness == harness => Some(m),
            _ => None,
        })
        .unwrap()
}

#[test]
fn claude_auto_memory_is_priced_apart_from_the_system_prompt_and_advised_off() {
    let w = Workspace::new();
    let root = std::fs::canonicalize(&w.claude).unwrap();
    let root = root.to_string_lossy();
    let records = claude_memory_session(&root, true);
    std::fs::write(
        w.claude.join("projects/project/main.jsonl"),
        jsonl(&records),
    )
    .unwrap();
    let data = scan(&options(&w, None)).unwrap();
    let e = &data.exposure;
    let component = |name: &str| e.components.iter().find(|c| c.name == name).unwrap();
    // The section is measured as JSON text, so its quotes count escaped.
    let section = r#"# Memory\n\nSYNTHETIC_PRIVATE_MEMORY_GUIDE \"quoted\"\n\n"#;
    let system = &records[1]["shape"]["system"];
    let memory_section = component(MEMORY_SECTION);
    assert_eq!(memory_section.category, Category::Memory);
    assert_eq!(memory_section.size.characters, Some(section.len() as i64));
    assert_eq!(
        component("wire-system").size.characters,
        Some((length(system) - section.len()) as i64)
    );
    assert_eq!(e.requests[0].view.prefix, 3);
    let index = component(&format!("{root}/projects/project/memory/MEMORY.md"));
    assert_eq!(index.category, Category::Memory);
    assert_eq!(e.memory.len(), 2);
    let write = e
        .memory
        .iter()
        .find(|m| m.access == MemoryAccess::Write)
        .unwrap();
    assert_eq!(write.request.as_deref(), Some(&*data.requests[0].id));
    assert!(write.characters.unwrap() > "SYNTHETIC_PRIVATE_NOTE".len() as i64);
    // The memory file read back is memory's output, not the Read tool's.
    let read: Vec<&str> = e.tool_results.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(read, [MEMORY_OUTPUT, "Write", "Read"]);

    let report = report(&data, 1.0);
    let memory = row(&report, crate::context::CLAUDE_MEMORY);
    assert_eq!(memory.kind, ContextKind::Memory);
    assert_eq!(memory.calls, Some(2));
    assert_eq!(memory.verdict, None);
    assert_eq!(
        control_text(memory.control.as_ref().unwrap(), Harness::Claude),
        r#""autoMemoryEnabled": false"#
    );
    let check = memory_check(&report, Harness::Claude);
    assert_eq!(check.base.action, crate::model::Action::Advisory);
    assert_eq!(check.base.current, Some(true));
    assert_eq!((check.reads, check.writes), (1, 1));
    assert!(check.writes_usd.unwrap() > 0.0);
    assert_eq!(
        check.base.savings_usd,
        Some(memory.estimated_usd.unwrap() + check.writes_usd.unwrap())
    );
    let text = format_report(
        &report,
        ReportOptions {
            width: None,
            color: false,
            unicode: false,
            details: false,
            host: None,
        },
    );
    assert!(shows(
        &text,
        "consider",
        "Auto memory on costs $",
        &[
            "Opinion: ",
            "used 2 times (1 read, 1 write). Set \"autoMemoryEnabled\": false in "
        ]
    ));
    assert!(!text.contains("MEMORY.md"));
    // Advice is not a change, so it leaves the exit code alone.
    assert!(
        !report
            .checks
            .iter()
            .any(|c| c.action() == crate::model::Action::Change)
    );
    assert!(!leaks(&report, &["SYNTHETIC_PRIVATE"]));
}

/// The note auto memory's check gives when recorded sessions disagree with the setting.
fn memory_note(check: &crate::model::MemoryCheck) -> Option<&str> {
    check
        .base
        .uncertainty
        .iter()
        .find(|u| u.contains("a session keeps the setting it started with"))
        .map(String::as_str)
}

#[test]
fn claude_auto_memory_turned_off_is_kept_off() {
    let w = Workspace::new();
    let root = std::fs::canonicalize(&w.claude).unwrap();
    std::fs::write(
        w.claude.join("settings.json"),
        r#"{ "autoMemoryEnabled": false }"#,
    )
    .unwrap();
    let records = claude_memory_session(&root.to_string_lossy(), false);
    std::fs::write(
        w.claude.join("projects/project/main.jsonl"),
        jsonl(&records),
    )
    .unwrap();
    let data = scan(&options(&w, None)).unwrap();
    assert_eq!(categories(&data.exposure, Category::Memory), 0);
    let first = report(&data, 1.0);
    let check = memory_check(&first, Harness::Claude);
    assert_eq!(check.base.current, Some(false));
    assert_eq!(memory_note(check), None);
    let rendered = text(&first);
    assert!(shows(
        &rendered,
        "keep",
        "Auto memory off [",
        &["Measured: costs nothing."]
    ));

    // Turned off after an earlier session used it: still kept off, but what it cost
    // in the window shows, and the earlier session is noted.
    let root = root.to_string_lossy();
    std::fs::write(
        w.claude.join("projects/project/early.jsonl"),
        jsonl(&claude_memory_session(&root, true)),
    )
    .unwrap();
    std::fs::write(
        w.claude.join("projects/project/main.jsonl"),
        jsonl(&later(claude_memory_session(&root, false))),
    )
    .unwrap();
    let turned_off = report(&scan(&options(&w, None)).unwrap(), 1.0);
    let check = memory_check(&turned_off, Harness::Claude);
    assert_eq!(check.base.current, Some(false));
    assert!(check.writes_usd.unwrap() > 0.0);
    assert!(
        memory_note(check)
            .unwrap()
            .starts_with("1 of 2 sessions still carried memory")
    );
    let text = text(&turned_off);
    assert!(shows(
        &text,
        "keep",
        "Auto memory off costs",
        &["Measured: used 2 times (1 read, 1 write)"]
    ));

    // A T3 instance whose launch arguments or environment could override the
    // setting leaves it unknown, advised off as it may be on.
    let mut data = scan(&options(&w, None)).unwrap();
    for profile in &mut data.profiles {
        profile.t3 = Some(crate::model::T3Instance {
            state: "/synthetic/t3".into(),
            instance: "claudeAgent".into(),
            enabled: true,
            idle_mode: crate::model::T3IdleMode::Off,
            idle_owner: "synthetic".into(),
            ownership_resolved: false,
            settings_unresolved: true,
            limitations: vec![],
            inherited_window: None,
            retained_since: None,
        });
    }
    let t3 = report(&data, 1.0);
    let check = memory_check(&t3, Harness::Claude);
    assert_eq!(check.base.current, None);
    assert_eq!(check.base.action, crate::model::Action::Advisory);
    assert!(
        check
            .unresolved
            .as_deref()
            .unwrap()
            .starts_with("the T3 instance's launch arguments")
    );
}

/// The same records as a later session, with their own ids.
fn later(records: Vec<Value>) -> Vec<Value> {
    fn walk(key: &str, value: &mut Value) {
        match value {
            Value::Object(map) => map.iter_mut().for_each(|(k, v)| walk(k, v)),
            Value::Array(items) => items.iter_mut().for_each(|v| walk(key, v)),
            Value::String(text) if key == "timestamp" => {
                let second: u32 = text[17..19].parse().unwrap();
                *text = at(second + 30);
            }
            Value::String(text) if key == "sessionId" => *text = "later".into(),
            Value::String(text) if ["id", "requestRef", "uuid", "shapeHash"].contains(&key) => {
                *text = format!("later-{text}");
            }
            _ => {}
        }
    }
    records
        .into_iter()
        .map(|mut r| {
            walk("", &mut r);
            r
        })
        .collect()
}

#[test]
fn claude_auto_memory_follows_settings_at_the_latest_sessions_directory() {
    let w = Workspace::new();
    let root = std::fs::canonicalize(&w.claude).unwrap();
    std::fs::write(
        w.claude.join("settings.json"),
        r#"{ "autoMemoryEnabled": false }"#,
    )
    .unwrap();
    // Every recorded session started before the setting changed and still carries
    // memory, and one of them made the newest request: today's setting decides.
    let mut early = claude_memory_session(&root.to_string_lossy(), true);
    early.push(json!({
        "type": "api-request", "id": "api-50", "timestamp": at(50), "sessionId": "session",
        "shapeHash": "shape", "keep": 0, "tail": ["m1"], "requestDigest": "d50",
        "params": { "model": "claude-opus-5-5" },
    }));
    early.push(assistant(50, json!([])));
    std::fs::write(w.claude.join("projects/project/main.jsonl"), jsonl(&early)).unwrap();
    let first = report(&scan(&options(&w, None)).unwrap(), 1.0);
    let check = memory_check(&first, Harness::Claude);
    assert_eq!(check.base.current, Some(false));
    assert_eq!(check.base.action, crate::model::Action::Keep);
    assert_eq!(check.off, r#""autoMemoryEnabled": false"#);
    assert!(
        check
            .off_file
            .as_deref()
            .unwrap()
            .ends_with("settings.json")
    );
    assert_eq!(
        memory_note(check).unwrap(),
        format!(
            "1 of 1 sessions still carried memory at their latest request, though {}:autoMemoryEnabled turns it off: a session keeps the setting it started with until it restarts, and an environment variable set only where it was launched is not visible here",
            check.off_file.as_deref().unwrap()
        )
    );
    let rendered = text(&first);
    assert!(!rendered.contains("settings.local.json"));
    assert!(shows(&rendered, "keep", "Auto memory off costs", &[]));

    // A project setting at the latest session's directory owns it, even when the
    // user setting agrees.
    std::fs::write(w.claude.join("settings.json"), "{}").unwrap();
    let project = w.home.join("project");
    std::fs::create_dir_all(project.join(".claude")).unwrap();
    let local = project.join(".claude/settings.local.json");
    std::fs::write(&local, r#"{ "autoMemoryEnabled": true }"#).unwrap();
    let project = std::fs::canonicalize(&project).unwrap();
    let records: Vec<Value> = claude_memory_session(&root.to_string_lossy(), true)
        .into_iter()
        .map(|mut r| {
            r["cwd"] = json!(project.to_string_lossy());
            r
        })
        .collect();
    std::fs::write(
        w.claude.join("projects/project/main.jsonl"),
        jsonl(&records),
    )
    .unwrap();
    let report = report(&scan(&options(&w, None)).unwrap(), 1.0);
    let check = memory_check(&report, Harness::Claude);
    assert_eq!(check.base.current, Some(true));
    let local = std::fs::canonicalize(&local).unwrap();
    let local = local.to_string_lossy();
    assert_eq!(check.off_file.as_deref(), Some(&*local));
    // The context row's details name the same setting, not the user settings file.
    let details = format_report(
        &report,
        ReportOptions {
            width: None,
            color: false,
            unicode: false,
            details: true,
            host: None,
        },
    );
    assert!(details.lines().any(|l| {
        l.trim_start()
            .strip_prefix("Turn off: \"autoMemoryEnabled\": false in ")
            .is_some_and(|file| {
                std::path::Path::new(file).ends_with(
                    std::path::Path::new("project")
                        .join(".claude")
                        .join("settings.local.json"),
                )
            })
    }));
}

#[test]
fn claude_auto_memory_without_recorded_context_has_an_unknown_cost() {
    let w = Workspace::new();
    let root = std::fs::canonicalize(&w.claude).unwrap();
    let file = format!(
        "{}/projects/project/memory/MEMORY.md",
        root.to_string_lossy()
    );
    let records = [
        attachment(
            0,
            json!({ "type": "instructions", "files": [
                { "path": file, "type": "AutoMem", "content": "SYNTHETIC_PRIVATE_INDEX" },
            ] }),
        ),
        assistant(1, json!([])),
    ];
    std::fs::write(
        w.claude.join("projects/project/main.jsonl"),
        jsonl(&records),
    )
    .unwrap();
    let first = report(&scan(&options(&w, None)).unwrap(), 1.0);
    let check = memory_check(&first, Harness::Claude);
    assert_eq!(check.context_usd, None);
    assert_eq!(check.base.savings_usd, None);
    assert!(
        check
            .base
            .uncertainty
            .iter()
            .any(|u| u.contains("loaded memory is unmeasured"))
    );

    // Beside a session that recorded its context, the total is a lower bound.
    std::fs::write(
        w.claude.join("projects/project/later.jsonl"),
        jsonl(&later(claude_memory_session(&root.to_string_lossy(), true))),
    )
    .unwrap();
    let both = report(&scan(&options(&w, None)).unwrap(), 1.0);
    let check = memory_check(&both, Harness::Claude);
    let coverage = check.base.coverage.unwrap();
    assert_eq!((coverage.priced, coverage.total), (2, 3));
    assert!(check.base.savings_usd.unwrap() > 0.0);
    // The temporary path's length moves the cents, since the loaded paths are part
    // of the priced context.
    let both = text(&both);
    assert!(shows(
        &both,
        "consider",
        "Auto memory on costs at least $0.",
        &["; from the 66.6% of requests that recorded their context."]
    ));
}

#[test]
fn codex_memory_summary_and_memory_tools_count_though_memories_are_off_by_default() {
    let w = Workspace::new();
    // Resolved as discovery resolves the profile root; a Windows shell names the file
    // without the `\\?\` prefix `fs::canonicalize` adds.
    let home = crate::discovery::realpath(&w.codex.to_string_lossy()).unwrap();
    let records = [
        codex_meta(),
        turn(),
        json!({
            "type": "response_item",
            "timestamp": at(1),
            "payload": {
                "type": "message",
                "role": "developer",
                "content": [
                    { "type": "input_text", "text": "SYNTHETIC_PRIVATE_PERMISSIONS" },
                    { "type": "input_text", "text": "## Memory\nSYNTHETIC_PRIVATE_SUMMARY" },
                    { "type": "input_text", "text": "SYNTHETIC_PRIVATE_CONTINUED" },
                ],
                "internal_chat_message_metadata_passthrough": {
                    "content_item_kinds": ["permissions", "memories.instructions", "memories.instructions"],
                },
            },
        }),
        // A full world state does not drop the summary.
        world(
            1,
            json!({ "agents_md": { "text": "SYNTHETIC_PRIVATE_AGENTS" } }),
            true,
        ),
        json!({
            "type": "response_item",
            "timestamp": at(2),
            "payload": { "type": "function_call", "namespace": "memories", "name": "read",
                         "call_id": "recall", "arguments": "{\"path\":\"SYNTHETIC_PRIVATE_PATH\"}" },
        }),
        json!({
            "type": "response_item",
            "timestamp": at(3),
            "payload": { "type": "function_call_output", "call_id": "recall", "output": "SYNTHETIC_PRIVATE_RECALLED" },
        }),
        json!({
            "type": "response_item",
            "timestamp": at(3),
            "payload": { "type": "function_call", "namespace": "memories", "name": "add_ad_hoc_note",
                         "call_id": "note", "arguments": "{\"note\":\"SYNTHETIC_PRIVATE_NOTE\"}" },
        }),
        json!({
            "type": "event_msg",
            "timestamp": at(4),
            "payload": { "type": "item_completed", "item": {
                "type": "CommandExecution", "id": "shell", "exit_code": 0, "status": "completed",
                "parsed_cmd": [{ "type": "read", "name": "notes.md", "path": format!("{home}/memories/notes.md"), "cmd": "cat" }],
                "aggregated_output": "SYNTHETIC_PRIVATE_SHELL",
            } },
        }),
        billed(5),
    ];
    let file = w.codex.join("sessions/main.jsonl");
    std::fs::write(&file, jsonl(&records)).unwrap();
    let data = scan(&options(&w, None)).unwrap();
    let e = &data.exposure;
    let summary = e
        .components
        .iter()
        .find(|c| c.category == Category::Memory)
        .unwrap();
    assert_eq!(
        summary.size.characters,
        Some("## Memory\nSYNTHETIC_PRIVATE_SUMMARY\nSYNTHETIC_PRIVATE_CONTINUED".len() as i64)
    );
    assert!(has(&e.requests[0].view.candidates, &summary.id));
    let accesses: Vec<MemoryAccess> = e.memory.iter().map(|m| m.access).collect();
    assert_eq!(
        accesses,
        [MemoryAccess::Read, MemoryAccess::Write, MemoryAccess::Read]
    );
    let r = report(&data, 1.0);
    let memory = row(&r, crate::context::CODEX_MEMORY);
    assert_eq!(memory.calls, Some(3));
    assert_eq!(memory.requests, 1);
    // Off by default: the session that carried a summary is noted, not taken as on.
    let check = memory_check(&r, Harness::Codex);
    assert_eq!(check.base.current, Some(false));
    assert_eq!(check.off, "features.memories = false");
    assert_eq!(check.context_usd, memory.estimated_usd);
    assert!(
        memory_note(check)
            .unwrap()
            .starts_with("1 of 1 sessions still carried memory")
    );
    assert!(shows(
        &text(&r),
        "keep",
        "Codex memories off costs unknown",
        &["; extraction and consolidation are not measured."]
    ));
    assert!(!leaks(&r, &["SYNTHETIC_PRIVATE"]));

    // Turned on in settings, it is advised off.
    std::fs::write(w.codex.join("config.toml"), "[features]\nmemories = true\n").unwrap();
    let on = report(&scan(&options(&w, None)).unwrap(), 1.0);
    let check = memory_check(&on, Harness::Codex);
    assert_eq!(check.base.current, Some(true));
    assert_eq!(memory_note(check), None);
    assert!(shows(
        &text(&on),
        "consider",
        "Codex memories on",
        &["Opinion: ", "Set features.memories = false in "]
    ));
    std::fs::remove_file(w.codex.join("config.toml")).unwrap();

    // Compaction rebuilds the developer context: a summary its replacement history
    // carries stays, and one it leaves out is gone.
    let developer = |text: &str| {
        json!({ "type": "message", "role": "developer",
                "content": [{ "type": "input_text", "text": text }],
                "internal_chat_message_metadata_passthrough": {
                    "content_item_kinds": ["memories.instructions"] } })
    };
    for (history, kept) in [
        (vec![developer("## Memory\nSYNTHETIC_PRIVATE_NEWER")], true),
        (vec![], false),
    ] {
        let mut records = records[..3].to_vec();
        records.push(json!({ "type": "compacted", "timestamp": at(2),
                             "payload": { "message": "", "replacement_history": history } }));
        records.push(billed(5));
        std::fs::write(&file, jsonl(&records)).unwrap();
        let data = scan(&options(&w, None)).unwrap();
        let e = &data.exposure;
        let carried = e.requests[0].view.candidates.iter().any(|id| {
            e.components
                .iter()
                .any(|c| c.id == *id && c.category == Category::Memory)
        });
        assert_eq!(carried, kept);
    }

    // With no summary and no setting, Codex memories are off.
    let context = world(
        1,
        json!({ "agents_md": { "text": "SYNTHETIC_PRIVATE_AGENTS" } }),
        true,
    );
    std::fs::write(&file, jsonl(&[codex_meta(), turn(), context, billed(5)])).unwrap();
    let data = scan(&options(&w, None)).unwrap();
    assert!(shows(
        &text(&report(&data, 1.0)),
        "keep",
        "Codex memories off [",
        &["Measured: costs nothing."]
    ));
    // A memory store shows Codex generated memories at some point, in requests it
    // records only in telemetry, so the line cannot say they cost nothing.
    std::fs::create_dir_all(w.codex.join("memories")).unwrap();
    let data = scan(&options(&w, None)).unwrap();
    assert!(shows(
        &text(&report(&data, 1.0)),
        "keep",
        "Codex memories off [",
        &[
            "Measured: never read back or written; no measured cost, but extraction and consolidation are not measured."
        ]
    ));
    std::fs::remove_dir(w.codex.join("memories")).unwrap();
    // A request that recorded no context may have loaded memories while they were
    // on, so turning them off does not make its cost zero.
    std::fs::write(&file, jsonl(&[codex_meta(), turn(), billed(5)])).unwrap();
    let data = scan(&options(&w, None)).unwrap();
    assert!(shows(
        &text(&report(&data, 1.0)),
        "keep",
        "Codex memories off costs unknown",
        &[]
    ));

    // A trusted project's config at the latest session's directory turns them on.
    let project = w.home.join("repo");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::create_dir_all(project.join(".codex")).unwrap();
    let project = crate::discovery::realpath(&project.to_string_lossy()).unwrap();
    let project_config = std::path::Path::new(&project)
        .join(".codex")
        .join("config.toml")
        .to_string_lossy()
        .into_owned();
    std::fs::write(&project_config, "[features]\nmemories = true\n").unwrap();
    std::fs::write(
        w.codex.join("config.toml"),
        format!("[projects.{project:?}]\ntrust_level = \"trusted\"\n"),
    )
    .unwrap();
    let mut meta = codex_meta();
    meta["payload"]["cwd"] = json!(project);
    std::fs::write(&file, jsonl(&[meta, turn(), billed(5)])).unwrap();
    let data = scan(&options(&w, None)).unwrap();
    let check = memory_check(&report(&data, 1.0), Harness::Codex).clone();
    assert_eq!(check.base.current, Some(true));
    assert_eq!(check.off_file.as_deref(), Some(&*project_config));

    // Without a recorded trust decision, whether the project layer applies is
    // unknown, so the state is too: a yellow advisory that it may be on.
    std::fs::write(w.codex.join("config.toml"), "").unwrap();
    let unresolved = |data: &crate::model::Scan| {
        let r = report(data, 1.0);
        let check = memory_check(&r, Harness::Codex).clone();
        let wide = ReportOptions {
            width: None,
            color: false,
            unicode: false,
            details: false,
            host: None,
        };
        (check, format_report(&r, wide))
    };
    let (check, rendered) = unresolved(&scan(&options(&w, None)).unwrap());
    assert_eq!(check.base.current, None);
    assert_eq!(check.base.action, crate::model::Action::Advisory);
    assert_eq!(
        check.unresolved.as_deref(),
        Some("a project .codex/config.toml sets memories, but the project's trust is not recorded")
    );
    assert!(shows(
        &rendered,
        "consider",
        "Codex memories may be on",
        &[
            "Unknown whether it is on: a project .codex/config.toml sets memories, but the project's trust is not recorded. Opinion: ",
            "Set features.memories = false in "
        ]
    ));
    assert!(
        !report(&scan(&options(&w, None)).unwrap(), 1.0)
            .checks
            .iter()
            .any(|c| c.action() == crate::model::Action::Change)
    );

    // Trusted, but a named profile that sets memories may have been selected.
    let trusted = format!("[projects.{project:?}]\ntrust_level = \"trusted\"\n");
    std::fs::write(w.codex.join("config.toml"), &trusted).unwrap();
    let named = w.codex.join("loud.config.toml");
    std::fs::write(&named, "[features]\nmemories = false\n").unwrap();
    let (check, rendered) = unresolved(&scan(&options(&w, None)).unwrap());
    assert_eq!(check.base.current, None);
    assert!(
        check
            .unresolved
            .unwrap()
            .starts_with("a named config profile sets memories")
    );
    // Turning it off in the user config alone would leave the profile's setting.
    assert!(rendered.contains("loud.config.toml where it sets memories."));
    std::fs::remove_file(&named).unwrap();

    // A named profile that changes project roots matters only where a layer it could
    // add or drop sets memories: here the root's layer, seen from a nested directory.
    let nested = format!("{project}/nested");
    std::fs::create_dir_all(&nested).unwrap();
    let mut meta = codex_meta();
    meta["payload"]["cwd"] = json!(nested);
    std::fs::write(&file, jsonl(&[meta, turn(), billed(5)])).unwrap();
    std::fs::write(
        w.codex.join("quiet.config.toml"),
        "project_root_markers = []\n",
    )
    .unwrap();
    let (check, _) = unresolved(&scan(&options(&w, None)).unwrap());
    assert_eq!(check.base.current, None);
    assert!(
        check
            .unresolved
            .unwrap()
            .starts_with("a .codex/config.toml above the latest session's directory sets memories")
    );
    std::fs::remove_file(&project_config).unwrap();
    let (check, _) = unresolved(&scan(&options(&w, None)).unwrap());
    assert_eq!((check.base.current, check.unresolved), (Some(false), None));
}
