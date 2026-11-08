//! Exposure derived through the whole reader, replay and report pipeline.

use serde_json::json;

use super::*;
use crate::context::control_text;
use crate::controls::Env;
use crate::discovery::DiscoveryOptions;
use crate::model::{ContextCost, ContextKind, Report, Scan, Verdict};
use crate::reader::{ScanOptions, scan};
use crate::replay::{AnalyzeOptions, analyze};
use crate::report::{ReportOptions, format_report};
use crate::test_support::{Workspace, prices};

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
            env: Env::new(),
            claude_dirs: vec![],
            codex_dirs: vec![],
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
            width: Some(200),
            color: false,
            unicode: false,
            details: false,
        },
    )
}

/// Whether the serialized value contains any of `needles`.
fn leaks(value: &impl serde::Serialize, needles: &[&str]) -> bool {
    let text = serde_json::to_string(value).unwrap();
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

/// Whether some line holds `name`, an optional ` [...]` tag, then a rest that
/// satisfies `rest`, like the pattern `name( \[[^\]]*\])?rest`.
fn labelled(text: &str, name: &str, rest: impl Fn(&str) -> bool) -> bool {
    text.match_indices(name).any(|(i, _)| {
        let after = &text[i + name.len()..];
        let line = |s: &str| rest(s.split('\n').next().unwrap());
        let tagged = after
            .strip_prefix(" [")
            .and_then(|t| t.find(']').map(|end| &t[end + 1..]));
        line(after) || tagged.is_some_and(line)
    })
}

/// A rest that starts with `prefix` and later holds `suffix` on the same line.
fn then(prefix: &str, suffix: &str) -> impl Fn(&str) -> bool {
    move |s: &str| {
        s.strip_prefix(prefix)
            .is_some_and(|tail| tail.contains(suffix))
    }
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
        .find(|c| c.provenance == "opaque-schema-identity")
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
            .all(|v| !v.automatic_unused && v.nested_calls == Visible::Unknown)
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
    assert!(e.components.iter().all(|c| c.current == "unresolved"));
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
    assert!(server.cost_usd > 0.0);
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
    assert!(labelled(&text, "Artifact tool", |s| s
        .starts_with(": on. Cost unknown")));
    assert!(text.contains("and the skill and subagent lists: cost unknown."));
}

#[test]
fn typed_and_mentioned_skills_nested_completions_and_script_references_stay_distinct() {
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
    assert_eq!(kinds(e, InvocationKind::SkillRequest).len(), 2);
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
            .all(|v| v.nested_calls == Visible::Partial
                && v.typed_skills == Observed::Observed
                && !v.automatic_unused)
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
        data.requests[0].id,
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
        control_text(artifact.control.as_ref().unwrap()),
        r#""enableArtifact": false"#
    );
    assert_eq!(artifact.calls, Some(0));
    assert!(artifact.current);
    // Tools lead the prefix, so Artifact sits entirely inside the 2,000 cached-read tokens.
    let artifact_tokens = length(&tools[0]) as f64 * ratio;
    assert!(artifact_tokens < 2000.0);
    assert!((artifact.cost_usd - (artifact_tokens * 0.2) / 1e6).abs() < 1e-12);
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
        control_text(review.control.as_ref().unwrap()),
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
    assert!((review.cost_usd / review.tokens.unwrap() as f64 / 4e-6 - 1.0).abs() < 0.01);
    assert_eq!(row(&report, "Claude Code system prompt").control, None);
    assert!(!leaks(&report, &["SYNTHETIC_PRIVATE"]));
    // Every priced piece here costs well under the default $1 per use; an
    // unpriced one gets no verdict.
    assert!(report.context.iter().all(|c| {
        let priced = c.control.is_some() && c.estimated_usd.is_some();
        c.verdict == priced.then_some(Verdict::Keep)
    }));

    // A piece is recommended off when its cost per use, counting no use as one,
    // exceeds the limit. Typed skill requests would not count as uses.
    let strict = analyze(AnalyzeOptions {
        scan: data.clone(),
        catalog: &prices(),
        since: None,
        now: NOW,
        max_cost_per_use_usd: artifact.cost_usd / 2.0,
    })
    .unwrap();
    let verdict = |name: &str| row(&strict, name).verdict;
    assert_eq!(verdict("Artifact"), Some(Verdict::Off));
    assert_eq!(verdict("docs"), Some(Verdict::Off));
    assert_eq!(verdict("review"), Some(Verdict::Keep));
    assert_eq!(verdict("Bash"), None);
    let text = text(&strict);
    assert!(labelled(
        &text,
        "Artifact tool",
        then(": on -> off. Costs ", "; never used.")
    ));
    assert!(labelled(
        &text,
        "Skill review",
        then(": on. Costs ", "; the model never used it.")
    ));
    let snippet = &text[text.find("To turn these off").unwrap()..];
    assert!(snippet.contains(r#""enableArtifact": false"#));
    let denies_docs = snippet.match_indices(r#""deny": ["#).any(|(i, d)| {
        let after = snippet[i + d.len()..].trim_start();
        after
            .strip_prefix(r#""mcp__docs""#)
            .is_some_and(|rest| rest.trim_start().starts_with(']'))
    });
    assert!(denies_docs);
    assert!(!snippet.contains("skillOverrides"));
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
    assert!((instructions.cost_usd / (tokens * (8.0 + 8.0) * 1e-6) - 1.0).abs() < 0.001);
    // In the first request the listing could straddle 1h writes and ordinary
    // input, so that request stays unpriced; later ones read it from cache.
    assert_eq!(review.requests, 6);
    assert_eq!(review.priced, 2);
    assert!((review.cost_usd / (review.tokens.unwrap() as f64 * 2.0 * 0.2e-6) - 1.0).abs() < 0.01);
    assert_eq!(review.estimated_usd, Some((review.cost_usd * 6.0) / 2.0));
    // The server row is priced only where both its tool and its instructions
    // were: not in the first request, where the instructions straddle slots.
    let docs = row(&report, "docs");
    assert_eq!(docs.requests, 6);
    assert_eq!(docs.priced, 2);
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
        .find(|c| c.id == aggregate.members[0])
        .unwrap();
    assert_eq!(
        member.size.characters,
        Some("- example:skill: SYNTHETIC_PRIVATE_DESCRIPTION".len() as i64)
    );
    assert!(!has(&e.requests[0].view.candidates, &member.id));
    let calls = kinds(e, InvocationKind::McpCall);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].status, InvocationStatus::Completed);
    assert_eq!(
        kinds(e, InvocationKind::SkillLoad)[0].status,
        InvocationStatus::Completed
    );
    assert!(
        e.components
            .iter()
            .any(|c| c.provenance == "provided-additional-tools" && c.size.characters.is_some())
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
    assert_eq!(system.estimated_usd, Some(system.cost_usd * 2.0));
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
        .find(|c| c.provenance == "prompt-snapshot-tool")
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
    assert!(requests.iter().all(
        |i| i.identity_basis == IdentityBasis::ContentOccurrence && i.locations.len() == 2
    ));
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
            .any(|c| c.provenance == "unclassified-rendered-attachment")
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
            .any(|c| c.provenance == "world-state-metadata-only")
    );
}
