//! Antigravity: where its CLI (`agy`) and its ACP server keep their history, and the requests
//! their transcripts record.
//!
//! Both keep their state under `GEMINI_HOME` (`~/.gemini`): the CLI in `antigravity-cli/`,
//! the ACP server T3 Code runs in `antigravity-acp/`. Each conversation keeps a transcript of
//! its steps, `brain/<id>/.system_generated/logs/transcript.jsonl`, and every step that called
//! a model records its model and token counts there. Only each step's type, index, time, model
//! and counts are read; its prompt, thinking, reply and tool text are skipped unread.
//! `conversation_summaries.db` names each conversation's workspace and, for a subagent, the
//! conversation that started it and its agent.

use std::io::BufRead;
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::Value;

use crate::collections::{HashMap, IndexMap};
use crate::controls::Env;
use crate::discovery::{absent, error, join, resolve};
use crate::model::{
    Agent, AgentControls, Api, BillingPart, CompactionControls, Few, Harness, Location, Millis,
    PartKind, PerBucket, Profile, Request, RequestEvidence, RequestMeta, Snapshot, Subagent, Text,
    TtlControl, Usage, WindowControl,
};
use crate::safe::{ScanError, count, identity, label, parse_instant};
use crate::sessions::Native;

/// The CLI's directory under `GEMINI_HOME`.
pub(crate) const CLI: &str = "antigravity-cli";

/// The ACP server's directory under its `GEMINI_HOME`, which T3 Code gives each instance.
pub(crate) const ACP: &str = "antigravity-acp";

/// The models.dev provider every Antigravity model is priced under.
const PROVIDER: &str = "google";

/// The reasoning efforts a recorded model slug can end in, such as `gemini-4-neon-high`.
const EFFORTS: [&str; 5] = ["minimal", "low", "medium", "high", "xhigh"];

/// The database that names each conversation's workspace and parent, in the root.
const SUMMARIES: &str = "conversation_summaries.db";

/// What an Antigravity request's pricing assumes, since Antigravity records no tier.
const STANDARD_TIER: &str =
    "Antigravity records no service tier; priced at Google's standard catalog rate";

/// Why a request with no recorded model is priced at its conversation's.
const CACHED_MODEL: &str = "Antigravity recorded no model for this request; it read the conversation's cache, which Gemini keeps per model, and every earlier request in the conversation ran one model, so it is priced at that model";

/// `$GEMINI_HOME`, else `~/.gemini`.
pub(crate) fn gemini_home(home: &str, env: &Env) -> String {
    match env.get("GEMINI_HOME").filter(|d| !d.is_empty()) {
        Some(dir) => resolve(dir),
        None => join(&[home, ".gemini"]),
    }
}

/// The CLI's root, which holds its conversations.
pub(crate) fn cli_root(home: &str, env: &Env) -> String {
    join(&[&gemini_home(home, env), CLI])
}

/// Every conversation's transcript under `root`, in path order.
fn transcripts(root: &str) -> Result<Vec<String>, ScanError> {
    let brain = join(&[root, "brain"]);
    let entries = match std::fs::read_dir(&brain) {
        Ok(entries) => entries,
        Err(e) if absent(e.kind()) => return Ok(vec![]),
        Err(_) => return Err(error("cannot list Antigravity conversations", &brain, 0)),
    };
    let mut files = vec![];
    for entry in entries {
        let entry = entry.map_err(|_| error("cannot list Antigravity conversations", &brain, 0))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let file = join(&[
            &brain,
            &name,
            ".system_generated",
            "logs",
            "transcript.jsonl",
        ]);
        match std::fs::metadata(&file) {
            Ok(info) if info.is_file() => files.push(file),
            Ok(_) => {}
            Err(e) if absent(e.kind()) => {}
            Err(_) => return Err(error("cannot inspect Antigravity transcript", &file, 0)),
        }
    }
    files.sort();
    Ok(files)
}

/// One Antigravity root's profile: its transcripts, and no setting token-max changes, since
/// no record says what Antigravity sent besides the conversation.
pub(crate) fn profile(canonical: String, alias: String, key: &str) -> Result<Profile, ScanError> {
    let histories = transcripts(&canonical)?;
    let unset = |reason: &str| TtlControl {
        value: None,
        owner: String::new(),
        reason: reason.into(),
        varies: false,
    };
    Ok(Profile {
        id: identity(key),
        harness: Harness::Antigravity,
        settings_file: join(&[&canonical, "settings.json"]),
        root: canonical,
        aliases: vec![alias],
        histories,
        ttl: PerBucket {
            main: unset("Antigravity's cache lifetime is not replayed"),
            subagent: unset("Antigravity's cache lifetime is not replayed"),
        },
        provider: None,
        billing: None,
        settings: Default::default(),
        mcp_servers: vec![],
        compaction: CompactionControls {
            default: WindowControl {
                value: None,
                owner: String::new(),
                reason: "Antigravity's compaction is not replayed".into(),
                applicable: false,
                minimum_claude_patch: None,
                comparison_unknown: None,
                unset: true,
                setting: None,
            },
            models: IndexMap::default(),
            capped: false,
            declared: None,
            tuned: IndexMap::default(),
            contexts: IndexMap::default(),
            percent: None,
            layers: None,
        },
        idle: None,
        speed: None,
        bash: None,
        retention: None,
        turn_end: None,
        effort: None,
        advisor: None,
        allowlist: None,
        project_files: vec![],
        speed_directories: None,
        idle_directories: None,
        background: vec![],
        background_directories: IndexMap::default(),
        background_requests: IndexMap::default(),
        memory: None,
        inbound: None,
        t3: None,
        plugins: None,
        skill: None,
        agents: AgentControls::default(),
        instruction_budget_file: None,
        instruction_budget_unresolved: None,
        outranking_files: vec![],
        ttl_directories: IndexMap::default(),
        claude_config: None,
        updater: None,
        codex_system_config: None,
        switched_off: vec![],
        pretended_on: vec![],
        policy_off: vec![],
    })
}

/// One root's requests, conversations and subagents, ready for the reader.
#[derive(Debug, Default)]
pub(crate) struct History {
    pub requests: Vec<Request>,
    /// Each conversation, with its transcript.
    pub sessions: Vec<(Native, Text)>,
    pub subagents: Vec<Subagent>,
    /// Each transcript as read, with the unfinished last line it left unread.
    pub snapshots: Vec<Snapshot>,
}

/// What `conversation_summaries.db` says about one conversation.
#[derive(Debug, Default, PartialEq)]
struct Summary {
    cwd: Option<Text>,
    parent: Option<String>,
    agent: Option<String>,
}

/// The local path a `file://` URI names, when it is absolute and UTF-8.
fn file_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut path = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            path.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            path.push(bytes[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(path).ok()?;
    // `file:///C:/work` names `C:/work` on Windows.
    let path = match path.strip_prefix('/') {
        Some(drive) if cfg!(windows) && drive.as_bytes().get(1) == Some(&b':') => drive.into(),
        _ => path,
    };
    crate::safe::is_absolute(&path).then_some(path)
}

/// Why the summaries could not be read, without their contents.
fn unreadable(file: &str, e: rusqlite::Error) -> ScanError {
    let reason = match e {
        rusqlite::Error::SqliteFailure(e, _)
            if matches!(
                e.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            "Antigravity conversation summaries are locked"
        }
        rusqlite::Error::SqliteFailure(_, _) => "cannot read Antigravity conversation summaries",
        _ => "unexpected Antigravity conversation summaries shape",
    };
    error(reason, file, 0)
}

/// Each conversation's workspace, parent and agent, by conversation ID; empty when the root
/// keeps no summaries.
fn summaries(root: &str) -> Result<HashMap<String, Summary>, ScanError> {
    let file = join(&[root, SUMMARIES]);
    match std::fs::metadata(&file) {
        Ok(_) => {}
        Err(e) if absent(e.kind()) => return Ok(HashMap::default()),
        Err(_) => {
            return Err(error(
                "cannot inspect Antigravity conversation summaries",
                &file,
                0,
            ));
        }
    }
    let fail = |e| unreadable(&file, e);
    let mut db = Connection::open_with_flags(
        &file,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(fail)?;
    // Antigravity writes while it runs; a reader waits out a write briefly, then fails loud.
    db.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(fail)?;
    db.execute_batch("PRAGMA query_only=ON; PRAGMA trusted_schema=OFF;")
        .map_err(fail)?;
    let tx = db.transaction().map_err(fail)?;
    let mut found = HashMap::default();
    {
        // Titles, previews and summaries stay in SQLite.
        let mut statement = tx
            .prepare(
                "SELECT conversation_id, workspace_uris, agent_name, parent_conversation_id
                 FROM conversation_summaries",
            )
            .map_err(fail)?;
        let mut rows = statement.query([]).map_err(fail)?;
        while let Some(row) = rows.next().map_err(fail)? {
            let Some(id) = row.get::<_, Option<String>>(0).map_err(fail)? else {
                continue;
            };
            let uris: Option<String> = row.get(1).map_err(fail)?;
            let cwd = uris
                .and_then(|u| serde_json::from_str::<Vec<String>>(&u).ok())
                .and_then(|u| u.first().and_then(|u| file_path(u)))
                .map(Text::from);
            let name = |index| -> Result<Option<String>, ScanError> {
                let text: Option<String> = row.get(index).map_err(fail)?;
                Ok(text.and_then(|t| label(&Value::from(t))))
            };
            found.insert(
                id,
                Summary {
                    cwd,
                    agent: name(2)?,
                    parent: name(3)?,
                },
            );
        }
    }
    tx.rollback().map_err(fail)?;
    Ok(found)
}

/// One transcript step, by the fields token-max reads; its text is skipped unread.
#[derive(Deserialize)]
struct Step {
    #[serde(rename = "type")]
    kind: Option<String>,
    step_index: Option<u64>,
    created_at: Option<String>,
    model: Option<String>,
    input_tokens: Option<Value>,
    cache_read_tokens: Option<Value>,
    output_tokens: Option<Value>,
}

/// A recorded model slug as its model and effort: `gemini-4-neon-high` is `gemini-4-neon` at
/// `high`.
fn split_model(slug: &str) -> (String, Option<String>) {
    match slug.rsplit_once('-') {
        Some((model, effort)) if !model.is_empty() && EFFORTS.contains(&effort) => {
            (model.into(), Some(effort.into()))
        }
        _ => (slug.into(), None),
    }
}

/// The usage Antigravity records, normalized: its input excludes cache reads, which are
/// added back, and its output already counts thinking. Gemini's implicit cache writes
/// nothing billed.
fn usage(input: Option<i64>, read: Option<i64>, output: Option<i64>) -> Usage {
    Usage {
        input: input.zip(read).map(|(input, read)| input + read),
        output,
        read,
        write: Some(0),
        ..Usage::default()
    }
}

/// The conversation a transcript belongs to: the `brain/<id>` directory it is under.
fn conversation(file: &str) -> Option<&str> {
    let mut parts = file.rsplit(['/', '\\']);
    let _ = (parts.next()?, parts.next()?, parts.next()?);
    parts.next().filter(|id| !id.is_empty())
}

/// Every request, conversation and subagent `root`'s transcripts record. `profiles` are the
/// profiles that read the root.
pub(crate) fn read(
    root: &str,
    files: &[String],
    profiles: &[String],
) -> Result<History, ScanError> {
    let summaries = summaries(root)?;
    let profiles: crate::model::Profiles = profiles.into();
    let mut history = History::default();
    for file in files {
        let Some(id) = conversation(file) else {
            return Err(error("unexpected Antigravity transcript path", file, 0));
        };
        let summary = summaries.get(id);
        let key: Text = identity(id).into();
        let parent = summary.and_then(|s| s.parent.clone());
        let agent = summary.and_then(|s| s.agent.clone());
        let cwd = summary.and_then(|s| s.cwd.clone());
        let shared = RequestMeta {
            harness: Harness::Antigravity,
            session: key.clone(),
            agent: if parent.is_some() {
                Agent::Subagent
            } else {
                Agent::Main
            },
            mode: agent.as_deref().map(Text::from),
            model: None,
            provider: Some(PROVIDER.into()),
            api: Api::Unknown,
            source: None,
            trigger: None,
            version: None,
            client: None,
            cwd: cwd.clone(),
            effort: None,
            server_tools: None,
            profiles: profiles.clone(),
            attempt: None,
            requested_speed: None,
            issues: vec![STANDARD_TIER.into()],
        };
        let (created, snapshot) = read_transcript(file, &shared, &mut history.requests)?;
        history.snapshots.push(snapshot);
        if let Some(parent) = &parent {
            history.subagents.push(Subagent {
                harness: Harness::Antigravity,
                session: key.clone(),
                parent: Some(identity(parent).into()),
                kind: agent.unwrap_or_else(|| crate::reader::UNRECORDED.into()),
                built_in: None,
                fork: false,
                native: false,
                model: None,
                effort: None,
            });
        }
        let native = Native {
            key,
            id: id.into(),
            parent,
            cwd,
            version: None,
            created: created.unwrap_or(0),
        };
        history.sessions.push((native, file.as_str().into()));
    }
    Ok(history)
}

/// Add one transcript's requests to `requests`, each sharing `shared` but for its model,
/// effort and source, and return when the conversation began and what was read.
fn read_transcript(
    file: &str,
    shared: &RequestMeta,
    requests: &mut Vec<Request>,
) -> Result<(Option<Millis>, Snapshot), ScanError> {
    let handle = std::fs::File::open(file)
        .map_err(|_| error("cannot read Antigravity transcript", file, 0))?;
    let unreadable = |_| error("cannot read Antigravity transcript", file, 0);
    let (device, inode) = crate::reader::file_identity(&handle).map_err(unreadable)?;
    let bytes = handle.metadata().map_err(unreadable)?.len();
    let reader = std::io::BufReader::new(handle);
    let mut snapshot = Snapshot {
        file: file.into(),
        harness: Harness::Antigravity,
        profiles: shared.profiles.iter().map(|p| p.to_string()).collect(),
        bytes,
        device,
        inode,
        deferred_bytes: 0,
        skipped_lines: 0,
        unreadable: Default::default(),
        lines: 0,
    };
    let location_file: Text = file.into();
    let mut created: Option<Millis> = None;
    // The one model every recorded request so far ran, or None once two differ.
    let mut only: Option<Option<String>> = None;
    let mut lines = reader.split(b'\n').enumerate().peekable();
    while let Some((index, line)) = lines.next() {
        let line_number = index as u64 + 1;
        let line =
            line.map_err(|_| error("cannot read Antigravity transcript", file, line_number))?;
        let last = lines.peek().is_none();
        snapshot.lines += 1;
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let step: Step = match serde_json::from_slice(&line) {
            Ok(step) => step,
            // Antigravity may be writing the last line as it is read: it is left for a
            // later read, and leaves the conversation's export incomplete.
            Err(_) if last => {
                snapshot.lines -= 1;
                snapshot.deferred_bytes = line.len() as u64;
                break;
            }
            Err(_) => {
                return Err(error(
                    "invalid Antigravity transcript line",
                    file,
                    line_number,
                ));
            }
        };
        let invalid = |_| error("invalid Antigravity token count", file, line_number);
        let time = step.created_at.as_deref().and_then(parse_instant);
        if let Some(time) = time {
            created = Some(created.map_or(time, |c| c.min(time)));
        }
        let input = count(step.input_tokens.as_ref()).map_err(invalid)?;
        let read = count(step.cache_read_tokens.as_ref()).map_err(invalid)?;
        let output = count(step.output_tokens.as_ref()).map_err(invalid)?;
        if input.is_none() && read.is_none() && output.is_none() {
            continue;
        }
        let recorded = step
            .model
            .as_deref()
            .and_then(|m| label(&Value::from(m)))
            .map(|m| split_model(&m));
        let mut meta = shared.clone();
        let model = match recorded {
            Some((model, effort)) => {
                only = match only {
                    None => Some(Some(model.clone())),
                    Some(Some(previous)) if previous == model => Some(Some(previous)),
                    Some(_) => Some(None),
                };
                meta.effort = effort.map(Text::from);
                Some(model)
            }
            // Gemini caches per model, so a read of this conversation's cache came from the
            // model every earlier request ran.
            None => match (&only, read) {
                (Some(Some(model)), Some(read)) if read > 0 => {
                    meta.issues.push(CACHED_MODEL.into());
                    Some(model.clone())
                }
                // A request on an unknown model could have filled the cache a later one reads.
                _ => {
                    only = Some(None);
                    None
                }
            },
        };
        meta.model = model.map(Text::from);
        let compaction = step.kind.as_deref() == Some("CHECKPOINT");
        if compaction {
            meta.source = Some("compaction".into());
        }
        let step_index = step.step_index.unwrap_or(index as u64);
        let time = time.or(created).unwrap_or(0);
        requests.push(Request {
            evidence: RequestEvidence::Measured,
            id: format!("antigravity:{}:{step_index}", shared.session).into(),
            time,
            end_time: time,
            reference: None,
            turn: None,
            parts: vec![BillingPart {
                kind: if compaction {
                    PartKind::Compaction
                } else {
                    PartKind::Message
                },
                model: meta.model.clone(),
                usage: usage(input, read, output),
                tier: Some("standard".into()),
                speed: None,
                geo: None,
            }],
            locations: Few::One(Location {
                file: location_file.clone(),
                line: line_number,
            }),
            t3: None,
            meta: Arc::new(meta),
        });
    }
    Ok((created, snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_splits_off_only_a_known_effort() {
        assert_eq!(
            split_model("gemini-4-neon-high"),
            ("gemini-4-neon".into(), Some("high".into()))
        );
        assert_eq!(
            split_model("gemini-4-carbon-xhigh"),
            ("gemini-4-carbon".into(), Some("xhigh".into()))
        );
        assert_eq!(split_model("gemini-4-neon"), ("gemini-4-neon".into(), None));
        assert_eq!(
            split_model("gemini-2.5-pro"),
            ("gemini-2.5-pro".into(), None)
        );
        assert_eq!(split_model("-high"), ("-high".into(), None));
    }

    #[test]
    fn workspace_uris_name_absolute_decoded_paths() {
        if cfg!(windows) {
            assert_eq!(
                file_path("file:///C:/my%20work").as_deref(),
                Some("C:/my work")
            );
        } else {
            assert_eq!(
                file_path("file:///my%20work/repo").as_deref(),
                Some("/my work/repo")
            );
        }
        assert_eq!(file_path("https://example.com/x"), None);
        assert_eq!(file_path("file://relative"), None);
        assert_eq!(file_path("file:///bad%zz"), None);
        assert_eq!(file_path("file:///bad%ff"), None);
    }

    #[test]
    fn a_transcript_names_its_conversation() {
        let file =
            "/home/u/.gemini/antigravity-cli/brain/abc/.system_generated/logs/transcript.jsonl";
        assert_eq!(conversation(file), Some("abc"));
        assert_eq!(conversation("transcript.jsonl"), None);
    }

    /// The synthetic root's history, read as one profile.
    fn synthetic(w: &crate::test_support::Workspace) -> (String, History) {
        let root = w.home.join(".gemini").join(CLI);
        crate::test_support::antigravity_root(&root);
        let root = root.to_string_lossy().into_owned();
        let profile = profile(root.clone(), root.clone(), "key").unwrap();
        assert_eq!(profile.histories.len(), 2);
        let history = read(&root, &profile.histories, std::slice::from_ref(&profile.id)).unwrap();
        (root, history)
    }

    #[test]
    fn transcripts_become_priced_requests_conversations_and_subagents_without_text() {
        let w = crate::test_support::Workspace::new();
        let (_, history) = synthetic(&w);
        assert!(!format!("{history:?}").contains("SYNTHETIC_PRIVATE"));
        let catalog = crate::test_support::prices();
        let cost = |r: &Request| crate::prices::price_request(&catalog, r).total_usd;
        let main = identity("main-id");
        let requests: Vec<&Request> = history
            .requests
            .iter()
            .filter(|r| *r.session == *main)
            .collect();
        // The user step records no usage.
        assert_eq!(requests.len(), 3);
        let first = requests[0];
        assert_eq!(first.model.as_deref(), Some("gemini-4-neon"));
        assert_eq!(first.effort.as_deref(), Some("high"));
        assert_eq!(first.agent, Agent::Main);
        assert_eq!(first.provider.as_deref(), Some(PROVIDER));
        // Recorded input is uncached; the normalized input adds the cache reads back.
        let second = &requests[1].parts[0].usage;
        assert_eq!(
            (second.input, second.read, second.write, second.output),
            (Some(1_500), Some(1_000), Some(0), Some(50))
        );
        // 500 uncached at $3, 1,000 cached at $0.15 and 50 output at $15 per million.
        assert!((cost(requests[1]).unwrap().to_f64() - 0.0024).abs() < 1e-12);
        // The checkpoint read the cache only gemini-4-neon held.
        let checkpoint = requests[2];
        assert_eq!(checkpoint.parts[0].kind, PartKind::Compaction);
        assert_eq!(checkpoint.source.as_deref(), Some("compaction"));
        assert_eq!(checkpoint.model.as_deref(), Some("gemini-4-neon"));
        assert_eq!(checkpoint.effort, None);
        assert!(checkpoint.issues.iter().any(|i| &**i == CACHED_MODEL));
        assert!(cost(checkpoint).is_some());
        let sub = history
            .requests
            .iter()
            .find(|r| *r.session == *identity("sub-id"))
            .unwrap();
        assert_eq!(sub.agent, Agent::Subagent);
        assert_eq!(sub.mode.as_deref(), Some("research"));
        assert_eq!(sub.effort.as_deref(), Some("medium"));
        assert_eq!(history.subagents.len(), 1);
        let subagent = &history.subagents[0];
        assert_eq!(subagent.kind, "research");
        assert_eq!(subagent.parent.as_deref(), Some(&*main));
        let (session, file) = history
            .sessions
            .iter()
            .find(|(s, _)| s.id == "sub-id")
            .unwrap();
        assert_eq!(session.parent.as_deref(), Some("main-id"));
        assert!(file.ends_with("transcript.jsonl"));
        if cfg!(unix) {
            assert_eq!(session.cwd.as_deref(), Some("/work/repo"));
        }
    }

    #[test]
    fn a_request_without_a_model_is_priced_only_when_the_conversations_cache_proves_it() {
        let w = crate::test_support::Workspace::new();
        let (root, _) = synthetic(&w);
        let file = |id: &str| {
            join(&[
                &root,
                "brain",
                id,
                ".system_generated",
                "logs",
                "transcript.jsonl",
            ])
        };
        let step = |model: Option<&str>, read: u64| {
            serde_json::json!({"type": "PLANNER_RESPONSE", "created_at": "2026-01-01T00:00:00Z",
                "model": model, "input_tokens": 10, "cache_read_tokens": read,
                "output_tokens": 1})
            .to_string()
        };
        // A first request with no model has no cache to prove one.
        let cold = [step(None, 0)].join("\n");
        // Two models ran, so a cache read proves neither.
        let mixed = [
            step(Some("gemini-4-neon-high"), 0),
            step(Some("gemini-4-carbon-high"), 0),
            step(None, 100),
        ]
        .join("\n");
        // A request on an unknown model could have filled the cache the next one reads.
        let gap = [
            step(Some("gemini-4-neon-high"), 0),
            step(None, 0),
            step(None, 100),
        ]
        .join("\n");
        let ids = ["cold", "mixed", "gap"];
        for (id, text) in ids.into_iter().zip([cold, mixed, gap]) {
            std::fs::create_dir_all(crate::discovery::dirname(&file(id))).unwrap();
            std::fs::write(file(id), text).unwrap();
        }
        let files: Vec<String> = ids.into_iter().map(file).collect();
        let history = read(&root, &files, &["p".into()]).unwrap();
        let unknown: Vec<&Request> = history
            .requests
            .iter()
            .filter(|r| r.model.is_none())
            .collect();
        assert_eq!(unknown.len(), 4);
        let catalog = crate::test_support::prices();
        for r in unknown {
            assert_eq!(crate::prices::price_request(&catalog, r).total_usd, None);
        }
    }

    #[test]
    fn gemini_home_moves_the_cli_and_a_missing_one_is_skipped() {
        let w = crate::test_support::Workspace::new();
        let moved = w.home.join("gemini");
        crate::test_support::antigravity_root(&moved.join(CLI));
        let antigravity = |options: &crate::discovery::DiscoveryOptions| -> Vec<String> {
            crate::discovery::discover(options)
                .unwrap()
                .profiles
                .into_iter()
                .filter(|p| p.harness == Harness::Antigravity)
                .map(|p| p.root)
                .collect()
        };
        let mut options = w.scan_options().discovery;
        options
            .env
            .insert("GEMINI_HOME".into(), moved.to_string_lossy().into_owned());
        assert_eq!(
            antigravity(&options),
            [moved.join(CLI).to_string_lossy().into_owned()]
        );
        // Gemini's other tools set it too, so a home without the CLI fails nothing.
        options.env.insert(
            "GEMINI_HOME".into(),
            w.home.join("absent").to_string_lossy().into_owned(),
        );
        assert!(antigravity(&options).is_empty());
        // `--defaults` still finds the history the moved home holds.
        options
            .env
            .insert("GEMINI_HOME".into(), moved.to_string_lossy().into_owned());
        let mut defaults = options.clone();
        defaults.pretend_defaults();
        assert_eq!(antigravity(&defaults), antigravity(&options));
        // The audit reads no Antigravity history.
        options.unreplayed = false;
        assert!(antigravity(&options).is_empty());
    }

    #[test]
    fn only_a_transcripts_last_line_may_be_unfinished() {
        let w = crate::test_support::Workspace::new();
        let (root, _) = synthetic(&w);
        let file = join(&[
            &root,
            "brain",
            "main-id",
            ".system_generated",
            "logs",
            "transcript.jsonl",
        ]);
        let text = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, format!("{text}{{\"type\":\"PLANNER_RES")).unwrap();
        let history = read(&root, std::slice::from_ref(&file), &["p".into()]).unwrap();
        assert_eq!(history.requests.len(), 3);
        // Left for a later read, it leaves the conversation's export incomplete.
        let deferred = "{\"type\":\"PLANNER_RES".len() as u64;
        assert_eq!(history.snapshots[0].deferred_bytes, deferred);
        std::fs::write(&file, &text).unwrap();
        let finished = read(&root, std::slice::from_ref(&file), &["p".into()]).unwrap();
        assert_eq!(finished.snapshots[0].deferred_bytes, 0);
        std::fs::write(&file, format!("{{\"type\":\n{text}")).unwrap();
        let error = read(&root, std::slice::from_ref(&file), &["p".into()]).unwrap_err();
        assert_eq!(error.message, "invalid Antigravity transcript line");
        assert_eq!(error.location.map(|l| l.line), Some(1));
    }
}
