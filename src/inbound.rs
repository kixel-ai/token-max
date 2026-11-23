//! What the turns that messages from your other Claude Code sessions start cost, beside the
//! `crossSessionInbound` setting that holds or refuses them. Evidence only: the messages are
//! how sessions coordinate, so a high cost per message is no reason to stop them.

use crate::collections::{HashMap, IndexMap};
use crate::model::{
    EventKind, Harness, InboundControl, InboundUse, Millis, Profile, Request, Scan, StructuralEvent,
};
use crate::prices::{Catalog, Sample, Sum, price_request};
use crate::rows::Group;

/// One line per Claude `crossSessionInbound` owner and value, with the deliveries from
/// `since`, else the scan's start, to `now`.
pub fn inbound_use(
    scan: &Scan,
    catalog: &Catalog,
    since: Option<Millis>,
    now: Millis,
) -> Vec<InboundUse> {
    // A T3 instance whose launch arguments may decide it stays apart from a resolved owner.
    let owner = |p: &Profile| {
        let c = p.inbound.as_ref().expect("filtered on an inbound control");
        format!(
            "{}:crossSessionInbound\n{:?}\n{}",
            c.file,
            c.value,
            t3_unresolved(p)
        )
    };
    let controlled = scan
        .profiles
        .iter()
        .filter(|p| p.harness == Harness::Claude && p.inbound.is_some());
    let window = |time: Millis| since.is_none_or(|s| time >= s) && time <= now;
    let turns = Turns::new(scan, &window);
    crate::rows::groups(controlled, owner)
        .iter()
        .map(|line| line_use(scan, catalog, &turns, &window, line))
        .collect()
}

/// The distinct sessions a record was read in: a forked transcript copies records under
/// its own session, and the copy read first names the record's `session`.
fn sessions(e: &StructuralEvent) -> Vec<&str> {
    let mut found = vec![&*e.session];
    for o in &e.origins {
        if !found.contains(&&*o.session) {
            found.push(&o.session);
        }
    }
    found
}

/// Each session's turn starts in order, and its subagents by their first request.
struct Turns<'a> {
    /// Every record that starts a turn, by each session it was read in, in time order.
    starts: HashMap<&'a str, Vec<&'a StructuralEvent>>,
    /// Each session's billed requests in time order, its main conversation's and any
    /// recorded in it.
    requests: HashMap<&'a str, Vec<&'a Request>>,
    /// Each session's subagent conversations, with when each first sent a request, even
    /// before the window.
    subagents: HashMap<&'a str, Vec<(Millis, &'a str)>>,
}

impl<'a> Turns<'a> {
    fn new(scan: &'a Scan, window: &dyn Fn(Millis) -> bool) -> Self {
        let mut starts: HashMap<&str, Vec<&StructuralEvent>> = HashMap::default();
        for e in scan.events.iter().filter(|e| {
            e.harness == Harness::Claude
                && matches!(
                    e.kind,
                    EventKind::TurnStart | EventKind::PeerTurn | EventKind::AutoTurn
                )
        }) {
            for session in sessions(e) {
                starts.entry(session).or_default().push(e);
            }
        }
        let mut requests: HashMap<&str, Vec<&Request>> = HashMap::default();
        let mut first: HashMap<&str, Millis> = HashMap::default();
        let claude = |r: &&Request| r.harness == Harness::Claude;
        for r in scan.cache_context.requests.iter().filter(claude) {
            first.entry(&r.session).or_insert(r.time);
        }
        for r in scan.requests.iter().filter(claude) {
            first.entry(&r.session).or_insert(r.time);
            if window(r.time) {
                requests.entry(&r.session).or_default().push(r);
            }
        }
        for list in starts.values_mut() {
            list.sort_by_key(|e| e.time);
        }
        for list in requests.values_mut() {
            list.sort_by_key(|r| r.time);
        }
        let mut subagents: HashMap<&str, Vec<(Millis, &str)>> = HashMap::default();
        for s in scan
            .subagents
            .iter()
            .filter(|s| s.harness == Harness::Claude)
        {
            if let (Some(parent), Some(&time)) = (s.parent.as_deref(), first.get(&*s.session)) {
                subagents
                    .entry(parent)
                    .or_default()
                    .push((time, &s.session));
            }
        }
        Turns {
            starts,
            requests,
            subagents,
        }
    }

    /// The requests of the turn `start` opened, in each session it was read in: the
    /// session's until the next record there starts a turn, and every request of each
    /// subagent first heard from in that span, which outlives the turn when it runs in the
    /// background. A request may repeat.
    fn requests(&self, start: &StructuralEvent) -> Vec<&'a Request> {
        let mut found = vec![];
        for session in sessions(start) {
            // A delivery also recorded as a prompt is one record, not the next turn.
            let next = self.starts.get(session).and_then(|list| {
                let first = list.partition_point(|e| e.time < start.time);
                let at = first + list[first..].iter().position(|e| e.id == start.id)?;
                list[at + 1..]
                    .iter()
                    .find(|e| e.location != start.location)
                    .map(|e| e.time)
            });
            let within = |time: Millis| time >= start.time && next.is_none_or(|n| time < n);
            // Sorted by time, so the turn's own requests are one slice.
            let all = self.requests.get(session).map_or(&[][..], Vec::as_slice);
            let from = all.partition_point(|r| r.time < start.time);
            let to = next.map_or(all.len(), |n| all.partition_point(|r| r.time < n));
            let own = all[from..to.max(from)].iter();
            let spawned = self
                .subagents
                .get(session)
                .into_iter()
                .flatten()
                .filter(|&&(first, _)| within(first))
                .flat_map(|(_, s)| self.requests.get(s).into_iter().flatten());
            found.extend(own.chain(spawned).copied());
        }
        found
    }
}

/// Whether a T3 instance's launch arguments can set this profile's settings unseen.
fn t3_unresolved(p: &Profile) -> bool {
    p.t3.as_ref()
        .is_some_and(|t| t.enabled && t.settings_unresolved)
}

fn line_use(
    scan: &Scan,
    catalog: &Catalog,
    turns: &Turns<'_>,
    window: &dyn Fn(Millis) -> bool,
    line: &Group<'_>,
) -> InboundUse {
    let mut control: InboundControl = line
        .lead
        .inbound
        .clone()
        .expect("filtered on an inbound control");
    // A T3 instance's launch arguments, including any --settings, can decide it instead.
    if control.unresolved.is_none() && t3_unresolved(line.lead) {
        control.unresolved = Some("the T3 instance's launch arguments, a secret environment value or machine policy can set it, and which apply is unresolved".into());
    }
    let mine = |e: &&StructuralEvent| {
        e.harness == Harness::Claude && line.covers(&e.profiles) && window(e.time)
    };
    let mut idle = 0;
    let mut busy = 0;
    // A request in two message-started turns, as a subagent resumed by a later one, counts once.
    let mut priced: IndexMap<&str, &Request> = IndexMap::default();
    for e in scan.events.iter().filter(mine) {
        match e.kind {
            EventKind::PeerTurn => {
                idle += 1;
                // A request T3 enrichment gave another profile is not this line's.
                for r in turns
                    .requests(e)
                    .into_iter()
                    .filter(|r| line.covers(&r.profiles))
                {
                    priced.entry(&r.id).or_insert(r);
                }
            }
            EventKind::PeerMessage => busy += 1,
            _ => {}
        }
    }
    let mut sum = Sum::new();
    let mut sample = Sample::default();
    for r in priced.values() {
        let cost = price_request(catalog, r);
        sum.add(&cost);
        sample.add_cost(&cost);
    }
    let requests = priced.len() as u64;
    InboundUse {
        profiles: line.ids.clone(),
        control,
        idle,
        busy,
        requests,
        cost: sum.cost(),
        estimate: sample.estimate(requests),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Inbound;
    use crate::prices::price_request;
    use crate::reader::scan;
    use crate::test_support::{Workspace, prices, report_line};
    use serde_json::{Value, json};

    const START: Millis = 1_793_491_200_000; // 2026-11-01T00:00:00Z

    fn at(second: i64) -> String {
        jiff::Timestamp::from_millisecond(START + second * 1000)
            .unwrap()
            .to_string()
    }

    /// Writes one JSON record per line, creating the file's directory.
    fn write(path: std::path::PathBuf, records: &[Value]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let text: String = records.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn message_turns_count_their_subagents_and_end_at_the_next_turn() {
        let w = Workspace::new();
        let cwd = w.home.join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        let cwd = cwd.to_str().unwrap().to_string();
        let record = |second: i64, uuid: &str, fields: Value| {
            let mut r = json!({
                "sessionId": "main",
                "uuid": uuid,
                "timestamp": at(second),
                "cwd": cwd,
            });
            r.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            r
        };
        let assistant = |second: i64, id: &str| {
            record(
                second,
                &format!("a-{id}"),
                json!({
                    "type": "assistant",
                    "message": {
                        "id": id,
                        "model": "claude-opus-5-5",
                        "usage": {
                            "input_tokens": 100,
                            "output_tokens": 20,
                            "cache_read_input_tokens": 0,
                            "cache_creation_input_tokens": 0,
                            "server_tool_use": {},
                            "service_tier": "standard",
                        },
                    },
                }),
            )
        };
        let user = |second: i64, uuid: &str, fields: Value| {
            let mut fields = fields;
            fields["type"] = json!("user");
            fields["message"] = json!({ "role": "user", "content": "SYNTHETIC_PRIVATE_PROMPT" });
            record(second, uuid, fields)
        };
        let peer = json!({ "kind": "peer", "from": "uds:/synthetic/peer.sock", "body": "SYNTHETIC_PRIVATE_BODY" });
        let handback = json!({ "kind": "peer", "from": "agent", "senderTaskId": "agent", "body": "SYNTHETIC_PRIVATE_BODY" });
        let queued = |second: i64, uuid: &str, origin: &Value| {
            record(
                second,
                uuid,
                json!({
                    "type": "attachment",
                    "attachment": {
                        "type": "queued_command",
                        "isMeta": true,
                        "origin": origin,
                        "prompt": "SYNTHETIC_PRIVATE_BODY",
                    },
                }),
            )
        };
        let mut notice = user(
            30,
            "notice",
            json!({ "isMeta": true, "turnOrigin": "system" }),
        );
        notice["message"]["content"] =
            json!("[Cross-session idle notice] SYNTHETIC_PRIVATE_LABEL is idle now.");
        write(
            w.claude.join("projects/project/main.jsonl"),
            &[
                user(0, "typed", json!({})),
                assistant(1, "before"),
                // A message to the idle session starts a turn.
                user(
                    10,
                    "message",
                    json!({ "isMeta": true, "turnOrigin": "peer", "origin": peer }),
                ),
                assistant(11, "answer"),
                // One read between tool calls counts, but adds no turn.
                queued(12, "busy", &peer),
                // A compaction summary carries the turn's origin but continues it.
                user(
                    13,
                    "summary",
                    json!({ "isCompactSummary": true, "turnOrigin": "peer" }),
                ),
                assistant(14, "after-summary"),
                // A subagent's report starts a turn of its own, which no setting gates.
                user(
                    20,
                    "report",
                    json!({ "isMeta": true, "turnOrigin": "peer", "origin": handback }),
                ),
                assistant(21, "report-answer"),
                notice,
                assistant(31, "notice-answer"),
                queued(32, "subagent-busy", &handback),
                user(40, "next", json!({})),
                assistant(41, "later"),
            ],
        );
        let subagent = |name: &str, requests: &[(i64, &str)]| {
            let records: Vec<Value> = requests
                .iter()
                .map(|&(second, id)| {
                    let mut r = assistant(second, id);
                    r["isSidechain"] = json!(true);
                    r
                })
                .collect();
            write(
                w.claude.join(format!(
                    "projects/project/main/subagents/agent-{name}.jsonl"
                )),
                &records,
            );
        };
        // Spawned in the message's turn, it counts even after that turn ends.
        subagent("spawned", &[(15, "spawned-1"), (50, "spawned-2")]);
        subagent("other", &[(22, "other-1")]);
        // Started before the message, so even its requests inside that turn are not the
        // message's, whether or not the window starts between them.
        subagent("old", &[(5, "old-1"), (11, "old-2")]);
        std::fs::write(
            w.claude.join("settings.json"),
            r#"{"crossSessionInbound": "accept"}"#,
        )
        .unwrap();
        // The project tightens it, so it decides.
        let dot_claude = std::path::Path::new(&cwd).join(".claude");
        std::fs::create_dir_all(&dot_claude).unwrap();
        let project = dot_claude.join("settings.json");
        std::fs::write(&project, r#"{"crossSessionInbound": "hold"}"#).unwrap();
        let project = project.to_str().unwrap().to_string();

        let data = scan(&w.scan_options()).unwrap();
        assert!(!format!("{data:?}").contains("SYNTHETIC_PRIVATE"));
        let catalog = prices();
        let lines = inbound_use(&data, &catalog, None, START + 3_600_000);
        assert_eq!(lines.len(), 1, "Codex gets no line");
        let u = &lines[0];
        assert_eq!((u.idle, u.busy, u.requests), (2, 1, 5));
        assert_eq!(u.control.value, Some(Inbound::Hold));
        assert_eq!(u.control.file, project);
        assert_eq!(u.control.directory.as_deref(), Some(cwd.as_str()));
        let priced = price_request(&catalog, &data.requests[0]);
        let one = priced.total_usd.unwrap_or_else(|| panic!("{priced:?}"));
        assert_eq!(u.cost.total_usd, Some(one + one + one + one + one));

        let counts = |u: &InboundUse| (u.idle, u.busy, u.requests);
        let mut windowed = w.scan_options();
        windowed.since = Some(START + 8_000);
        let windowed = scan(&windowed).unwrap();
        let lines = inbound_use(&windowed, &catalog, None, START + 3_600_000);
        assert_eq!(counts(&lines[0]), (2, 1, 5));
        // A later analysis bound counts only the deliveries and requests after it.
        let lines = inbound_use(&data, &catalog, Some(START + 25_000), START + 3_600_000);
        assert_eq!(counts(&lines[0]), (1, 0, 1));

        // The line has no verdict, changes nothing and offers nothing.
        let report = crate::replay::analyze(crate::replay::AnalyzeOptions {
            scan: data,
            catalog: &catalog,
            since: None,
            now: START + 3_600_000,
            max_cost_per_use_usd: 1.0,
        })
        .unwrap();
        let fixes = crate::doctor::fixes(&report, false);
        assert!(fixes.is_empty());
        let text =
            crate::report::format_report(&report, &fixes, crate::report::ReportOptions::default())
                .text();
        let line = report_line(&text, "Messages from other sessions");
        assert!(!line.change, "{text}");
        assert_eq!(line.mark, "info");
        assert!(line.head.contains("hold"), "{}", line.head);
        assert!(
            line.setting.contains("crossSessionInbound"),
            "{}",
            line.setting
        );
    }

    #[test]
    fn a_t3_instance_reads_its_own_latest_sessions_project_settings() {
        let w = Workspace::new();
        let project = w.home.join("t3-project");
        std::fs::create_dir_all(project.join(".claude")).unwrap();
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let local = project.join(".claude").join("settings.local.json");
        std::fs::write(&local, r#"{"crossSessionInbound": "refuse"}"#).unwrap();
        let root = w.claude.to_str().unwrap();
        let mut profiles = vec![
            crate::test_support::make_profile("native", root),
            crate::test_support::make_t3_profile("agent", root),
        ];
        for p in &mut profiles {
            p.settings_file = format!("{root}/settings.json");
        }
        std::fs::write(
            &profiles[0].settings_file,
            r#"{"crossSessionInbound": "accept"}"#,
        )
        .unwrap();
        let mut request = crate::test_support::make_request();
        request.profiles = vec!["agent".to_string()].into();
        request.cwd = Some(project.to_str().unwrap().into());
        crate::discovery::discover_inbound(&w.scan_options().discovery, &mut profiles, &[request])
            .unwrap();
        let control = |i: usize| {
            let c = profiles[i].inbound.clone().unwrap();
            (c.value, c.file)
        };
        // The native profile recorded no session there, so its user file decides.
        assert_eq!(
            control(0),
            (Some(Inbound::Accept), profiles[0].settings_file.clone())
        );
        let local = local.to_str().unwrap().to_string();
        assert_eq!(control(1), (Some(Inbound::Refuse), local));
    }
}
