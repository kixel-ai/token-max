//! `token-max sessions`: each transcript session's usage at public API prices, for tools such
//! as moq-dev/quest that tie sessions to branches.
//!
//! The reader notes what every record says about its session; the export then prices the
//! session's deduplicated requests. Native session IDs stay out of the scan's serialized form,
//! so they never cross SSH or reach the report.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::collections::{HashMap, IndexMap};
use crate::model::{Harness, Millis, Request, Scan, Text};
use crate::prices::{Catalog, add_costs, price_part, request_evidence, server_cost};
use crate::safe::{iso, label};

/// What one transcript record says about its session.
pub(crate) struct Record<'a> {
    pub harness: Harness,
    /// The reader's key for the record's session.
    pub session: &'a Text,
    /// Whether the file is a nested Claude subagent transcript.
    pub subagent: bool,
    pub file: &'a Text,
    pub cwd: Option<&'a Text>,
    pub raw: &'a Map<String, Value>,
    pub time: Option<Millis>,
    /// Whether the record falls in the selected window.
    pub selected: bool,
}

/// A Git branch a session's records name, from the first to the last selected one.
#[derive(Clone, Debug, PartialEq)]
struct Branch {
    name: String,
    first: Millis,
    last: Millis,
}

/// What a session's records name, without content.
#[derive(Clone, Debug, PartialEq)]
struct Facts {
    /// When the session began: its first record, or for Codex its `session_meta`, since a
    /// fork's copied records keep their original times.
    created: Option<Millis>,
    id: Option<String>,
    /// The Codex thread this one was forked from.
    forked_from: Option<String>,
    parent: Option<String>,
    cwd: Option<Text>,
    branches: Vec<Branch>,
    review: bool,
    files: Vec<Text>,
}

/// Every session's facts, by the reader's session key.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sessions {
    facts: IndexMap<Text, Facts>,
    /// The last session observed, since consecutive records almost always share one.
    last: usize,
}

/// A recorded native ID, such as a UUID; anything else is not kept.
fn native(value: Option<&Value>) -> Option<String> {
    label(value?)
}

/// The thread a Codex `session_meta` names and the thread it was spawned from: its root as
/// `session_id`, or in older spawns its parent as `parent_thread_id`.
pub(crate) fn codex_thread(meta: &Map<String, Value>) -> (Option<&str>, Option<&str>) {
    fn text(v: Option<&Value>) -> Option<&str> {
        v.and_then(Value::as_str).filter(|s| !s.is_empty())
    }
    let id = text(meta.get("id")).or_else(|| text(meta.get("session_id")));
    let parent = text(meta.get("session_id"))
        .filter(|root| Some(*root) != id)
        .or_else(|| text(meta.get("parent_thread_id")))
        .or_else(|| {
            text(
                meta.get("source")
                    .and_then(|s| s.pointer("/subagent/thread_spawn/parent_thread_id")),
            )
        });
    (id, parent)
}

/// A name Git could have given a branch.
fn branch(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|name| {
        (1..=255).contains(&name.len())
            && !name.chars().any(|c| c.is_control() || c.is_whitespace())
    })
}

impl Sessions {
    /// Whether no record named a session.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }

    pub(crate) fn observe(&mut self, record: Record) {
        let index = match self.facts.get_index(self.last) {
            Some((key, _)) if key == record.session => self.last,
            _ => {
                let entry = self.facts.entry(record.session.clone());
                let index = entry.index();
                entry.or_insert_with(|| Facts {
                    created: None,
                    id: None,
                    forked_from: None,
                    parent: None,
                    cwd: None,
                    branches: vec![],
                    review: false,
                    files: vec![],
                });
                index
            }
        };
        self.last = index;
        let facts = &mut self.facts[index];
        if facts.files.last() != Some(record.file) && !facts.files.contains(record.file) {
            facts.files.push(record.file.clone());
        }
        if facts.cwd.is_none() {
            facts.cwd = record.cwd.cloned();
        }
        let raw = record.raw;
        let earliest = |created: Option<Millis>| match (created, record.time) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let name = match record.harness {
            // A subagent's records carry its parent's session ID and its own agent ID.
            Harness::Claude => {
                facts.created = earliest(facts.created);
                if facts.id.is_none() || (record.subagent && facts.parent.is_none()) {
                    let session = native(raw.get("sessionId"));
                    if record.subagent {
                        facts.parent = facts.parent.take().or(session);
                        facts.id = facts.id.take().or_else(|| {
                            native(raw.get("agentId")).or_else(|| {
                                let name = record.file.rsplit(['/', '\\']).next()?;
                                let stem = name.strip_suffix(".jsonl").unwrap_or(name);
                                native(Some(&stem.strip_prefix("agent-").unwrap_or(stem).into()))
                            })
                        });
                    } else {
                        facts.id = session;
                    }
                }
                branch(raw.get("gitBranch"))
            }
            Harness::Codex => {
                if raw.get("type").and_then(Value::as_str) != Some("session_meta") {
                    return;
                }
                let Some(meta) = raw.get("payload").and_then(Value::as_object) else {
                    return;
                };
                facts.created = earliest(facts.created);
                let (id, parent) = codex_thread(meta);
                facts.id = facts
                    .id
                    .take()
                    .or_else(|| native(id.map(Value::from).as_ref()));
                facts.parent = facts
                    .parent
                    .take()
                    .or_else(|| native(parent.map(Value::from).as_ref()));
                facts.forked_from = facts
                    .forked_from
                    .take()
                    .or_else(|| native(meta.get("forked_from_id")));
                facts.review |= meta.get("source").and_then(|s| s.get("subagent"))
                    == Some(&Value::from("review"));
                branch(meta.get("git").and_then(|git| git.get("branch")))
            }
        };
        if let (Some(time), Some(name), true) = (record.time, name, record.selected) {
            match facts.branches.iter_mut().find(|b| b.name == name) {
                Some(b) => {
                    b.first = b.first.min(time);
                    b.last = b.last.max(time);
                }
                None => facts.branches.push(Branch {
                    name: name.to_string(),
                    first: time,
                    last: time,
                }),
            }
        }
    }
}

/// A branch as the export prints it.
#[derive(Debug, PartialEq, Serialize)]
pub struct BranchSpan {
    pub name: String,
    pub first: String,
    pub last: String,
}

/// One model and effort's usage within a session.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub requests: u64,
    /// Input tokens that neither read nor wrote the cache.
    pub input_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_read_tokens: i64,
    pub output_tokens: i64,
    /// The priced cost; a lower bound unless `complete`.
    pub cost_usd: f64,
    pub complete: bool,
}

/// One session's priced usage: one line of `token-max sessions`.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub harness: Harness,
    pub session: Option<String>,
    /// The session a subagent or Codex review thread was spawned from.
    pub parent: Option<String>,
    pub review: bool,
    pub cwd: Option<String>,
    pub branches: Vec<BranchSpan>,
    pub start: String,
    pub end: String,
    pub models: Vec<ModelUsage>,
    pub cost_usd: f64,
    /// False when any request could not be priced or a file had unreadable lines.
    pub complete: bool,
    pub skipped_lines: u64,
    pub files: Vec<String>,
}

/// Price each session with a selected request, ordered by its first request.
pub fn export<'s>(scan: &'s Scan, catalog: &Catalog) -> Vec<Session> {
    struct Totals<'a> {
        harness: Harness,
        start: Millis,
        end: Millis,
        rows: IndexMap<(Option<&'a str>, Option<&'a str>), ModelUsage>,
    }
    let facts = &scan.sessions.facts;
    let mut held: HashMap<&str, Vec<&Text>> = HashMap::default();
    for (key, f) in facts {
        for file in &f.files {
            held.entry(file).or_default().push(key);
        }
    }
    let ids: HashMap<&str, &Facts> = facts
        .values()
        .filter_map(|f| Some((f.id.as_deref()?, f)))
        .collect();
    // Sources rank before their forks, by recorded lineage and then by when they began.
    let rank = |key: &Text| {
        let mut f = facts.get(key);
        let created = f.and_then(|f| f.created).unwrap_or(Millis::MAX);
        let mut depth = 0;
        // Bounded so a cycle in recorded lineage cannot loop.
        while depth < facts.len()
            && let Some(source) = f.and_then(|f| ids.get(f.forked_from.as_deref()?))
        {
            f = Some(source);
            depth += 1;
        }
        (depth, created)
    };
    // A Codex fork copies its source's records under its own `session_meta`, so a request in
    // several rollouts belongs to the source, whichever the reader read first. Claude records
    // name their own session, which the reader already keeps.
    let owner = |r: &'s Request| -> &'s Text {
        let own = rank(&r.session);
        r.locations
            .iter()
            .filter_map(|l| held.get(&*l.file))
            .flatten()
            .map(|key| (rank(key), *key))
            .min_by_key(|(rank, _)| *rank)
            .filter(|(rank, _)| *rank < own)
            .map_or(&r.session, |(_, key)| key)
    };
    let mut sessions: IndexMap<&Text, Totals> = IndexMap::default();
    for r in &scan.requests {
        let key = if r.harness == Harness::Codex && r.locations.len() > 1 {
            owner(r)
        } else {
            &r.session
        };
        let totals = sessions.entry(key).or_insert_with(|| Totals {
            harness: r.harness,
            start: r.time,
            end: r.end_time,
            rows: IndexMap::default(),
        });
        totals.start = totals.start.min(r.time);
        totals.end = totals.end.max(r.end_time);
        let costs: Vec<_> = r.parts.iter().map(|p| price_part(catalog, r, p)).collect();
        let server = server_cost(r);
        let priced = request_evidence(r, add_costs(costs.iter().chain([&server])))
            .total_usd
            .is_some();
        for (index, (p, cost)) in r.parts.iter().zip(&costs).enumerate() {
            let key = (p.model.as_deref(), r.effort.as_deref());
            let row = totals.rows.entry(key).or_insert_with(|| ModelUsage {
                model: key.0.map(str::to_string),
                effort: key.1.map(str::to_string),
                requests: 0,
                input_tokens: 0,
                cache_write_tokens: 0,
                cache_read_tokens: 0,
                output_tokens: 0,
                cost_usd: 0.0,
                complete: true,
            });
            let u = &p.usage;
            let read = u.read.unwrap_or(0);
            let write = u.write.unwrap_or(0);
            row.input_tokens += u.input.map_or(0, |input| input - read - write);
            row.cache_write_tokens += write;
            row.cache_read_tokens += read;
            row.output_tokens += u.output.unwrap_or(0);
            row.cost_usd += cost.known_usd;
            row.complete &= priced && cost.total_usd.is_some();
            // Server fees belong to the request, which its first part stands for.
            if index == 0 {
                row.requests += 1;
                row.cost_usd += server.known_usd;
                row.complete &= server.total_usd.is_some();
            }
        }
    }
    let snapshots: HashMap<&str, _> = scan
        .snapshots
        .iter()
        .map(|s| (s.file.as_str(), s))
        .collect();
    let mut result: Vec<(Millis, Session)> = sessions
        .into_iter()
        .map(|(key, totals)| {
            let facts = facts.get(key);
            let files = facts.map_or(&[][..], |f| &f.files[..]);
            let (skipped_lines, deferred) = files
                .iter()
                .filter_map(|file| snapshots.get(&**file))
                .fold((0, 0), |(lines, bytes), s| {
                    (lines + s.skipped_lines, bytes + s.deferred_bytes)
                });
            let models: Vec<ModelUsage> = totals.rows.into_values().collect();
            let session = Session {
                harness: totals.harness,
                session: facts.and_then(|f| f.id.clone()),
                parent: facts.and_then(|f| f.parent.clone()),
                review: facts.is_some_and(|f| f.review),
                cwd: facts.and_then(|f| f.cwd.as_deref().map(str::to_string)),
                branches: facts
                    .map(|f| &f.branches[..])
                    .unwrap_or_default()
                    .iter()
                    .map(|b| BranchSpan {
                        name: b.name.clone(),
                        first: iso(b.first),
                        last: iso(b.last),
                    })
                    .collect(),
                start: iso(totals.start),
                end: iso(totals.end),
                cost_usd: models.iter().map(|m| m.cost_usd).sum(),
                complete: models.iter().all(|m| m.complete) && skipped_lines == 0 && deferred == 0,
                models,
                skipped_lines,
                files: files.iter().map(|f| f.to_string()).collect(),
            };
            (totals.start, session)
        })
        .collect();
    result.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.session.cmp(&b.1.session)));
    result.into_iter().map(|(_, session)| session).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::scan;
    use crate::test_support::{Workspace, prices};
    use serde_json::json;
    use std::path::Path;

    fn write(path: &Path, records: &[Value], tail: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text: String = records.iter().map(|r| format!("{r}\n")).collect();
        text.push_str(tail);
        std::fs::write(path, text).unwrap();
    }

    /// A Claude reply billed 100 uncached, 1000 read, 500 written for 5m, and 50 output
    /// tokens: $0.0041 on the synthetic Opus prices.
    fn reply(session: &str, id: &str, time: &str, branch: &str, effort: &str) -> Value {
        json!({"type": "assistant", "uuid": format!("uuid-{id}"), "sessionId": session,
            "timestamp": time, "version": "2.1.322", "cwd": "/synthetic/project",
            "gitBranch": branch, "effort": effort,
            "message": {"id": format!("message-{id}"), "model": "claude-opus-5-5",
                "content": [{"type": "text", "text": "SYNTHETIC_PRIVATE_REPLY"}],
                "usage": {"input_tokens": 100, "output_tokens": 50,
                    "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 500,
                    "cache_creation": {"ephemeral_5m_input_tokens": 500,
                        "ephemeral_1h_input_tokens": 0},
                    "service_tier": "standard", "speed": "standard",
                    "inference_geo": "global",
                    "server_tool_use": {"web_search_requests": 0}}}})
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn claude_subagents_and_copies_are_their_own_sessions_and_count_once() {
        let w = Workspace::new();
        let project = w.claude.join("projects/project");
        let first = reply("session-a", "a1", "2026-11-01T00:01:00Z", "main", "high");
        write(
            &project.join("session-a.jsonl"),
            &[
                // Before the window: neither its cost nor its branch counts.
                reply("session-a", "a0", "2026-10-31T00:00:00Z", "old", "high"),
                json!({"type": "user", "sessionId": "session-a", "gitBranch": "main",
                    "timestamp": "2026-11-01T00:00:00Z", "cwd": "/synthetic/project",
                    "message": {"role": "user", "content": "SYNTHETIC_PRIVATE_PROMPT"}}),
                first.clone(),
                reply("session-a", "a2", "2026-11-01T00:02:00Z", "feature", "low"),
            ],
            "{\"type\":\"assistant\",\"truncated\n",
        );
        let mut subagent = reply("session-a", "s1", "2026-11-01T00:03:00Z", "quest/a", "high");
        subagent["agentId"] = json!("x1");
        subagent["isSidechain"] = json!(true);
        write(
            &project.join("session-a/subagents/agent-x1.jsonl"),
            &[subagent],
            "",
        );
        // A fork copies the conversation so far, then continues as its own session.
        write(
            &project.join("session-b.jsonl"),
            &[
                first,
                reply("session-b", "b1", "2026-11-01T00:04:00Z", "main", "high"),
            ],
            "",
        );
        let mut options = w.scan_options();
        options.since = Some(1_793_491_200_000);
        let sessions = export(&scan(&options).unwrap(), &prices());
        let ids: Vec<_> = sessions.iter().map(|s| s.session.as_deref()).collect();
        assert_eq!(ids, [Some("session-a"), Some("x1"), Some("session-b")]);

        let main = &sessions[0];
        assert_eq!(main.harness, Harness::Claude);
        assert_eq!(main.parent, None);
        assert_eq!(main.cwd.as_deref(), Some("/synthetic/project"));
        let span = |name: &str, first: &str, last: &str| BranchSpan {
            name: name.into(),
            first: format!("2026-11-01T00:{first}:00.000Z"),
            last: format!("2026-11-01T00:{last}:00.000Z"),
        };
        assert_eq!(
            main.branches,
            [span("main", "00", "01"), span("feature", "02", "02")]
        );
        assert_eq!(
            (&*main.start, &*main.end),
            ("2026-11-01T00:01:00.000Z", "2026-11-01T00:02:00.000Z")
        );
        let efforts: Vec<_> = main.models.iter().map(|m| m.effort.as_deref()).collect();
        assert_eq!(efforts, [Some("high"), Some("low")]);
        let high = &main.models[0];
        assert_eq!(high.model.as_deref(), Some("claude-opus-5-5"));
        let tokens = |m: &ModelUsage| {
            (
                m.requests,
                m.input_tokens,
                m.cache_read_tokens,
                m.cache_write_tokens,
                m.output_tokens,
            )
        };
        assert_eq!(tokens(high), (1, 100, 1000, 500, 50));
        assert!(high.complete && close(high.cost_usd, 0.0041));
        assert!(close(main.cost_usd, 0.0082));
        // The truncated line may have held usage, so the total is only a lower bound.
        assert_eq!(main.skipped_lines, 1);
        assert!(!main.complete);
        assert_eq!(
            main.files.len(),
            2,
            "the fork's copy is listed with the original"
        );

        let sub = &sessions[1];
        assert_eq!(sub.parent.as_deref(), Some("session-a"));
        assert_eq!(sub.branches[0].name, "quest/a");
        assert!(sub.complete && close(sub.cost_usd, 0.0041));

        let fork = &sessions[2];
        assert_eq!(
            fork.models[0].requests, 1,
            "the copied reply stays with session-a"
        );
        assert!(fork.complete && close(fork.cost_usd, 0.0041));
    }

    #[test]
    fn codex_review_threads_name_their_parent_and_effort() {
        let w = Workspace::new();
        let thread = |id: &str, source: Value, root: Option<&str>| {
            let mut meta = json!({"type": "session_meta", "timestamp": "2026-11-01T00:00:00Z",
                "payload": {"id": id, "cwd": "/synthetic/project", "source": source,
                    "model_provider": "openai", "git": {"branch": "quest/m0/a"}}});
            if let Some(root) = root {
                meta["payload"]["session_id"] = json!(root);
            }
            let context = json!({"type": "turn_context", "timestamp": "2026-11-01T00:00:01Z",
                "payload": {"turn_id": format!("turn-{id}"), "model": "gpt-6.1-sol",
                    "effort": "medium"}});
            let usage = json!({"type": "token_usage_record",
                "timestamp": "2026-11-01T00:00:02Z",
                "payload": {"response_id": format!("response-{id}"), "usage": {
                    "input_tokens": 1000, "cached_input_tokens": 200,
                    "cache_write_input_tokens": 0, "output_tokens": 100}}});
            write(
                &w.codex.join(format!("sessions/rollout-{id}.jsonl")),
                &[meta, context, usage],
                "",
            );
        };
        thread("root", json!("exec"), None);
        thread("review", json!({"subagent": "review"}), Some("root"));
        let sessions = export(&scan(&w.scan_options()).unwrap(), &prices());
        let found: Vec<_> = sessions
            .iter()
            .map(|s| (s.session.as_deref(), s.parent.as_deref(), s.review))
            .collect();
        assert_eq!(
            found,
            [
                (Some("review"), Some("root"), true),
                (Some("root"), None, false)
            ]
        );
        for s in &sessions {
            assert_eq!(s.harness, Harness::Codex);
            assert_eq!(s.branches[0].name, "quest/m0/a");
            let m = &s.models[0];
            let found = (
                m.model.as_deref(),
                m.effort.as_deref(),
                m.input_tokens,
                m.cache_read_tokens,
                m.output_tokens,
            );
            assert_eq!(found, (Some("gpt-6.1-sol"), Some("medium"), 800, 200, 100));
            assert!(s.complete && s.cost_usd > 0.0);
        }
    }

    #[test]
    fn a_codex_forks_copied_requests_stay_with_its_source_whichever_is_read_first() {
        let usage = |id: &str, time: &str| {
            json!({"type": "token_usage_record", "timestamp": time,
                "payload": {"response_id": id, "turn_id": "turn", "usage": {
                    "input_tokens": 1000, "cached_input_tokens": 0,
                    "cache_write_input_tokens": 0, "output_tokens": 100}}})
        };
        let context = json!({"type": "turn_context", "timestamp": "2026-11-01T00:00:00Z",
            "payload": {"turn_id": "turn", "model": "gpt-6.1-sol"}});
        let copied = usage("response-a", "2026-11-01T00:00:01Z");
        // A fork begins after its source, or records its lineage when the times tie.
        for (forked_at, lineage) in [("01:00", None), ("00:00", Some("source"))] {
            let w = Workspace::new();
            let meta = |id: &str, time: &str, source: Option<&str>| {
                json!({"type": "session_meta", "timestamp": format!("2026-11-01T{time}:00Z"),
                    "payload": {"id": id, "source": "exec", "model_provider": "openai",
                        "forked_from_id": source}})
            };
            // The fork's rollout sorts first, so the reader meets the copy before the original.
            write(
                &w.codex.join("sessions/a-fork.jsonl"),
                &[
                    meta("fork", forked_at, lineage),
                    context.clone(),
                    copied.clone(),
                    usage("response-b", "2026-11-01T01:00:01Z"),
                ],
                "",
            );
            write(
                &w.codex.join("sessions/b-source.jsonl"),
                &[
                    meta("source", "00:00", None),
                    context.clone(),
                    copied.clone(),
                ],
                "",
            );
            let sessions = export(&scan(&w.scan_options()).unwrap(), &prices());
            let found: Vec<_> = sessions
                .iter()
                .map(|s| (s.session.as_deref(), s.models[0].requests, &*s.start))
                .collect();
            assert_eq!(
                found,
                [
                    (Some("source"), 1, "2026-11-01T00:00:01.000Z"),
                    (Some("fork"), 1, "2026-11-01T01:00:01.000Z"),
                ],
                "{forked_at}"
            );
        }
    }

    #[test]
    fn a_claude_reply_takes_its_api_requests_effort_when_it_records_none() {
        let w = Workspace::new();
        let mut reply = reply("session", "r", "2026-11-01T00:00:01Z", "main", "unused");
        reply.as_object_mut().unwrap().remove("effort");
        reply["requestRef"] = json!("api-r");
        write(
            &w.claude.join("projects/project/session.jsonl"),
            &[
                json!({"type": "api-request", "id": "api-r", "sessionId": "session",
                    "timestamp": "2026-11-01T00:00:00Z",
                    "params": {"model": "claude-opus-5-5",
                        "output_config": {"effort": "max"}}}),
                reply,
            ],
            "",
        );
        let sessions = export(&scan(&w.scan_options()).unwrap(), &prices());
        assert_eq!(sessions[0].models[0].effort.as_deref(), Some("max"));
    }
}
