//! One plain sentence saying what each reported setting and feature does.
//!
//! Built-in tools and settings get a curated sentence, checked against the provider's
//! documentation; a skill or MCP tool reuses the first sentence of the description it ships.

use crate::model::{BackgroundFeature, Bucket, CheckResult, ContextKind, Harness, SubagentCost};

/// The most characters of a shipped description that are kept, so a long one costs no more.
const MAX_CHARACTERS: usize = 200;

/// The first sentence of a shipped description, on one line and at most `MAX_CHARACTERS`
/// long; None when it is empty.
pub fn sentence(text: &str) -> Option<String> {
    let line = text.trim().lines().next()?;
    let end = line
        .char_indices()
        .find(|&(i, c)| {
            matches!(c, '.' | '!' | '?')
                && line[i + 1..].starts_with(char::is_whitespace)
                && !abbreviation(&line[..i])
        })
        .map_or(line.len(), |(i, _)| i + 1);
    let words: Vec<&str> = line[..end].split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    let text = words.join(" ");
    if text.chars().count() <= MAX_CHARACTERS {
        return Some(text);
    }
    // The ellipsis counts toward the bound.
    let cut: String = text.chars().take(MAX_CHARACTERS - 3).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    Some(format!("{}...", cut.trim_end_matches([',', ';', ':'])))
}

/// Whether the word before a period is an abbreviation, which does not end a sentence.
fn abbreviation(before: &str) -> bool {
    let word = before
        .rsplit(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or_default();
    matches!(
        word.to_ascii_lowercase().as_str(),
        "e.g" | "i.e" | "etc" | "vs" | "cf"
    )
}

/// What a checked setting does.
pub fn check(result: &CheckResult) -> &'static str {
    match result {
        CheckResult::Ttl(t) => match t.bucket {
            Bucket::Main => {
                "How long the main conversation's prompt stays cached between requests: a 1h cache costs more to write than 5m but survives longer pauses."
            }
            Bucket::Subagent => {
                "How long a subagent's prompt stays cached between its requests: a 1h cache costs more to write than 5m but survives longer pauses."
            }
        },
        CheckResult::Compaction(_) => {
            "How large a conversation grows before it is automatically summarized and continued from the summary."
        }
        CheckResult::TurnEnd(_) => {
            "Whether Codex summarizes a conversation when a turn ends, once it fills a set share of the context window: the summary is paid for while the cache is warm, and every later request carries the summary instead of the conversation."
        }
        CheckResult::Idle(_) => {
            "Whether Claude Code summarizes a large idle session just before its cache expires, so coming back rewrites a short summary instead of the whole conversation."
        }
        CheckResult::T3Idle(_) => {
            "Whether T3 Code summarizes an idle thread: never, when you return to it, or just before its cache expires."
        }
        CheckResult::Speed(_) => "Fast mode serves responses faster at a premium price per token.",
        CheckResult::Background(b) => match b.feature {
            BackgroundFeature::PromptSuggestions => {
                "After each response, Claude Code sends an extra request to suggest your next prompt."
            }
            BackgroundFeature::SessionRecaps => {
                "When you come back to a session after stepping away, Claude Code sends an extra request to recap it."
            }
            BackgroundFeature::GoalCheckIns => {
                "While background work keeps an active goal waiting, Claude Code starts turns that check on it, even when the session sits idle."
            }
            BackgroundFeature::AutoRecaps => {
                "While its terminal is unfocused, Codex sends an extra request to recap the session."
            }
        },
        CheckResult::BashCap(_) => {
            "The longest Claude waits on a foreground Bash command before Claude Code moves it to the background."
        }
        CheckResult::Keepalive(_) => {
            "Pinging an idle session just before its cache expires keeps the cache warm for your return, at the price of the pings; no setting does it, only a /loop task."
        }
        CheckResult::Memory(m) => match m.harness {
            Harness::Claude => {
                "Auto memory lets Claude write notes for itself and loads them into every session."
            }
            Harness::Codex => {
                "Codex memories save what Codex learns in a session and load it into later ones."
            }
        },
        CheckResult::Effort(_) => {
            "How much the main session's model reasons before it answers: a higher effort can do better on hard tasks and writes more output tokens."
        }
    }
}

/// What a built-in Claude Code tool lets Claude do, from Claude Code's tools reference
/// (https://code.claude.com/docs/en/tools-reference, checked 2026-11-15).
pub fn claude_tool(name: &str) -> Option<&'static str> {
    Some(match name {
        "Agent" | "Task" => {
            "Lets Claude start a subagent with its own context window to handle a task."
        }
        "Artifact" => {
            "Lets Claude publish an HTML or Markdown file to claude.ai as an artifact: a private, interactive page you can share."
        }
        "AskUserQuestion" => {
            "Lets Claude ask you multiple-choice questions to settle requirements or ambiguity."
        }
        "Bash" => "Lets Claude run shell commands in your environment.",
        "CronCreate" => "Lets Claude schedule a recurring or one-shot prompt within the session.",
        "CronDelete" => "Lets Claude cancel one of the session's scheduled tasks.",
        "CronList" => "Lets Claude list the session's scheduled tasks.",
        "Edit" => "Lets Claude make targeted edits to files.",
        "EndConversation" => {
            "Lets Claude end the session, in rare cases of sustained abuse or when you ask and confirm."
        }
        "EnterPlanMode" => "Lets Claude switch to plan mode to design an approach before coding.",
        "EnterWorktree" => "Lets Claude create or switch into an isolated git worktree.",
        "ExitPlanMode" => "Lets Claude present a plan for your approval and leave plan mode.",
        "ExitWorktree" => {
            "Lets Claude leave a worktree session and return to the original directory."
        }
        "Glob" => "Lets Claude find files by name pattern.",
        "Grep" => "Lets Claude search file contents for a pattern.",
        "ListAgents" => "Lets Claude list the agents and sessions it can message with SendMessage.",
        "ListMcpResourcesTool" => "Lets Claude list the resources connected MCP servers expose.",
        "LSP" => {
            "Lets Claude use language servers to jump to definitions, find references and see type errors."
        }
        "Monitor" => {
            "Lets Claude run a command in the background and react to each line it prints."
        }
        "NotebookEdit" => "Lets Claude modify Jupyter notebook cells.",
        "PowerShell" => "Lets Claude run PowerShell commands natively.",
        "PushNotification" => {
            "Lets Claude send you a desktop notification, or a phone push with Remote Control."
        }
        "Read" => "Lets Claude read files.",
        "ReadMcpResourceTool" => "Lets Claude read an MCP server's resource by URI.",
        "RemoteTrigger" => {
            "Lets Claude create, run and list cloud scheduled tasks on claude.ai, behind the /schedule command."
        }
        "ReportFindings" => {
            "Lets Claude report code-review findings as a list Claude Code renders, when review instructions ask for it."
        }
        "ScheduleWakeup" => "Lets Claude pick when the next iteration of a self-paced /loop runs.",
        "SendFeedback" => {
            "Lets Claude draft feedback about Claude Code, which you review before anything is sent."
        }
        "SendMessage" => {
            "Lets Claude message another agent: a teammate, a subagent it resumes, or another of your Claude Code sessions."
        }
        "SendUserFile" => "Lets Claude send files from the session to your device.",
        "ShareOnboardingGuide" => {
            "Lets Claude upload ONBOARDING.md and return a link teammates can open, from /team-onboarding."
        }
        "Skill" => "Lets Claude run a skill in the main conversation.",
        "SubagentHandback" => {
            "Lets a subagent in auto mode hand its final report back after the classifier reviews it."
        }
        "TaskCreate" => "Lets Claude add a task to the session's task list.",
        "TaskGet" => "Lets Claude read one task from the session's task list.",
        "TaskList" => "Lets Claude list the session's tasks and their status.",
        "TaskUpdate" => "Lets Claude update or delete a task in the session's task list.",
        "TaskOutput" => {
            "Let Claude read a background task's output; Claude Code removed it in v2.1.277."
        }
        "TaskStop" => "Lets Claude stop a running background task or agent.",
        "TodoWrite" => "Lets Claude keep a checklist of the session's tasks.",
        "ToolSearch" => {
            "Lets Claude find and load deferred tools, such as MCP tools, only when it needs them."
        }
        "WaitForMcpServers" => {
            "Lets Claude wait for MCP servers that are still connecting, when tool search is off."
        }
        "WebFetch" => "Lets Claude fetch the content of a URL.",
        "WebSearch" => "Lets Claude search the web.",
        "Workflow" => {
            "Lets Claude run a dynamic workflow: a script that orchestrates many subagents in the background and returns one result."
        }
        "Write" => "Lets Claude create or overwrite files.",
        _ => return None,
    })
}

/// What a kind of feature is, for a walk question that groups several of them.
pub fn kind(kind: ContextKind) -> &'static str {
    match kind {
        ContextKind::Tool => {
            "Built-in tools the model can call; each one's definition is sent with every request."
        }
        ContextKind::McpServer => {
            "MCP servers connect the model to another program or service; each one's tools are listed in every request."
        }
        ContextKind::McpTool => "Tools from an MCP server, each listed in every request.",
        ContextKind::McpDiscovery => {
            "Listings that tell the model which deferred MCP tools it can load."
        }
        ContextKind::Skill => {
            "Skills are packaged instructions the model can load when a task calls for one; each is listed by name and description in every request."
        }
        ContextKind::ToolOutput => {
            "Tool results kept in the conversation after the calls that returned them."
        }
        ContextKind::Instructions
        | ContextKind::System
        | ContextKind::Memory
        | ContextKind::Other => "Context sent with every request.",
    }
}

/// What a subagent type or agent role is.
pub fn subagent(c: &SubagentCost) -> &'static str {
    match (c.harness, c.kind.as_str()) {
        (_, crate::reader::UNRECORDED) => "Subagents whose records do not name their type.",
        (Harness::Claude, _) if c.fork => {
            "Forks: subagents that start from a copy of the conversation and run on the main model."
        }
        (Harness::Claude, "Explore") => {
            "Claude Code's built-in read-only subagent for searching and reading a codebase."
        }
        (Harness::Claude, "Plan") => {
            "Claude Code's built-in subagent that researches a codebase in plan mode before Claude presents a plan."
        }
        (Harness::Claude, "general-purpose") => {
            "Claude Code's built-in subagent for multi-step tasks that need both research and changes."
        }
        (Harness::Claude, _) => {
            "A subagent type Claude starts with the Agent tool, defined by a plugin or an agent file."
        }
        (Harness::Codex, _) if c.native => "Subagents Codex starts itself for this kind of work.",
        (Harness::Codex, _) => {
            "A Codex agent role: a subagent with its own model and instructions, set in config.toml."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shipped_description_keeps_its_first_sentence_on_one_line() {
        let cases = [
            (
                "Executes a bash command and returns its output.  - `cd` carries over.",
                "Executes a bash command and returns its output.",
            ),
            (
                "Invoke a skill.\n\nA skill is a packaged set.",
                "Invoke a skill.",
            ),
            // A first line without a period is the sentence.
            ("Fetch a page\nwith more", "Fetch a page"),
            (
                "  Search  docs, e.g. guides. Then more.",
                "Search docs, e.g. guides.",
            ),
            ("Version 1.2 ships. More.", "Version 1.2 ships."),
            ("Done!Not a break. Next.", "Done!Not a break."),
        ];
        for (text, want) in cases {
            assert_eq!(sentence(text).as_deref(), Some(want), "{text:?}");
        }
        assert_eq!(sentence(" \n "), None);
        assert_eq!(sentence(""), None);
    }

    #[test]
    fn a_long_sentence_is_cut_at_a_word_within_the_bound() {
        let long = format!("{} end.", "word, ".repeat(60));
        let cut = sentence(&long).unwrap();
        assert!(cut.ends_with("word..."), "{cut}");
        assert!(cut.chars().count() <= MAX_CHARACTERS);
        // A single unbroken word is cut at the bound itself.
        let unbroken = "λ".repeat(MAX_CHARACTERS * 2);
        assert_eq!(
            sentence(&unbroken).unwrap(),
            format!("{}...", "λ".repeat(MAX_CHARACTERS - 3))
        );
    }
}
