//! What a transcript's unreadable lines could hide, found without keeping their text.
//!
//! A line the reader cannot parse may hold a call to a feature that otherwise looks
//! unused. Searching it for the feature's name settles that for a line that was fully
//! written. A record a crash cut off can lose its name, so a transcript with an
//! unreadable line also counts the tool results whose call it never parsed: every call
//! that ran leaves a result, so such a result shows a call is missing. Only hashes of
//! the names and ids survive, in memory, and never in output.

use std::hash::{DefaultHasher, Hash, Hasher};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::collections::HashSet;
use crate::model::{ContextKind, Harness};

/// A token longer than this is data, not a feature or tool name.
const LONGEST_NAME: usize = 256;

/// What one transcript's unreadable lines mention and which tool results lack a call.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Unreadable {
    /// Hashes of the names, MCP servers and ids in the unreadable lines' bytes.
    #[serde(skip)]
    names: HashSet<u64>,
    /// Hashes of the tool call ids the unreadable lines' bytes name as results.
    #[serde(skip)]
    results: HashSet<u64>,
    /// Tool results, in parsed records or unreadable lines, whose id matches no parsed
    /// call anywhere in the file; zero unless the file has an unreadable line.
    pub unmatched_results: u64,
}

fn hash(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// The marker for an MCP server that a tool name `mcp__server__tool` mentions.
fn server_marker(server: &str) -> u64 {
    hash(&format!("mcp:{server}"))
}

/// The marker for any tool whose name starts with `Artifact`, which share one switch.
fn artifact_marker() -> u64 {
    hash("prefix:Artifact")
}

impl Unreadable {
    /// Take in one unreadable line: its names and the ids of the results it holds.
    pub fn observe(&mut self, line: &[u8]) {
        // Claude names a result's call by `tool_use_id`. Codex names it by `call_id`, which
        // its calls carry too, so only an output item's counts; its tag comes first.
        for (key, tagged) in [(&b"\"tool_use_id\""[..], false), (b"\"call_id\"", true)] {
            for at in memchr::memmem::find_iter(line, key) {
                if tagged && !output_before(&line[..at]) {
                    continue;
                }
                if let Some(id) = value_after(&line[at + key.len()..]) {
                    self.results.insert(hash(id));
                }
            }
        }
        let text = unescape(line);
        for token in text
            .split(|b| !(b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-'))
            .filter(|t| !t.is_empty() && t.len() <= LONGEST_NAME)
        {
            // Tokens are ASCII.
            let token = std::str::from_utf8(token).expect("ASCII tokens are UTF-8");
            self.names.insert(hash(token));
            if let Some(rest) = token.strip_prefix("mcp__") {
                self.names
                    .insert(server_marker(rest.split("__").next().unwrap_or(rest)));
            }
            if token.starts_with("Artifact") {
                self.names.insert(artifact_marker());
            }
        }
    }

    /// Settle the results nothing parsed called, once the whole file has been read.
    pub fn finish(&mut self, parsed: &ToolIds, unreadable_lines: u64) {
        let results = std::mem::take(&mut self.results);
        self.unmatched_results = if unreadable_lines == 0 {
            0
        } else {
            parsed
                .results
                .iter()
                .chain(&results)
                .filter(|id| !parsed.calls.contains(*id))
                .collect::<HashSet<_>>()
                .len() as u64
        };
    }

    /// Whether the lines mention the row's feature by the name a call records it under.
    pub fn mentions(&self, kind: ContextKind, name: &str) -> bool {
        let has = |text: &str| self.names.contains(&hash(text));
        // A name made of several tokens, such as `plugin:skill`, needs every one.
        let named = |name: &str| {
            let mut parts = name
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                .filter(|p| !p.is_empty())
                .peekable();
            parts.peek().is_some() && parts.all(has)
        };
        match kind {
            ContextKind::Tool if name == "Artifact" => self.names.contains(&artifact_marker()),
            ContextKind::Tool | ContextKind::Skill => named(name),
            // A call names `mcp__server__tool`; a native Codex completion names the raw server.
            ContextKind::McpServer => self.names.contains(&server_marker(name)) || named(name),
            ContextKind::McpTool => {
                let split = name.strip_prefix("mcp__").and_then(|n| n.split_once("__"));
                named(name)
                    || split.is_some_and(|(server, tool)| {
                        !tool.is_empty() && named(tool) && named(server)
                    })
            }
            _ => false,
        }
    }
}

/// Whether the nearest `"type"` tag before this point names an output item.
fn output_before(head: &[u8]) -> bool {
    const KEY: &[u8] = b"\"type\"";
    let mut end = head.len();
    while let Some(at) = memchr::memmem::rfind(&head[..end], KEY) {
        let rest = head[at + KEY.len()..].trim_ascii_start();
        if let Some(rest) = rest.strip_prefix(b":")
            && let Some(rest) = rest.trim_ascii_start().strip_prefix(b"\"")
        {
            let value = &rest[..memchr::memchr(b'"', rest).unwrap_or(rest.len())];
            return value.ends_with(b"_output");
        }
        end = at;
    }
    false
}

/// The text after a JSON key: `:`, optional space, then the string value's opening quote;
/// the value's name characters, or None when it is not a string.
fn value_after(rest: &[u8]) -> Option<&str> {
    let rest = rest.trim_ascii_start().strip_prefix(b":")?;
    let rest = rest.trim_ascii_start().strip_prefix(b"\"")?;
    let end = rest
        .iter()
        .position(|b| !(b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-'))
        .unwrap_or(rest.len());
    (end > 0).then(|| std::str::from_utf8(&rest[..end]).expect("ASCII ids are UTF-8"))
}

/// Decode JSON string escapes, such as `\u0041gent` for `Agent`, wherever the bytes hold
/// them. Anything that is not a whole escape stays as written.
fn unescape(line: &[u8]) -> Vec<u8> {
    let unit = |at: usize| -> Option<u16> {
        let digits = line.get(at + 2..at + 6)?;
        if line[at] != b'\\' || line.get(at + 1) != Some(&b'u') {
            return None;
        }
        u16::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()
    };
    let mut out = Vec::with_capacity(line.len());
    let mut at = 0;
    while at < line.len() {
        if line[at] != b'\\' {
            out.push(line[at]);
            at += 1;
            continue;
        }
        let simple = match line.get(at + 1) {
            Some(b'n') => Some(b'\n'),
            Some(b't') => Some(b'\t'),
            Some(b'r') => Some(b'\r'),
            Some(b'b') => Some(8),
            Some(b'f') => Some(12),
            Some(&c @ (b'"' | b'\\' | b'/')) => Some(c),
            _ => None,
        };
        if let Some(byte) = simple {
            out.push(byte);
            at += 2;
        } else if let Some(high) = unit(at) {
            let low = unit(at + 6).filter(|l| (0xdc00..0xe000).contains(l));
            let (code, used) = match low {
                Some(low) if (0xd800..0xdc00).contains(&high) => (
                    Some(0x10000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(low) - 0xdc00)),
                    12,
                ),
                _ => (Some(u32::from(high)), 6),
            };
            let c = code.and_then(char::from_u32).unwrap_or('\u{fffd}');
            let mut buffer = [0; 4];
            out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
            at += used;
        } else {
            out.push(b'\\');
            at += 1;
        }
    }
    out
}

/// The tool call and result ids of a transcript's parsed records, by hash.
#[derive(Debug, Default)]
pub struct ToolIds {
    calls: HashSet<u64>,
    results: HashSet<u64>,
}

impl ToolIds {
    /// Take in one parsed record's calls and results.
    pub fn observe(&mut self, record: &Map<String, Value>, harness: Harness) {
        let id = |value: Option<&Value>| value.and_then(Value::as_str).map(hash);
        if harness == Harness::Codex {
            if record.get("type").and_then(Value::as_str) != Some("response_item") {
                return;
            }
            let Some(item) = record.get("payload").and_then(Value::as_object) else {
                return;
            };
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            if kind.ends_with("_call") {
                self.calls.extend(
                    id(item.get("call_id"))
                        .into_iter()
                        .chain(id(item.get("id"))),
                );
            } else if kind.ends_with("_output") {
                self.results
                    .extend(id(item.get("call_id")).or_else(|| id(item.get("id"))));
            }
            return;
        }
        let Some(Value::Array(content)) = record.get("message").and_then(|m| m.get("content"))
        else {
            return;
        };
        for block in content.iter().filter_map(Value::as_object) {
            if block
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t.ends_with("tool_use"))
            {
                self.calls.extend(id(block.get("id")));
            }
            self.results.extend(id(block.get("tool_use_id")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(lines: &[&str]) -> Unreadable {
        let mut unreadable = Unreadable::default();
        for line in lines {
            unreadable.observe(line.as_bytes());
        }
        unreadable
    }

    #[test]
    fn a_name_is_found_as_a_call_records_it_even_escaped_or_joined() {
        let lines = seen(&[
            // An escaped name, a cut record joined with the next record, and a skill.
            r#"{"name":"Agent","input":{"skill":"review"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"x"}{"type":"user","message":{"content":[{"type":"tool_use","name":"mcp__docs__search"}"#,
        ]);
        assert!(lines.mentions(ContextKind::Tool, "Agent"));
        assert!(lines.mentions(ContextKind::Skill, "review"));
        assert!(lines.mentions(ContextKind::McpServer, "docs"));
        assert!(lines.mentions(ContextKind::McpTool, "mcp__docs__search"));
        assert!(!lines.mentions(ContextKind::McpTool, "mcp__docs__fetch"));
        assert!(!lines.mentions(ContextKind::Tool, "Bash"));
        assert!(!lines.mentions(ContextKind::McpServer, "other"));
        // A cut inside the name leaves a prefix that names nothing.
        assert!(!seen(&[r#"{"type":"tool_use","name":"Age"#]).mentions(ContextKind::Tool, "Agent"));
    }

    #[test]
    fn artifact_helpers_and_plugin_skills_match_their_row() {
        let lines = seen(&[r#"{"name":"ArtifactData","skill":"plugin:review"}"#]);
        assert!(lines.mentions(ContextKind::Tool, "Artifact"));
        assert!(lines.mentions(ContextKind::Skill, "plugin:review"));
        assert!(!lines.mentions(ContextKind::Skill, "plugin:other"));
    }

    #[test]
    fn an_escaped_surrogate_pair_and_a_lone_half_decode_without_failing() {
        assert_eq!(
            unescape(br"\ud83d\ude00\ud83d \u00"),
            "\u{1f600}\u{fffd} \\u00".as_bytes()
        );
        assert_eq!(unescape(br"a\/b\u0041"), b"a/bA");
        assert_eq!(unescape(br"\\u0041"), br"\u0041");
    }

    fn parsed(records: &[Value], harness: Harness) -> ToolIds {
        let mut ids = ToolIds::default();
        for record in records {
            ids.observe(record.as_object().unwrap(), harness);
        }
        ids
    }

    #[test]
    fn results_without_a_parsed_call_count_only_beside_an_unreadable_line() {
        let claude = parsed(
            &[
                serde_json::json!({"message": {"content": [{"type": "tool_use", "id": "a", "name": "Bash"}]}}),
                serde_json::json!({"message": {"content": [{"type": "tool_result", "tool_use_id": "a"}]}}),
            ],
            Harness::Claude,
        );
        // Every result has its call.
        let mut cut = seen(&[r#"{"type":"tool_use","name":"Age"#]);
        cut.finish(&claude, 1);
        assert_eq!(cut.unmatched_results, 0);
        // A result whose call was cut off, parsed or joined into an unreadable line.
        let mut result = seen(&[
            r#"{"type":"tool_use","name":"Age{"content":[{"tool_use_id" : "b","type":"tool_result"}]}"#,
        ]);
        result.finish(&claude, 1);
        assert_eq!(result.unmatched_results, 1);
        let mut orphan = parsed(
            &[
                serde_json::json!({"message": {"content": [{"type": "tool_result", "tool_use_id": "c"}]}}),
            ],
            Harness::Claude,
        );
        let mut line = seen(&[r#"{"x":"#]);
        line.finish(&orphan, 1);
        assert_eq!(line.unmatched_results, 1);
        // Without an unreadable line nothing can be missing.
        line.finish(&orphan, 0);
        assert_eq!(line.unmatched_results, 0);
        orphan.calls.clear();
    }

    #[test]
    fn codex_calls_and_outputs_pair_by_call_id() {
        let record = |kind: &str, id: &str| serde_json::json!({"type": "response_item", "payload": {"type": kind, "call_id": id}});
        let ids = parsed(
            &[
                record("function_call", "k1"),
                record("function_call_output", "k1"),
                record("custom_tool_call_output", "k2"),
                serde_json::json!({"type": "event_msg", "payload": {"type": "exec_command_end", "call_id": "k3"}}),
            ],
            Harness::Codex,
        );
        // An unreadable call carries a call_id too, but nothing ran yet to leave a result.
        let mut unreadable = seen(&[
            r#"{"payload":{"type":"function_call_output","call_id":"k4"}"#,
            r#"{"payload":{"type":"function_call","name":"exec","call_id":"k5"}"#,
            // JSON may carry whitespace around the tag.
            r#"{"payload":{"type" : "custom_tool_call_output", "call_id": "k6"}"#,
        ]);
        unreadable.finish(&ids, 1);
        assert_eq!(unreadable.unmatched_results, 3);
    }
}
