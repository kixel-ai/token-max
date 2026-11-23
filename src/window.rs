//! Which transcripts a `--since` window depends on, so `token-max sessions` can skip the rest.
//!
//! A transcript is read when one of its records is timed inside the window, or when it shares
//! an identifier with a transcript that is read: the reader joins records of different files
//! only through such identifiers, so the transcripts left out cannot change the window's
//! requests or the facts of its sessions. Record timestamps decide, never modification times,
//! which a clock change or a copy can leave earlier than the records inside.

use std::borrow::Cow;
use std::hash::BuildHasher;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use crate::collections::HashSet;
use crate::model::{Harness, Millis, Snapshot};
use crate::reader::read_lines;
use crate::safe::timestamp;

/// The most threads that summarize transcripts at once.
const THREADS: usize = 8;

/// What one transcript holds that decides whether the window reads it.
struct Summary {
    /// A record is timed inside the window, or may be.
    reaches: bool,
    /// Hashes of the identifiers its records share with other transcripts. Two identifiers
    /// sharing a hash only make a transcript read that need not be.
    joins: Vec<u64>,
}

/// Whether each snapshot must be read for the requests at or after `since`.
pub fn reads(snapshots: &[Snapshot], since: Millis) -> Vec<bool> {
    let hasher = foldhash::fast::RandomState::default();
    let summaries = summarize(snapshots, since, &hasher);
    let mut read: Vec<bool> = summaries.iter().map(|s| s.reaches).collect();
    let mut joined: HashSet<u64> = HashSet::default();
    for (s, _) in summaries.iter().zip(&read).filter(|(_, read)| **read) {
        joined.extend(&s.joins);
    }
    // Each pass reads every transcript joined to one already read; a chain of joins, such
    // as a nested subagent's ancestors, takes one pass per link.
    loop {
        let mut grew = false;
        for (s, read) in summaries.iter().zip(&mut read) {
            if !*read && s.joins.iter().any(|j| joined.contains(j)) {
                *read = true;
                joined.extend(&s.joins);
                grew = true;
            }
        }
        if !grew {
            return read;
        }
    }
}

/// Summarize every snapshot, one transcript per thread at a time.
fn summarize(
    snapshots: &[Snapshot],
    since: Millis,
    hasher: &foldhash::fast::RandomState,
) -> Vec<Summary> {
    let next = AtomicUsize::new(0);
    let done: Vec<Mutex<Option<Summary>>> = snapshots.iter().map(|_| Mutex::new(None)).collect();
    // Past eight threads, summaries finished no sooner on 32 cores, and each thread holds
    // several megabytes of allocator pages.
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(THREADS)
        .min(snapshots.len());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut reader = Summarizer {
                    since,
                    hasher,
                    buffer: vec![],
                    joins: HashSet::default(),
                };
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(snapshot) = snapshots.get(index) else {
                        return;
                    };
                    let summary = reader.summary(snapshot);
                    *done[index].lock().expect("no summary panics") = Some(summary);
                }
            });
        }
    });
    done.into_iter()
        .map(|s| {
            s.into_inner()
                .expect("no summary panics")
                .expect("every snapshot is summarized")
        })
        .collect()
}

/// One thread's summaries, reusing its buffers across transcripts.
struct Summarizer<'h> {
    since: Millis,
    hasher: &'h foldhash::fast::RandomState,
    buffer: Vec<u8>,
    /// A transcript repeats its session and turn on every record, so each is kept once.
    joins: HashSet<u64>,
}

impl Summarizer<'_> {
    /// Read one transcript's lines as the reader would, keeping only what decides its reach.
    fn summary(&mut self, snapshot: &Snapshot) -> Summary {
        let Self {
            since,
            hasher,
            buffer,
            joins,
        } = self;
        let schema = match snapshot.harness {
            Harness::Claude => &CLAUDE,
            Harness::Codex => &CODEX,
        };
        joins.clear();
        // Reading counts lines on the snapshot; the reader counts them again on its own.
        let mut copy = snapshot.clone();
        let read = |line: &[u8]| {
            let found = pick(line, snapshot.harness, schema, &mut |id: &str| {
                joins.insert(hasher.hash_one(id));
            })?;
            Some(match found.time {
                Some(time) => time >= *since,
                // The reader counts a record without a time in the window, and fails on
                // such a request, so its transcript is read.
                None => found.untimed_matters,
            })
        };
        // A transcript the reader fails on is read, so the reader names why.
        let Ok(mut lines) = read_lines(&mut copy, read, std::mem::take(buffer)) else {
            return Summary {
                reaches: true,
                joins: vec![],
            };
        };
        let mut reaches = false;
        for line in &mut lines {
            match line {
                Ok((inside, _)) => reaches |= inside,
                Err(_) => {
                    reaches = true;
                    break;
                }
            }
        }
        *buffer = lines.into_buffer();
        Summary {
            reaches,
            joins: joins.iter().copied().collect(),
        }
    }
}

/// The fields of a record that decide its reach, from its top level.
struct Found {
    time: Option<Millis>,
    /// Whether the record is one the reader may price or fail on when it has no time.
    /// Claude Code writes many untimed records the reader skips, such as titles and
    /// prompts; Codex times all of its records.
    untimed_matters: bool,
}

impl Found {
    fn new(harness: Harness) -> Self {
        Self {
            time: None,
            untimed_matters: harness == Harness::Codex,
        }
    }
}

/// Whether a Claude record of this type is one the reader may price or fail on.
fn priced(kind: &str) -> bool {
    matches!(kind, "assistant" | "api-request" | "system")
}

/// Where in a record the fields a window reads sit.
enum Node {
    /// An object, of which only these members are read.
    Object(&'static [(&'static str, Node)]),
    /// An array, each of whose items is read as this.
    Each(&'static Node),
    /// An identifier other records may share.
    Id,
    /// The record's time.
    Time,
    /// A Claude record's type.
    Kind,
}

const BLOCK: Node = Node::Object(&[("id", Node::Id), ("tool_use_id", Node::Id)]);

/// Every identifier the reader, the session export or the T3 join match across Claude
/// records: sessions and subagents, records and their parents, requests and the API records
/// they answer, and the tool and message keys T3 items name.
///
/// A join the reader adds on any other field must be listed here or in [`CODEX`], or a
/// window could leave out a transcript that changes it.
const CLAUDE: Node = Node::Object(&[
    ("timestamp", Node::Time),
    ("type", Node::Kind),
    ("sessionId", Node::Id),
    ("agentId", Node::Id),
    ("uuid", Node::Id),
    ("parentUuid", Node::Id),
    ("id", Node::Id),
    ("requestId", Node::Id),
    ("requestRef", Node::Id),
    ("compactMetadata", Node::Object(&[("requestId", Node::Id)])),
    (
        "message",
        Node::Object(&[("id", Node::Id), ("content", Node::Each(&BLOCK))]),
    ),
]);

/// Every identifier matched across Codex records: threads, their parents and fork sources,
/// turns, responses, tool calls and compactions.
const CODEX: Node = Node::Object(&[
    ("timestamp", Node::Time),
    (
        "payload",
        Node::Object(&[
            ("id", Node::Id),
            ("session_id", Node::Id),
            ("parent_thread_id", Node::Id),
            ("forked_from_id", Node::Id),
            ("turn_id", Node::Id),
            ("response_id", Node::Id),
            ("call_id", Node::Id),
            ("compaction_response_id", Node::Id),
            ("window_id", Node::Id),
            (
                "source",
                Node::Object(&[(
                    "subagent",
                    Node::Object(&[(
                        "thread_spawn",
                        Node::Object(&[("parent_thread_id", Node::Id)]),
                    )]),
                )]),
            ),
        ]),
    ),
]);

/// Read one line's identifiers into `add` and return its time and type, or None when the
/// reader would find it unreadable.
///
/// Most lines are walked once without building them, skipping every other member. A line
/// that walk refuses, such as one with escaped lone surrogates, is parsed as the reader
/// parses it; identifiers the refused walk already added only make more transcripts read.
fn pick(line: &[u8], harness: Harness, schema: &Node, add: &mut dyn FnMut(&str)) -> Option<Found> {
    let mut found = Found::new(harness);
    let mut de =
        serde_json::Deserializer::from_slice(line.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(line));
    let walked = Walk {
        node: schema,
        found: &mut found,
        add,
    }
    .deserialize(&mut de)
    .and_then(|()| de.end());
    if walked.is_ok() {
        return Some(found);
    }
    let raw = Value::Object(crate::reader::parse(line)?);
    let mut found = Found::new(harness);
    walk(&raw, schema, &mut found, add);
    Some(found)
}

/// [`pick`]'s walk over a parsed record.
fn walk(value: &Value, node: &Node, found: &mut Found, add: &mut dyn FnMut(&str)) {
    match (node, value) {
        (Node::Object(members), Value::Object(map)) => {
            for (key, node) in *members {
                if let Some(value) = map.get(*key) {
                    walk(value, node, found, add);
                }
            }
        }
        (Node::Each(node), Value::Array(items)) => {
            for item in items {
                walk(item, node, found, add);
            }
        }
        (Node::Id, Value::String(id)) if !id.is_empty() => add(id),
        (Node::Time, value) => found.time = timestamp(value),
        (Node::Kind, value) => found.untimed_matters = value.as_str().is_some_and(priced),
        _ => {}
    }
}

/// A walk of one value as `node` describes it, during deserialization.
struct Walk<'n, 'f, 'a> {
    node: &'n Node,
    found: &'f mut Found,
    add: &'a mut dyn FnMut(&str),
}

impl<'de> DeserializeSeed<'de> for Walk<'_, '_, '_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Walk<'_, '_, '_> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<(), E> {
        match self.node {
            Node::Id if !text.is_empty() => (self.add)(text),
            Node::Time => self.found.time = timestamp(&Value::from(text)),
            Node::Kind => self.found.untimed_matters = priced(text),
            _ => {}
        }
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let members = match self.node {
            Node::Object(members) => *members,
            _ => &[],
        };
        while let Some(key) = map.next_key::<Cow<'de, str>>()? {
            match members.iter().find(|(name, _)| *name == key) {
                Some((_, node)) => map.next_value_seed(Walk {
                    node,
                    found: &mut *self.found,
                    add: &mut *self.add,
                })?,
                None => map.next_value::<IgnoredAny>().map(drop)?,
            }
        }
        self.clear_scalar();
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        match self.node {
            Node::Each(node) => {
                while seq
                    .next_element_seed(Walk {
                        node,
                        found: &mut *self.found,
                        add: &mut *self.add,
                    })?
                    .is_some()
                {}
            }
            _ => while seq.next_element::<IgnoredAny>()?.is_some() {},
        }
        self.clear_scalar();
        Ok(())
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        self.clear_scalar();
        Ok(())
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        self.clear_scalar();
        Ok(())
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        self.clear_scalar();
        Ok(())
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        self.clear_scalar();
        Ok(())
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        self.clear_scalar();
        Ok(())
    }
}

impl Walk<'_, '_, '_> {
    /// A time or type that is not a string reads as none, and a later duplicate member
    /// replaces an earlier one, as in the parsed record.
    fn clear_scalar(self) {
        match self.node {
            Node::Time => self.found.time = None,
            Node::Kind => self.found.untimed_matters = false,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a line yields read without building it, and read as the reader parses it.
    fn both(line: &str, harness: Harness) -> [(Option<Millis>, bool, Vec<String>); 2] {
        let schema = match harness {
            Harness::Claude => &CLAUDE,
            Harness::Codex => &CODEX,
        };
        let mut ids = vec![];
        let mut found = Found::new(harness);
        let mut de = serde_json::Deserializer::from_slice(line.as_bytes());
        Walk {
            node: schema,
            found: &mut found,
            add: &mut |id: &str| ids.push(id.to_string()),
        }
        .deserialize(&mut de)
        .and_then(|()| de.end())
        .unwrap();
        let streamed = (found.time, found.untimed_matters, ids);
        let mut ids = vec![];
        let raw = Value::Object(crate::reader::parse(line.as_bytes()).unwrap());
        let mut found = Found::new(harness);
        walk(&raw, schema, &mut found, &mut |id: &str| {
            ids.push(id.to_string())
        });
        [streamed, (found.time, found.untimed_matters, ids)]
    }

    #[test]
    fn reading_a_line_without_building_it_finds_what_parsing_it_does() {
        let at = |time: &str| timestamp(&Value::from(time));
        for (line, harness, time, untimed_matters, ids) in [
            // A later duplicate replaces an earlier one, and a time that is not a string is
            // none; escaped names and identifiers read as the names they spell.
            (
                r#"{"timestamp":"2026-11-01T00:00:00Z","type":"assistant","timestamp":5,"session\u0049d":"s\u0031","message":"text"}"#,
                Harness::Claude,
                None,
                true,
                &["s1"][..],
            ),
            (
                r#"{"timestamp":{"nested":"2026-11-01T00:00:00Z"},"type":"system","timestamp":"2026-11-01T00:00:00Z","type":"ai-title","uuid":""}"#,
                Harness::Claude,
                at("2026-11-01T00:00:00Z"),
                false,
                &[],
            ),
            (
                r#"{"type":"assistant","message":{"id":"m","content":["text",{"type":"tool_use","id":"t","input":{"id":"inner"}},{"tool_use_id":"r","content":[{"id":"deeper"}]}]},"requestId":"q","parentUuid":null}"#,
                Harness::Claude,
                None,
                true,
                &["q", "m", "t", "r"],
            ),
            (
                r#"{"timestamp":"2026-11-01T00:00:00Z","payload":{"id":"a","session_id":"b","source":{"subagent":{"thread_spawn":{"parent_thread_id":"c"}}},"turn_id":"d","info":{"id":"skipped"}}}"#,
                Harness::Codex,
                at("2026-11-01T00:00:00Z"),
                true,
                &["a", "b", "d", "c"],
            ),
            (
                r#"{"type":"event_msg","payload":[{"id":"skipped"}]}"#,
                Harness::Codex,
                None,
                true,
                &[],
            ),
        ] {
            let [streamed, parsed] = both(line, harness);
            let mut want = (
                time,
                untimed_matters,
                ids.iter().map(|s| s.to_string()).collect(),
            );
            assert_eq!(streamed.0, want.0, "{line}");
            assert_eq!(streamed.1, want.1, "{line}");
            let sorted = |mut ids: Vec<String>| {
                ids.sort();
                ids
            };
            want.2 = sorted(want.2);
            assert_eq!(sorted(streamed.2), want.2, "{line}");
            assert_eq!((parsed.0, parsed.1, sorted(parsed.2)), want, "{line}");
        }
    }

    #[test]
    fn a_line_the_walk_refuses_is_parsed_as_the_reader_parses_it() {
        // JavaScript writers leave lone surrogates in truncated text, which only the
        // reader's parse accepts.
        let line = br#"{"timestamp":"2026-11-01T00:00:00Z","uuid":"u","sessionId":"s\ud800"}"#;
        let mut ids = vec![];
        let found = pick(line, Harness::Claude, &CLAUDE, &mut |id: &str| {
            ids.push(id.to_string())
        })
        .unwrap();
        assert_eq!(found.time, timestamp(&Value::from("2026-11-01T00:00:00Z")));
        assert!(ids.contains(&"s\u{FFFD}".to_string()), "{ids:?}");
        assert!(
            pick(
                b"{\"timestamp\":",
                Harness::Claude,
                &CLAUDE,
                &mut |_: &str| {}
            )
            .is_none()
        );
    }
}
