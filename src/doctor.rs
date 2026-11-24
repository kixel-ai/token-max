//! `token-max doctor`'s changes: the settings edit behind each recommendation, offered one
//! at a time on a terminal and written to a user settings file only after a yes.
//!
//! Only a profile's own `settings.json` or `config.toml` is edited. A change some other
//! owner would still override, such as a shell variable, a project or managed settings file
//! or T3, is a step to take by hand that names that owner.

use std::fs;
use std::io::{self, BufRead, ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::collections::{IndexMap, IndexSet};
use crate::context::{json_value, setting_text, typed_only};
use crate::mcp::{Definition, Entry, Lock};
use crate::model::{
    BackgroundFeature, BashCap, Billing, Bucket, CheckResult, CompactionCheck, ContextKind,
    Control, Harness, Margin, Profile, Reowned, Report, Setting, Verdict,
};
use crate::report::{
    AT_LEAST, Choice, Listing, Origin, Recommendation, Source, UP_TO, about, approx, capitalize,
    recommendations, share_text,
};
use crate::safe::{ScanError, terminal_text};
use crate::terminal::{BOLD, DIM, GREEN, RED, WrapOptions, YELLOW, wrap_spans};

/// Whose spend a share is of, for a host's own changes.
const THIS_HOST: &str = "this host's";

/// One recommendation and how to carry it out.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fix {
    /// The report line's name, such as "Main-agent prompt cache TTL".
    pub name: String,
    /// The current value and the one to change to, such as "5m -> 1h".
    pub value: String,
    /// Worth considering, rather than a recommended change.
    pub suggested: bool,
    /// What it saves every 30 days, when measured.
    pub saving: Option<f64>,
    /// How far `saving` may be off when extrapolated over unpriced requests.
    pub margin: Option<Margin>,
    /// How the saving reads, such as "saves up to".
    #[serde(deserialize_with = "known")]
    pub verb: Known,
    /// The saving's share of this host's spend every 30 days.
    pub share: Option<f64>,
    /// What the setting or feature does, from the report line.
    pub what: String,
    /// What was measured and why the change makes sense, from the report line's evidence.
    pub evidence: String,
    pub how: How,
    /// The report lines it comes from: the first, and any other line making the same edit.
    pub origins: Vec<Origin>,
    /// The changes of its kind and reason it is offered with when it saves little; none
    /// for a change that stands alone, such as a check.
    pub group: Option<Group>,
    /// Where a never-used MCP server it turns off is configured, which the walk offers to
    /// remove after a yes; empty for any other change.
    pub removal: Vec<Definition>,
}

/// A saving verb. Named apart from `&'static str`, which serde would borrow from the input
/// it reads.
pub type Known = &'static str;

/// A saving verb read back from another host, which must be one this build prints.
fn known<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<&'static str, D::Error> {
    const KNOWN: [&str; 3] = ["saves", UP_TO, AT_LEAST];
    let text = String::deserialize(deserializer)?;
    KNOWN
        .into_iter()
        .find(|k| *k == text)
        .ok_or_else(|| serde::de::Error::custom("unknown saving verb"))
}

/// Small changes the walk offers together: features of one kind changed for one reason,
/// such as never-used MCP servers turned off.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub kind: ContextKind,
    pub reason: Reason,
    /// The group's heading, such as "Never-used MCP servers".
    pub label: String,
    /// This member's name in the group's list, such as the server's name.
    pub member: String,
    /// Where the member was found, such as a skill's SKILL.md, which tells apart two
    /// members with one name.
    pub location: Option<String>,
}

impl Group {
    /// Whether two changes in these groups belong to one: the same kind and reason, and
    /// so the same heading. Their changes must match too.
    fn joins(&self, other: &Group) -> bool {
        self.kind == other.kind && self.reason == other.reason && self.label == other.label
    }
}

/// Why a feature is offered off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    /// Never used across a window that would have recorded a use.
    Unused,
    /// No use recorded, but the window cannot rule one out.
    Unrecorded,
    /// Used, but it cost more than `--max-cost-per-use` per use.
    Costly,
    /// A redundant copy of a catalog skill whose most-used copy stays, used or not.
    Duplicate,
}

/// Whether the doctor can make a change, and what it is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum How {
    /// Edits to a profile's own settings file.
    Edit(Patch),
    /// A change only the person can make: what to do, and why the doctor cannot.
    Manual { step: String, reason: String },
}

/// Edits to one settings file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Patch {
    pub file: String,
    /// Claude's `settings.json` is JSON; Codex's `config.toml` is TOML.
    pub harness: Harness,
    pub edits: Vec<Edit>,
}

/// Set `path` to `value`, add `value` to the list at `path`, or remove `path`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edit {
    pub path: Vec<String>,
    /// Null for a removal.
    pub value: Value,
    pub op: Op,
}

/// What an edit does to its setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    Set,
    /// Add the value to the list, once.
    Append,
    Remove,
}

impl From<&Control> for Edit {
    fn from(control: &Control) -> Self {
        Edit {
            path: control.path.clone(),
            value: json_value(&control.value),
            op: if control.append { Op::Append } else { Op::Set },
        }
    }
}

impl Edit {
    fn set(path: &str, value: impl Into<Value>) -> Self {
        Edit {
            path: path.split('.').map(String::from).collect(),
            value: value.into(),
            op: Op::Set,
        }
    }

    /// Remove `path`, given split on dots.
    fn remove(path: &[&str]) -> Self {
        Edit {
            path: path.iter().map(|p| p.to_string()).collect(),
            value: Value::Null,
            op: Op::Remove,
        }
    }

    /// The edit as a settings fragment, such as `"promptCacheTtl": "1h"`.
    pub fn text(&self, harness: Harness) -> String {
        match self.op {
            Op::Set => setting_text(&self.path, self.value.clone(), false, harness),
            Op::Append => format!(
                "{} (added to the list)",
                setting_text(&self.path, self.value.clone(), true, harness)
            ),
            Op::Remove => format!("remove {}", self.path.join(".")),
        }
    }
}

/// Every change the report recommends or suggests, biggest saving first, each with its
/// edit or the step to take by hand. An edit another line already makes is listed once,
/// naming both lines. Each fix's description and evidence are empty until `explain` fills them
/// from the report.
pub fn fixes(report: &Report, unicode: bool) -> Vec<Fix> {
    fixes_from(
        recommendations(report, unicode)
            .into_iter()
            .map(|line| fix(report, line))
            .collect(),
    )
}

/// Drop repeated edits, and leave the second of two values for one setting by hand.
pub(crate) fn fixes_from(all: Vec<Fix>) -> Vec<Fix> {
    let mut fixes: Vec<Fix> = vec![];
    for mut fix in all {
        if let Some(same) = fixes
            .iter_mut()
            .find(|f| matches!(f.how, How::Edit(_)) && f.how == fix.how)
        {
            same.origins.extend(fix.origins);
            continue;
        }
        // Two lines that want one setting at different values, such as checks for two
        // projects that share the user file, leave the choice to the person.
        if let How::Edit(patch) = &fix.how
            && let Some(other) = fixes.iter().find(|f| conflicts(&f.how, patch))
        {
            fix.how = How::Manual {
                step: steps(patch),
                reason: format!(
                    "{} wants a different value for the same setting",
                    other.name
                ),
            };
        }
        fixes.push(fix);
    }
    fixes
}

/// Whether `how` sets a setting `patch` also sets, to another value, in the same file or
/// one a symlink makes the same.
fn conflicts(how: &How, patch: &Patch) -> bool {
    let How::Edit(other) = how else { return false };
    (other.file == patch.file
        || identity(Path::new(&other.file)) == identity(Path::new(&patch.file)))
        && other.edits.iter().any(|a| {
            patch.edits.iter().any(|b| {
                a.op != Op::Append
                    && b.op != Op::Append
                    && a.path == b.path
                    && (a.op != b.op || a.value != b.value)
            })
        })
}

/// The edits as steps to take by hand.
fn steps(patch: &Patch) -> String {
    steps_in(patch, &patch.file)
}

/// The edits as steps to take by hand in `file`, such as the end of a symlink.
fn steps_in(patch: &Patch, file: &str) -> String {
    let (removals, sets): (Vec<&Edit>, Vec<&Edit>) =
        patch.edits.iter().partition(|e| e.op == Op::Remove);
    let text = |edits: Vec<&Edit>| -> Vec<String> {
        edits.iter().map(|e| e.text(patch.harness)).collect()
    };
    let mut steps = vec![];
    if !sets.is_empty() {
        steps.push(format!("set {}", text(sets).join(" and ")));
    }
    steps.extend(text(removals));
    format!("in {file}, {}", steps.join(", and "))
}

/// A path as the filesystem resolves it, so two names for one file compare equal; a file
/// that does not exist yet resolves through its directory.
fn identity(path: &Path) -> PathBuf {
    if let Ok(path) = fs::canonicalize(path) {
        return path;
    }
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) if !dir.as_os_str().is_empty() => {
            fs::canonicalize(dir).map_or_else(|_| path.to_path_buf(), |dir| dir.join(name))
        }
        _ => path.to_path_buf(),
    }
}

fn fix(report: &Report, line: Recommendation<'_>) -> Fix {
    // Every profile on the line, the one whose controls it reports first.
    let ids: Vec<&str> = match line.source {
        Source::Check(result) => {
            let (lead, all) = check_profiles(result);
            std::iter::once(lead)
                .chain(all.iter().map(String::as_str).filter(|id| *id != lead))
                .collect()
        }
        Source::Piece(c) => c.profiles.iter().map(String::as_str).collect(),
    };
    let profile_id = ids.first().copied().unwrap_or_default();
    let members: Vec<&Profile> = ids
        .iter()
        .filter_map(|id| report.scan.profiles.iter().find(|p| p.id == *id))
        .collect();
    let group = group(report, &line);
    let mut removal = vec![];
    let how = match (members.first(), line.source) {
        (None, _) => How::Manual {
            step: format!("change it where {profile_id} is configured"),
            reason: "its profile is no longer selected".into(),
        },
        (Some(profile), Source::Check(result)) => check_how(result, profile),
        (Some(_), Source::Piece(c)) => match &c.control {
            Some(control) => {
                let how = switch(&members, control.into(), report.scan.defaults);
                let unused = group.as_ref().is_some_and(|g| {
                    g.kind == ContextKind::McpServer && g.reason == Reason::Unused
                });
                if unused && matches!(how, How::Edit(_)) {
                    removal = crate::mcp::definitions(&c.name, control, &members);
                }
                how
            }
            None => How::Manual {
                step: "disable it in config.toml with [[skills.config]]".into(),
                reason: "the recorded catalog does not give its SKILL.md path".into(),
            },
        },
    };
    Fix {
        name: line.name,
        value: line.value,
        suggested: line.suggested,
        saving: line.saving,
        margin: line.margin,
        verb: line.verb,
        share: line.share,
        what: String::new(),
        evidence: String::new(),
        how,
        origins: vec![line.origin],
        group,
        removal,
    }
}

/// The group a feature's change joins: its kind and why it is offered, as its verdict says.
fn group(report: &Report, line: &Recommendation<'_>) -> Option<Group> {
    let Source::Piece(c) = line.source else {
        return None;
    };
    let kind = match c.kind {
        ContextKind::Tool => "tools",
        ContextKind::McpServer => "MCP servers",
        ContextKind::McpTool => "MCP tools",
        ContextKind::McpDiscovery => "MCP discovery listings",
        ContextKind::Skill => "skills",
        ContextKind::ToolOutput => "tool outputs",
        ContextKind::Instructions
        | ContextKind::System
        | ContextKind::Memory
        | ContextKind::Other => return None,
    };
    let duplicate = c
        .skill
        .as_ref()
        .is_some_and(|s| s.retained_path.is_some() && s.path != s.retained_path);
    let reason = match (c.verdict?, c.calls) {
        (Verdict::Off, _) if duplicate => Reason::Duplicate,
        (Verdict::Off, _) => Reason::Unused,
        (Verdict::Consider, Some(0)) => Reason::Unrecorded,
        (Verdict::Consider, Some(_)) => Reason::Costly,
        _ => return None,
    };
    let what = match reason {
        // A skill left typeable counts only the model's uses.
        Reason::Unused if typed_only(c.control.as_ref()) => {
            format!("{} the model never used", capitalize(kind))
        }
        Reason::Unused => format!("Never-used {kind}"),
        Reason::Unrecorded => format!("{} with no recorded use", capitalize(kind)),
        Reason::Costly => format!(
            "{} over {} per use",
            capitalize(kind),
            about(report.max_cost_per_use_usd)
        ),
        Reason::Duplicate => format!("Duplicate copies of {kind}"),
    };
    Some(Group {
        kind: c.kind,
        reason,
        label: what,
        member: c.name.clone(),
        location: c.skill.as_ref().and_then(|s| s.path.clone()),
    })
}

/// Fill each fix's description and evidence from the report lines it comes from.
pub fn explain(fixes: &mut [Fix], listing: &Listing) {
    for fix in fixes {
        fix.what = listing.what(fix.origins[0]);
        fix.evidence = listing.evidence(fix.origins[0]);
    }
}

/// A tool, skill or MCP switch in the line's settings file, unless a file that outranks it
/// for some of its profiles' sessions sets the same switch, which would keep it on there. A
/// line whose shared history spans several settings files is switched off in each by hand.
/// Under `defaults`, the user's own files set nothing, so only policy can outrank them.
fn switch(members: &[&Profile], edit: Edit, defaults: bool) -> How {
    let profile = members[0];
    let file = &profile.settings_file;
    // Two names a symlink makes one file are one file.
    let mut files: IndexMap<String, &str> = IndexMap::default();
    for m in members {
        files
            .entry(
                crate::discovery::realpath(&m.settings_file)
                    .unwrap_or_else(|_| m.settings_file.clone()),
            )
            .or_insert(m.settings_file.as_str());
    }
    let patch = Patch {
        file: file.clone(),
        harness: profile.harness,
        edits: vec![edit],
    };
    let owners = members
        .iter()
        .map(|m| outranking(m, &patch.edits[0], defaults))
        .collect::<Result<Vec<_>, _>>()
        .map(|all| all.into_iter().flatten().collect::<IndexSet<String>>());
    if files.len() > 1 {
        let all: Vec<&str> = files.into_values().collect();
        let also = match &owners {
            Ok(o) if o.is_empty() => String::new(),
            Ok(o) => format!(
                ", and in {} where it is set",
                o.iter().map(String::as_str).collect::<Vec<_>>().join(", ")
            ),
            Err(_) => ", and in any file that outranks them".into(),
        };
        return How::Manual {
            step: format!(
                "in {}{also}, set {}",
                all.join(" and "),
                patch.edits[0].text(profile.harness)
            ),
            reason: "its requests were copied into the histories of several profiles, so which one made them is unknown".into(),
        };
    }
    match owners.map(|o| o.into_iter().collect::<Vec<_>>()) {
        Ok(owners) if owners.is_empty() => How::Edit(patch),
        Ok(owners) => {
            let text = patch.edits[0].text(profile.harness);
            let them = owners.join(", ");
            let verb = if owners.len() == 1 { "sets" } else { "set" };
            How::Manual {
                step: format!("in {file}, and in {them} where it is set, set {text}"),
                reason: format!("{them} {verb} it too, which outranks {file}"),
            }
        }
        Err(e) => {
            let at = e
                .location
                .map_or_else(String::new, |l| format!(" {}", l.file));
            How::Manual {
                step: steps(&patch),
                reason: format!(
                    "{}{at}, so whether it overrides {file} is unknown",
                    e.message
                ),
            }
        }
    }
}

/// The files, or legacy Codex profile tables, that outrank the profile's settings file and
/// set what `edit` sets. Claude's `permissions.deny` rules from every file apply together,
/// so a rule added to the user file is never outranked.
fn outranking(profile: &Profile, edit: &Edit, defaults: bool) -> Result<Vec<String>, ScanError> {
    if profile.harness == Harness::Claude && edit.op == Op::Append {
        return Ok(vec![]);
    }
    let mut owners = vec![];
    for file in &profile.outranking_files {
        if sets(&crate::discovery::settings(file, profile.harness)?, edit) {
            owners.push(file.clone());
        }
    }
    if profile.harness == Harness::Codex && !defaults {
        let user = crate::discovery::settings(&profile.settings_file, Harness::Codex)?;
        for (name, table) in crate::controls::members(user.get("profiles")) {
            if sets(crate::controls::members(Some(table)), edit) {
                owners.push(format!("{}:profiles.{name}", profile.settings_file));
            }
        }
    }
    Ok(owners)
}

/// Whether a file that outranks the profile's own sets the switch `control` flips, or
/// whether that can't be read.
///
/// Any such file counts, even one that a higher file overrides back: the maintainer chose
/// to keep offering the switch in that case rather than resolve each chain here.
pub(crate) fn outranked(profile: &Profile, control: &Control) -> bool {
    let edit = Edit::from(control);
    // Claude's deny rules from every file apply together.
    if profile.harness == Harness::Claude && edit.op == Op::Append {
        return false;
    }
    let differs = |config: &Map<String, Value>| sets(config, &edit) && !sets_to(config, &edit);
    for file in &profile.outranking_files {
        match crate::discovery::settings(file, profile.harness) {
            Ok(config) if !differs(&config) => {}
            _ => return true,
        }
    }
    if profile.harness == Harness::Codex {
        let Ok(user) = crate::discovery::settings(&profile.settings_file, Harness::Codex) else {
            return true;
        };
        for (_, table) in crate::controls::members(user.get("profiles")) {
            if differs(crate::controls::members(Some(table))) {
                return true;
            }
        }
    }
    false
}

/// Whether `config` sets what `edit` sets to the same value: the key's value, or for a Codex
/// `[[skills.config]]` entry the one with the same `path`.
fn sets_to(config: &Map<String, Value>, edit: &Edit) -> bool {
    let mut value = config.get(&edit.path[0]);
    for key in &edit.path[1..] {
        value = value.and_then(|v| v.get(key));
    }
    match (edit.op == Op::Append, value) {
        (true, Some(Value::Array(items))) => items.iter().any(|i| match edit.value.get("path") {
            Some(path) => {
                i.get("path") == Some(path) && i.get("enabled") == edit.value.get("enabled")
            }
            None => *i == edit.value,
        }),
        (false, Some(value)) => *value == edit.value,
        _ => false,
    }
}

/// Whether `config` sets what `edit` sets: the same key, or for a list a Codex
/// `[[skills.config]]` entry with the same `path`.
fn sets(config: &Map<String, Value>, edit: &Edit) -> bool {
    let Some((last, parents)) = edit.path.split_last() else {
        return false;
    };
    let mut table = config;
    for key in parents {
        match table.get(key) {
            Some(Value::Object(next)) => table = next,
            _ => return false,
        }
    }
    let Some(value) = table.get(last) else {
        return false;
    };
    // A Codex skill entry is matched by its `path`; any other list a file sets replaces the
    // user's.
    match (edit.op == Op::Append, edit.value.get("path"), value) {
        (true, Some(key), Value::Array(items)) => items.iter().any(|i| i.get("path") == Some(key)),
        (true, Some(_), _) => false,
        _ => true,
    }
}

/// The profile whose controls a check reports, and every profile its line covers.
fn check_profiles(result: &CheckResult) -> (&str, &[String]) {
    match result {
        CheckResult::Ttl(c) => (&c.base.profile, &c.base.profiles),
        CheckResult::Compaction(c) => (&c.base.profile, &c.base.profiles),
        CheckResult::TurnEnd(c) => (&c.base.profile, &c.base.profiles),
        CheckResult::Idle(c) => (&c.base.profile, &c.base.profiles),
        CheckResult::T3Idle(c) => (&c.base.profile, &c.base.profiles),
        CheckResult::Speed(c) => (&c.base.profile, &c.base.profiles),
        CheckResult::Background(c) => (&c.base.profile, &c.base.profiles),
        CheckResult::Keepalive(c) => (&c.base.profile, &c.base.profiles),
        CheckResult::Memory(c) => (&c.base.profile, &c.base.profiles),
        CheckResult::Effort(c) => (&c.base.profile, &c.base.profiles),
    }
}

/// Whether `owner` is `key` in `file`, as `{file}:{key}`, maybe followed by a note such as
/// ` (default)`. Owners name the setting that decides the current value, so editing it is
/// enough: nothing of higher precedence sets it.
fn names(owner: &str, file: &str, key: &str) -> bool {
    owner
        .strip_prefix(file)
        .and_then(|rest| rest.strip_prefix(':'))
        .and_then(|rest| rest.strip_prefix(key))
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(" ("))
}

/// The edits for the first `(key, edits)` whose key owns the setting, or else a step by hand.
fn owned(
    owner: &str,
    profile: &Profile,
    candidates: Vec<(String, Vec<Edit>)>,
    step: String,
) -> How {
    let file = &profile.settings_file;
    for (key, edits) in candidates {
        if names(owner, file, &key) {
            return How::Edit(Patch {
                file: file.clone(),
                harness: profile.harness,
                edits,
            });
        }
    }
    manual(owner, profile, step)
}

/// A change the owner's precedence keeps out of the profile's settings file.
fn manual(owner: &str, profile: &Profile, step: String) -> How {
    let file = &profile.settings_file;
    let variable = owner.split(" (").next().unwrap_or(owner);
    let reason = if !variable.is_empty()
        && variable
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        format!(
            "the {variable} environment variable sets it and outranks {file}; change it where it is exported"
        )
    } else if profile.t3.is_some() && !owner.starts_with(&format!("{file}:")) {
        format!("T3 sets it ({owner}), and token-max does not edit T3 settings")
    } else if owner.starts_with(&format!("{file}:")) {
        format!("token-max does not know how to edit {owner}")
    } else {
        format!("{owner} sets it; token-max edits only {file}")
    };
    How::Manual { step, reason }
}

/// How to move a Bash timeout cap to the one suggested, in `profile`'s settings; None when
/// it suggests no other cap.
pub(crate) fn bash_how(cap: &BashCap, profile: &Profile) -> Option<How> {
    let to = cap.suggested.filter(|to| *to != cap.current)?.to_string();
    let file = profile.settings_file.as_str();
    let step = format!("set {}", cap.replacement);
    let max = "BASH_MAX_TIMEOUT_MS";
    let mut how = owned(
        &cap.owner,
        profile,
        vec![(format!("env.{max}"), vec![env(max, &to)])],
        step.clone(),
    );
    // A default above the cap raises the cap again, so it comes down too, in the one edit.
    let default = "BASH_DEFAULT_TIMEOUT_MS";
    if let (How::Edit(patch), Some(control)) = (&mut how, &profile.bash)
        && cap.replacement.contains(default)
    {
        if !names(&control.default_owner, file, &format!("env.{default}")) {
            return Some(manual(&control.default_owner, profile, step));
        }
        patch.edits.push(env(default, &to));
    }
    Some(how)
}

fn check_how(result: &CheckResult, profile: &Profile) -> How {
    let file = profile.settings_file.as_str();
    match result {
        CheckResult::Ttl(t) => {
            let Some(ttl) = t.base.recommended else {
                return unknown(&t.base.owner);
            };
            let (key, variable) = match t.bucket {
                Bucket::Main => ("promptCacheTtl", "CLAUDE_CODE_PROMPT_CACHE_TTL"),
                Bucket::Subagent => (
                    "subagentPromptCacheTtl",
                    "CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL",
                ),
            };
            let set = vec![Edit::set(key, ttl.as_str())];
            owned(
                &t.base.owner,
                profile,
                vec![
                    (key.into(), set.clone()),
                    // The setting outranks this variable, so setting it is enough.
                    ("env.ENABLE_PROMPT_CACHING_1H".into(), set),
                    (format!("env.{variable}"), vec![env(variable, ttl.as_str())]),
                ],
                format!("set {} to {}", t.base.owner, ttl.as_str()),
            )
        }
        CheckResult::Compaction(c) => {
            let Some(window) = c.base.recommended else {
                return unknown(&c.base.owner);
            };
            match &c.reowned {
                Some(reowned) if c.base.current != Some(window) => reown(reowned, profile),
                _ => window_how(c, window, profile),
            }
        }
        CheckResult::TurnEnd(c) => {
            let Some(percent) = c.base.recommended else {
                return unknown(&c.base.owner);
            };
            let key = crate::controls::TURN_END_KEY;
            owned(
                &c.base.owner,
                profile,
                vec![(key.into(), vec![Edit::set(key, percent)])],
                format!("set {} to {percent}", c.base.owner),
            )
        }
        CheckResult::Idle(c) => {
            let Some(on) = c.base.recommended else {
                return unknown(&c.base.owner);
            };
            owned(
                &c.base.owner,
                profile,
                vec![(
                    "idleCompaction".into(),
                    vec![Edit::set("idleCompaction", on)],
                )],
                format!("set {} to {on}", c.base.owner),
            )
        }
        CheckResult::T3Idle(c) => {
            let mode = c.base.recommended.map_or("unknown", |m| m.as_str());
            How::Manual {
                step: format!("set {} to {mode}", c.base.owner),
                reason: "token-max does not edit T3 settings".into(),
            }
        }
        CheckResult::Speed(s) => {
            let candidates = match profile.harness {
                Harness::Claude => vec![("fastMode".into(), vec![Edit::set("fastMode", false)])],
                Harness::Codex => vec![(
                    "service_tier".into(),
                    vec![Edit::set("service_tier", "default")],
                )],
            };
            owned(&s.base.owner, profile, candidates, s.replacement.clone())
        }
        CheckResult::Background(b) => {
            let (key, variable, off) = match b.feature {
                BackgroundFeature::PromptSuggestions => (
                    Some("promptSuggestionEnabled"),
                    "CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION",
                    "false",
                ),
                BackgroundFeature::SessionRecaps => (
                    Some("awaySummaryEnabled"),
                    "CLAUDE_CODE_ENABLE_AWAY_SUMMARY",
                    "0",
                ),
                BackgroundFeature::GoalCheckIns => (None, "CLAUDE_CODE_GOAL_CHECKIN_MINUTES", "0"),
                BackgroundFeature::AutoRecaps => (Some("tui.auto_recap"), "", ""),
            };
            let owner = &b.base.owner;
            // Nothing sets this variable, so the profile's settings env can.
            if owner == &format!("{variable} (default)") {
                return How::Edit(Patch {
                    file: file.into(),
                    harness: profile.harness,
                    edits: vec![env(variable, off)],
                });
            }
            let mut candidates: Vec<(String, Vec<Edit>)> = key
                .map(|key| (key.to_string(), vec![Edit::set(key, false)]))
                .into_iter()
                .collect();
            if !variable.is_empty() {
                candidates.push((format!("env.{variable}"), vec![env(variable, off)]));
            }
            owned(
                owner,
                profile,
                candidates,
                off_step(&b.off, b.off_file.as_deref()),
            )
        }
        CheckResult::Memory(m) => {
            // Another file may set memories for some runs, such as a named profile or a
            // project whose trust is unrecorded: the user file alone may not turn it off.
            if m.unresolved.is_some() || !m.overrides.is_empty() {
                let mut step = off_step(&m.off, m.off_file.as_deref());
                if !m.overrides.is_empty() {
                    step = format!(
                        "{step}, and in {} where it sets memories",
                        m.overrides.join(", ")
                    );
                }
                let reason = match &m.unresolved {
                    Some(why) => why.clone(),
                    None => format!("{} may set memories too", m.overrides.join(", ")),
                };
                return How::Manual { step, reason };
            }
            let candidates = match m.harness {
                Harness::Claude => vec![
                    (
                        "autoMemoryEnabled".into(),
                        vec![Edit::set("autoMemoryEnabled", false)],
                    ),
                    (
                        "env.CLAUDE_CODE_DISABLE_AUTO_MEMORY".into(),
                        vec![env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1")],
                    ),
                ],
                Harness::Codex => vec![(
                    "features.memories".into(),
                    vec![Edit::set("features.memories", false)],
                )],
            };
            owned(
                &m.base.owner,
                profile,
                candidates,
                off_step(&m.off, m.off_file.as_deref()),
            )
        }
        CheckResult::Keepalive(k) => unknown(&k.base.owner),
        // Effort lines give no verdict, so they are never offered.
        CheckResult::Effort(e) => unknown(&e.base.owner),
    }
}

/// The edit that sets a compaction window where its owner holds it.
fn window_how(c: &CompactionCheck, window: i64, profile: &Profile) -> How {
    let file = profile.settings_file.as_str();
    let owner = &c.base.owner;

    let step = format!("set {owner} to {window}");
    if profile.harness == Harness::Codex {
        let key = "model_auto_compact_token_limit";
        return owned(
            owner,
            profile,
            vec![(key.into(), vec![Edit::set(key, window)])],
            step,
        );
    }
    let mut candidates = vec![
        (
            "autoCompactWindow".into(),
            vec![Edit::set("autoCompactWindow", window)],
        ),
        (
            "env.CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(),
            vec![env("CLAUDE_CODE_AUTO_COMPACT_WINDOW", &window.to_string())],
        ),
    ];
    // A model's own row, which owners name whole; model IDs may hold dots.
    let row = owner
        .strip_prefix(&format!("{file}:modelSettings."))
        .map(|rest| rest.split(" (").next().unwrap_or(rest))
        .and_then(|rest| rest.strip_suffix(".autoCompactWindow"));
    if let Some(model) = row {
        candidates.push((
            format!("modelSettings.{model}.autoCompactWindow"),
            vec![Edit {
                path: vec![
                    "modelSettings".into(),
                    model.into(),
                    "autoCompactWindow".into(),
                ],
                value: window.into(),
                op: Op::Set,
            }],
        ));
    }
    owned(owner, profile, candidates, step)
}

/// The edits that change a window under `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` so one setting owns
/// every trigger: each change the plan checked, as an edit to the profile's settings file, or a
/// step by hand for any other owner, which leaves the whole change by hand.
fn reown(reowned: &Reowned, profile: &Profile) -> How {
    let file = profile.settings_file.as_str();
    let mut foreign: Vec<(String, String)> = vec![];
    let mut edits = vec![];
    for change in &reowned.changes {
        let owner = change.owner();
        let mine = matches!(&change.source, crate::model::Source::File(f) if f == file);
        if !mine {
            let step = match (&change.source, &change.setting, change.value) {
                // T3 keeps an instance's variables as entries named in a list.
                (crate::model::Source::Instance { file, id }, Setting::Env(key), value) => {
                    let list = format!("{file}:providerInstances.{id}.environment");
                    match value {
                        Some(value) => format!("set the {key} entry in {list} to {value}"),
                        None => format!("remove the {key} entry from {list}"),
                    }
                }
                (_, _, Some(value)) => format!("set {owner} to {value}"),
                (_, _, None) => format!("remove {owner}"),
            };
            foreign.push((owner, step));
            continue;
        }
        let path: Vec<String> = match &change.setting {
            Setting::Window => vec!["autoCompactWindow".into()],
            Setting::Row(model) => vec![
                "modelSettings".into(),
                model.to_string(),
                "autoCompactWindow".into(),
            ],
            Setting::Env(key) => vec!["env".into(), key.clone()],
        };
        edits.push(match (change.value, &change.setting) {
            (None, _) => Edit::remove(&path.iter().map(String::as_str).collect::<Vec<_>>()),
            // A Claude settings file's `env` block holds strings.
            (Some(value), Setting::Env(_)) => Edit {
                path,
                value: value.to_string().into(),
                op: Op::Set,
            },
            (Some(value), _) => Edit {
                path,
                value: value.into(),
                op: Op::Set,
            },
        });
    }
    let patch = Patch {
        file: file.into(),
        harness: profile.harness,
        edits,
    };
    let Some((first, _)) = foreign.first() else {
        return How::Edit(patch);
    };
    let mut steps: Vec<String> = foreign.iter().map(|(_, step)| step.clone()).collect();
    if !patch.edits.is_empty() {
        steps.insert(0, self::steps(&patch));
    }
    let step = steps.join("; ");
    match manual(first, profile, String::new()) {
        How::Manual { reason, .. } => How::Manual { step, reason },
        How::Edit(_) => unreachable!("manual is never an edit"),
    }
}

/// A variable in a Claude settings file's `env` block, which holds strings.
fn env(variable: &str, value: &str) -> Edit {
    Edit {
        path: vec!["env".into(), variable.into()],
        value: value.into(),
        op: Op::Set,
    }
}

/// The step that turns a feature off, from the setting and the file it belongs in.
fn off_step(off: &str, file: Option<&str>) -> String {
    match file {
        Some(file) => format!("set {off} in {file}"),
        None => format!("set {off} where the environment is configured"),
    }
}

/// A recommendation without a value to set, which no current check produces.
fn unknown(owner: &str) -> How {
    How::Manual {
        step: format!("change {owner}"),
        reason: "the recommended value is unknown".into(),
    }
}

/// A fix as lines of text: a heading, then its edit or its step by hand.
#[cfg(test)]
pub struct Described {
    pub head: String,
    /// Each line, and whether it is a settings fragment that must not wrap.
    pub body: Vec<(String, bool)>,
}

/// How a fix reads as text, with paths under `home` shown from `~`.
#[cfg(test)]
pub fn describe(fix: &Fix, home: Option<&str>) -> Described {
    let kind = if fix.suggested {
        "worth considering"
    } else {
        "recommended"
    };
    let head = format!("{}: {} ({kind})", fix.name, fix.value);
    let body = match &fix.how {
        How::Edit(patch) => {
            let mut body = vec![(
                format!("In {}:", crate::report::home_path(home, &patch.file)),
                false,
            )];
            body.extend(
                patch
                    .edits
                    .iter()
                    .map(|e| (format!("  {}", e.text(patch.harness)), true)),
            );
            body
        }
        How::Manual { step, reason } => {
            vec![(
                home_in(&format!("By hand, since {reason}: {step}."), home),
                false,
            )]
        }
    };
    Described { head, body }
}

/// A fix on one line, for the report's list: its edits and file, or its step by hand.
pub fn setting(fix: &Fix, home: Option<&str>) -> String {
    match &fix.how {
        How::Edit(patch) => {
            let edits: Vec<String> = patch.edits.iter().map(|e| e.text(patch.harness)).collect();
            format!(
                "{} in {}",
                edits.join(" and "),
                crate::report::home_path(home, &patch.file)
            )
        }
        How::Manual { step, .. } => format!("by hand: {}", home_in(step, home)),
    }
}

/// How the walk prints.
#[derive(Clone)]
pub struct Style {
    pub color: bool,
    /// Use Unicode decorations such as arrows.
    pub unicode: bool,
    pub width: Option<usize>,
    /// The home directory, which paths are shown from as `~`.
    pub home: Option<String>,
    /// The SSH destination whose files these are, which names them; none for this host's.
    pub host: Option<String>,
}

/// Cells every line of the walk is indented by, as the report's are.
const MARGIN: usize = crate::report::MARGIN;
/// A card's figure is aligned to the right of this many cells, or of a narrower terminal,
/// and its paragraphs wrap there.
const CARD_CELLS: usize = 88;

/// A question of the walk as a card: a heading with its figure at the right, the change,
/// then labeled paragraphs.
pub(crate) struct Card<'a> {
    /// Such as "[1/4]".
    pub(crate) counter: String,
    pub(crate) name: &'a str,
    /// What it saves, such as "saves $206/mo", and its share of this host's spend.
    pub(crate) saving: Option<(String, String)>,
    /// Shown instead of a saving, such as "saving not measured".
    pub(crate) no_saving: &'a str,
    /// The current value and the offered one, such as "on → off"; empty with none to offer.
    pub(crate) value: String,
    pub(crate) standing: Standing,
    /// Labeled paragraphs, such as ("What", ...) and ("Why", ...).
    pub(crate) rows: Vec<(&'a str, String)>,
}

/// How strongly a card's change is put.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standing {
    /// Measured to save.
    Recommended,
    /// Measured, with a cost only the person can weigh.
    Considered,
    /// `vibes`' advice, which nothing measured backs: no verdict.
    Advice,
}

impl Standing {
    fn of(suggested: bool) -> Self {
        if suggested {
            Standing::Considered
        } else {
            Standing::Recommended
        }
    }
}

/// A prompt: its question, and whether it offers to split a group into its changes.
pub(crate) struct Prompt {
    text: String,
    split: bool,
}

impl Style {
    /// Spans wrapped `indent` cells in, rows after the first at `continuation` when given,
    /// each styled on a color terminal, ending with a line break.
    pub(crate) fn spans(
        &self,
        spans: &[(&str, &str)],
        width: Option<usize>,
        indent: usize,
        continuation: Option<usize>,
    ) -> String {
        let escaped: Vec<(String, &str)> = spans
            .iter()
            .map(|(text, style)| (terminal_text(text), *style))
            .collect();
        let escaped: Vec<(&str, &str)> = escaped.iter().map(|(t, s)| (t.as_str(), *s)).collect();
        let options = WrapOptions {
            width,
            indent,
            continuation,
        };
        format!("{}\n", wrap_spans(&escaped, options, self.color))
    }

    /// A line of the walk, inside its margin.
    pub(crate) fn line(&self, text: &str) -> String {
        self.spans(&[(text, "")], self.width, MARGIN, None)
    }

    /// Secondary text, such as what the walk left out.
    pub(crate) fn dim(&self, text: &str) -> String {
        self.spans(&[(text, DIM)], self.width, MARGIN, None)
    }

    /// A raw line, such as a settings fragment, that stays whole so it can be copied.
    fn raw(&self, text: &str, indent: usize, style: &str) -> String {
        let text = terminal_text(text);
        let pad = " ".repeat(indent);
        if self.color && !style.is_empty() {
            format!("{pad}\x1b[{style}m{text}\x1b[0m\n")
        } else {
            format!("{pad}{text}\n")
        }
    }

    /// The walk's heading: how many questions about how many changes, then that nothing is
    /// written before the confirm, with a blank line around it.
    pub(crate) fn headline(&self, text: &str) -> String {
        format!(
            "\n{}{}",
            self.spans(&[(text, BOLD)], self.width, MARGIN, None),
            self.dim("Nothing is written until you confirm the list at the end.")
        )
    }

    /// A question, bold, then its answers, dim.
    pub(crate) fn prompt(&self, question: &str, split: bool) -> Prompt {
        let options = if split {
            "[y]es, [n]o, [s]plit, [q]uit"
        } else {
            "[y]es, [n]o, [q]uit"
        };
        Prompt {
            text: self.ask(question, options),
            split,
        }
    }

    /// `question` and its `options` as the walk asks them, ready for an answer on the line.
    fn ask(&self, question: &str, options: &str) -> String {
        let text = self.spans(
            &[(question, BOLD), (" ", ""), (options, DIM), (":", "")],
            self.width,
            MARGIN,
            None,
        );
        format!("{} ", text.trim_end_matches('\n'))
    }

    fn arrow(&self) -> &'static str {
        if self.unicode { "→" } else { "->" }
    }

    /// The cells a card lays out in.
    fn card_width(&self) -> Option<usize> {
        self.width.map(|w| w.min(CARD_CELLS))
    }

    /// A card: the counter and name with the saving at the right, then the change with its
    /// colored verdict, then each labeled paragraph hanging under its label.
    pub(crate) fn card(&self, card: &Card<'_>) -> String {
        let width = self.card_width();
        let body = MARGIN + crate::terminal::cells(&card.counter) + 1;
        let (figure, share) = match &card.saving {
            Some((figure, share)) => (figure.as_str(), share.as_str()),
            None => (card.no_saving, ""),
        };
        let share = if share.is_empty() {
            String::new()
        } else {
            format!("  {share}")
        };
        let used = body
            + crate::terminal::cells(card.name)
            + crate::terminal::cells(figure)
            + crate::terminal::cells(&share);
        // Right-aligned when it fits; otherwise two spaces after the name.
        let gap = " ".repeat(width.map_or(2, |w| w.saturating_sub(used).max(2)));
        let figure_style = if card.saving.is_some() { GREEN } else { DIM };
        let counter = format!("{} ", card.counter);
        let mut text = self.spans(
            &[
                (&counter, DIM),
                (card.name, BOLD),
                (&gap, ""),
                (figure, figure_style),
                (&share, DIM),
            ],
            width,
            MARGIN,
            Some(body),
        );
        let (verdict, color) = match card.standing {
            Standing::Recommended => ("Recommended", RED),
            Standing::Considered => ("Worth considering", YELLOW),
            Standing::Advice => ("", ""),
        };
        let value = match (card.value.as_str(), verdict) {
            (value, "") => value.to_string(),
            (value, _) => format!("{value}  "),
        };
        if !value.is_empty() {
            text += &self.spans(&[(&value, ""), (verdict, color)], width, body, None);
        }
        let label = card
            .rows
            .iter()
            .map(|(l, _)| crate::terminal::cells(l))
            .max()
            .unwrap_or(0)
            + 2;
        for (name, row) in &card.rows {
            let name = format!("{name:<label$}");
            text += &self.spans(&[(&name, DIM), (row, "")], width, body, Some(body + label));
        }
        text
    }

    /// A change as the walk offers it, its counter `counter`: what it does and the evidence
    /// behind it, or the step to take by hand when it cannot be made here.
    fn entry(&self, fix: &Fix, counter: &str, host: Option<&str>) -> String {
        let mut rows = vec![];
        if let Some(host) = host {
            rows.push(("Host", host.to_string()));
        }
        if !fix.what.is_empty() {
            rows.push(("What", fix.what.clone()));
        }
        let why = why(&fix.evidence);
        if !why.is_empty() {
            rows.push(("Why", home_in(why, self.home.as_deref())));
        }
        self.card(&Card {
            counter: counter.into(),
            name: &fix.name,
            saving: saving(fix.verb, fix.saving, fix.margin, fix.share),
            no_saving: "saving not measured",
            value: fix.value.clone(),
            standing: Standing::of(fix.suggested),
            rows,
        })
    }

    /// A file as the person knows it, from `~`.
    fn path(&self, path: &str) -> String {
        crate::report::home_path(self.home.as_deref(), path)
    }

    /// A file to write, from `~` and after its host's name as `scp` names it, such as
    /// `devbox:~/.claude/settings.json`.
    pub(crate) fn file(&self, path: &str) -> String {
        match &self.host {
            Some(host) => format!("{host}:{}", self.path(path)),
            None => self.path(path),
        }
    }
}

/// What a change saves, such as ("saves $12/mo", "3%"), or none when it was not measured.
fn saving(
    verb: &str,
    usd: Option<f64>,
    margin: Option<Margin>,
    share: Option<f64>,
) -> Option<(String, String)> {
    usd.map(|usd| {
        (
            format!("{verb} {}", approx(usd, margin, "/mo")),
            share.map_or_else(String::new, share_text),
        )
    })
}

/// Evidence without its closing sentence naming the setting, such as "Set in
/// ~/.claude/settings.json:promptCacheTtl.", which the edit shown after it already names.
fn why(evidence: &str) -> &str {
    ["Set ", "Turn off: ", "Hide it from the model: "]
        .iter()
        .filter_map(|lead| evidence.rfind(&format!(" {lead}")))
        .max()
        .filter(|&at| {
            let last = &evidence[at + 1..];
            last.ends_with('.') && !last[..last.len() - 1].contains(". ")
        })
        .map_or(evidence, |at| &evidence[..at])
}

/// `text` with every path under `home` shown from `~`.
pub(crate) fn home_in(text: &str, home: Option<&str>) -> String {
    match home {
        Some(dir) => text
            .replace(&format!("{dir}/"), "~/")
            .replace(&format!("{dir}\\"), "~\\"),
        None => text.to_string(),
    }
}

/// ", 3% of this host's spend", or nothing when the spend is unknown; `whose` names the
/// host or hosts, such as "this host's".
fn share_of(share: Option<f64>, whose: &str) -> String {
    share.map_or_else(String::new, |s| {
        format!(", {} of {whose} spend", share_text(s))
    })
}

/// A settings file the walk may write.
#[derive(Clone, Debug)]
struct Target {
    /// The file as its profile names it.
    file: String,
    /// Where an edit lands: the file, or the end of its symlinks.
    resolved: PathBuf,
    /// The file as the filesystem resolves it, shared by every name for it.
    identity: PathBuf,
    harness: Harness,
    /// Its text when the walk began; None when it did not exist.
    before: Option<String>,
    /// Whether it is written under Claude Code's lock, re-read first, since Claude Code
    /// rewrites it while it runs.
    lock: bool,
}

/// Why the doctor cannot write a settings file, and the file to edit by hand instead.
#[derive(Debug)]
struct Refused {
    reason: String,
    /// The end of the file's symlinks when they resolve, else the file as named.
    file: String,
}

impl Target {
    /// The file as it is now, or why the doctor cannot write it.
    fn open(file: &str, harness: Harness) -> Result<Target, Refused> {
        let path = Path::new(file);
        let link = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
        let resolved = if link {
            fs::canonicalize(path).map_err(|e| Refused {
                reason: match e.kind() {
                    ErrorKind::NotFound => format!("{file} is a symlink to a missing file"),
                    _ => format!("{file} cannot be resolved ({e})"),
                },
                file: file.into(),
            })?
        } else {
            path.to_path_buf()
        };
        let shown = resolved.display().to_string();
        let refused = |reason: String| Refused {
            reason,
            file: shown.clone(),
        };
        let before = match fs::read(&resolved) {
            Ok(bytes) => Some(
                String::from_utf8(bytes)
                    .map_err(|_| refused(format!("{shown} is not UTF-8 text")))?,
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(refused(format!("{shown} cannot be read ({e})"))),
        };
        let directory = resolved.parent().filter(|d| !d.as_os_str().is_empty());
        let directory = directory.unwrap_or(Path::new("."));
        if !writable(directory) || (before.is_some() && !writable(&resolved)) {
            return Err(refused(format!("{shown} is read-only")));
        }
        Ok(Target {
            file: file.into(),
            identity: identity(&resolved),
            resolved,
            harness,
            before,
            lock: false,
        })
    }

    /// Read the file again, as it is now.
    fn reread(&mut self) -> io::Result<()> {
        self.before = match fs::read(&self.resolved) {
            Ok(bytes) => Some(
                String::from_utf8(bytes).map_err(|_| io::Error::other("it is not UTF-8 text"))?,
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        Ok(())
    }

    /// Whether the file is still as the walk found it.
    fn unchanged(&self) -> io::Result<bool> {
        let now = match fs::read(&self.resolved) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        Ok(now.as_deref() == self.before.as_deref().map(str::as_bytes))
    }

    /// Where the backup of this file goes, when doctor is to make one: beside the file
    /// itself, as it was before the change. A file that does not exist yet has nothing to
    /// keep, and one git tracks with no uncommitted changes is kept by git.
    fn backup(&self) -> Option<PathBuf> {
        self.before.as_ref()?;
        if git_keeps(&self.resolved) {
            return None;
        }
        let mut name = self.resolved.file_name()?.to_os_string();
        name.push(".token-max.bak");
        Some(self.resolved.with_file_name(name))
    }

    /// Whether git holds this file's text as it is now, so a backup would only clutter the
    /// repository with a file to commit.
    fn kept_by_git(&self) -> bool {
        self.before.is_some() && git_keeps(&self.resolved)
    }

    /// Copy the file as the walk read it to a temporary sibling, to become its backup once
    /// the write commits.
    fn stage_backup(&self) -> io::Result<Option<Staged>> {
        let (Some(bak), Some(before)) = (self.backup(), &self.before) else {
            return Ok(None);
        };
        let name = bak.file_name().unwrap_or_default().to_string_lossy();
        let temp = bak.with_file_name(format!(".{name}-{}", std::process::id()));
        let existing = fs::metadata(&self.resolved).ok();
        create_copy(&temp, existing.as_ref(), before.as_bytes())?;
        Ok(Some(Staged {
            temp,
            bak,
            keep: false,
        }))
    }

    /// Replace the file with `text` atomically, keeping its permissions and any symlink,
    /// while `lock`, when given, is still held.
    fn write(&self, text: &str, lock: Option<&Lock>) -> io::Result<()> {
        let name = self
            .resolved
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let temp = self
            .resolved
            .with_file_name(format!(".{name}.token-max-{}", std::process::id()));
        let existing = fs::metadata(&self.resolved).ok();
        create_copy(&temp, existing.as_ref(), text.as_bytes())?;
        let result = (|| {
            // Check again just before replacing it, so a write by another tool since the
            // walk's check is kept rather than overwritten.
            if !self.unchanged()? {
                return Err(io::Error::other("it changed while token-max was writing"));
            }
            if lock.is_some_and(|l| !l.held()) {
                return Err(io::Error::other(
                    "another program took over its lock while token-max was writing",
                ));
            }
            fs::rename(&temp, &self.resolved)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

/// A backup written beside its file, waiting for the file's own write to commit; dropped
/// uncommitted, it leaves the earlier backup as it was.
#[derive(Debug)]
struct Staged {
    temp: PathBuf,
    bak: PathBuf,
    /// Set when the commit failed after its file was written, so the copy is the only
    /// record of the file as it was.
    keep: bool,
}

impl Staged {
    /// Replace the earlier backup with this one; the drop then finds nothing left to remove.
    /// A failure keeps the staged copy, which the caller names.
    fn commit(mut self) -> io::Result<()> {
        let result = fs::rename(&self.temp, &self.bak);
        self.keep = result.is_err();
        result
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.temp);
        }
    }
}

/// Create `temp`, which must not exist, holding `bytes`, with the owner and permissions of
/// `existing` when given. A failure leaves no `temp` that this call made.
/// Settings can hold credentials, so the copy starts readable by its owner alone and takes
/// the replaced file's owner and group before its permissions: group bits copied onto
/// another group would widen who can read it.
fn create_copy(temp: &Path, existing: Option<&fs::Metadata>, bytes: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(temp)?;
    fill(file, existing, bytes).inspect_err(|_| {
        let _ = fs::remove_file(temp);
    })
}

/// Give `file` its owner and permissions, then its contents.
fn fill(mut file: fs::File, existing: Option<&fs::Metadata>, bytes: &[u8]) -> io::Result<()> {
    if let Some(metadata) = existing {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let made = file.metadata()?;
            if (made.uid(), made.gid()) != (metadata.uid(), metadata.gid()) {
                std::os::unix::fs::fchown(&file, Some(metadata.uid()), Some(metadata.gid()))
                    .map_err(|e| {
                        io::Error::new(e.kind(), format!("cannot keep its owner and group: {e}"))
                    })?;
            }
        }
        file.set_permissions(metadata.permissions())?;
    }
    file.write_all(bytes)?;
    file.sync_all()
}

/// Whether git tracks `file` and holds its text as it is now, with no uncommitted change.
/// Without git, or outside a repository, it keeps nothing.
fn git_keeps(file: &Path) -> bool {
    let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else {
        return false;
    };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .args(["--"])
            .arg(name)
            .env("GIT_OPTIONAL_LOCKS", "0")
            // The file's name is a path, not a glob: `a[1].json` must not match `a1.json`.
            .env("GIT_LITERAL_PATHSPECS", "1")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
    };
    // `H` marks a tracked file git compares with its index; `assume-unchanged` and
    // `skip-worktree` files carry other marks, and `status` would not show their changes.
    git(&["ls-files", "-v", "--error-unmatch"]).is_some_and(|o| o.stdout.starts_with(b"H "))
        && git(&["status", "--porcelain"]).is_some_and(|o| o.stdout.is_empty())
}

/// Whether this process may write `path`.
#[cfg(unix)]
fn writable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

/// Whether this process may write `path`; Windows marks read-only files, and ignores the
/// mark on directories.
#[cfg(not(unix))]
fn writable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_dir() || !m.permissions().readonly())
}

/// A question saving less than this every 30 days in total is not asked.
const ASKED_USD: f64 = 1.0;

/// A file's accepted changes: settings to set, and MCP server entries to remove.
#[derive(Debug)]
struct Pending {
    target: Target,
    edits: Vec<Edit>,
    removals: Vec<Entry>,
}

impl Pending {
    fn changes(&self) -> usize {
        self.edits.len() + self.removals.len()
    }
}

/// What the person accepted in the walk, and the files to write once they confirm.
#[derive(Debug)]
pub struct Plan {
    /// Each file with accepted changes, in the order first accepted.
    files: Vec<Pending>,
    /// What became of each change, by the report lines it came from.
    choices: Vec<(Origin, Choice)>,
    total: Total,
}

impl Plan {
    /// What became of each change, by the report lines it came from.
    pub fn choices(&self) -> &[(Origin, Choice)] {
        &self.choices
    }

    /// The accepted changes' summed saving after `label`, such as "Accepted"; empty when
    /// nothing was accepted.
    pub fn total(&self, label: &str) -> String {
        self.total.text(label, THIS_HOST)
    }
}

/// The accepted changes' own savings: those measured, summed apart from those that hold only
/// when a default would be the other value, and how many were not measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Total {
    /// Savings measured whatever the defaults, in full or as the part that was priced.
    measured: Sum,
    /// How many of `measured` leave part of their cost unmeasured, so save at least their figure.
    partial: usize,
    /// Savings that hold only when an unset default would otherwise be the other value.
    up_to: Sum,
    unmeasured: usize,
}

/// Monthly savings added up, with their share of this host's spend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Sum {
    changes: usize,
    usd: f64,
    /// How far `usd` may be off, from the changes extrapolated over unpriced requests.
    margin: Option<Margin>,
    share: f64,
    /// Whether some change's share of the host's spend is unknown.
    unknown_share: bool,
}

impl Sum {
    fn add(&mut self, usd: f64, margin: Option<Margin>, share: Option<f64>) {
        self.changes += 1;
        self.usd += usd;
        self.margin = Margin::plus(self.margin, margin);
        match share {
            Some(share) => self.share += share,
            None => self.unknown_share = true,
        }
    }

    /// Another host's sum added to this one. Hosts are separate samples, so their margins
    /// combine in quadrature, and the share is set again once the hosts' spend is added.
    fn merge(&mut self, other: &Sum) {
        self.changes += other.changes;
        self.usd += other.usd;
        self.margin = Margin::independent(self.margin, other.margin);
    }

    /// Such as "$40/mo, 9% of this host's spend", or "~$40/mo ± $6" when extrapolated.
    fn text(&self, whose: &str) -> String {
        let share = share_of((!self.unknown_share).then_some(self.share), whose);
        format!("{}{share}", approx(self.usd, self.margin, "/mo"))
    }
}

/// "1 change" or "2 changes".
fn changes(n: usize) -> String {
    format!("{n} change{}", if n == 1 { "" } else { "s" })
}

impl Total {
    /// What every change in `fixes` saves together.
    pub(crate) fn of(fixes: &[Fix]) -> Self {
        let mut total = Total::default();
        for fix in fixes {
            total.add(fix);
        }
        total
    }

    /// Another host's savings added to these.
    pub(crate) fn merge(&mut self, other: &Total) {
        self.measured.merge(&other.measured);
        self.up_to.merge(&other.up_to);
        self.partial += other.partial;
        self.unmeasured += other.unmeasured;
    }

    /// Set each saving's share to its part of `monthly`, the spend every 30 days of the hosts
    /// merged; unknown when it is none.
    pub(crate) fn set_share(&mut self, monthly: f64) {
        for sum in [&mut self.measured, &mut self.up_to] {
            sum.unknown_share = monthly <= 0.0;
            sum.share = if monthly > 0.0 {
                sum.usd / monthly
            } else {
                0.0
            };
        }
    }

    fn add(&mut self, fix: &Fix) {
        let Some(usd) = fix.saving else {
            self.unmeasured += 1;
            return;
        };
        if fix.verb == UP_TO {
            self.up_to.add(usd, fix.margin, fix.share);
            return;
        }
        self.measured.add(usd, fix.margin, fix.share);
        self.partial += usize::from(fix.verb == AT_LEAST);
    }

    /// How many changes were accepted.
    pub(crate) fn changes(&self) -> usize {
        self.measured.changes + self.up_to.changes + self.unmeasured
    }

    /// Such as "about $40/mo, 9% of this host's spend; overlapping changes may save less".
    fn amount(&self, whose: &str) -> String {
        let mut parts = vec![];
        if self.measured.changes > 0 {
            let usd = self.measured.text(whose);
            let mut text = if usd.starts_with(['<', '~']) {
                usd
            } else {
                format!("about {usd}")
            };
            if self.partial > 0 {
                let was = if self.partial == 1 { "was" } else { "were" };
                text += &format!(
                    ", more where {} {was} priced only in part",
                    changes(self.partial)
                );
            }
            parts.push(text);
        }
        if self.up_to.changes > 0 {
            parts.push(format!(
                "up to {}, from {} whose saving depends on the default it replaces",
                self.up_to.text(whose),
                changes(self.up_to.changes)
            ));
        }
        if self.unmeasured > 0 {
            let whose = if self.unmeasured == 1 {
                "whose saving was"
            } else {
                "whose savings were"
            };
            parts.push(format!("{} {whose} not measured", changes(self.unmeasured)));
        }
        // Each saving holds every other setting constant, so two may count the same requests.
        let overlap = if self.measured.changes + self.up_to.changes > 1 {
            "; overlapping changes may save less"
        } else {
            ""
        };
        format!("{}{overlap}", parts.join(", plus "))
    }

    /// The amount after `label`, as a sentence; empty when nothing was added.
    pub(crate) fn text(&self, label: &str, whose: &str) -> String {
        let amount = self.amount(whose);
        if amount.is_empty() {
            return amount;
        }
        format!("{label}: {amount}.")
    }
}

/// What every change in `fixes` saves together, as the line under the report's headline,
/// counted from the defaults in a `--defaults` report; empty when there is none.
pub fn total(fixes: &[Fix], defaults: bool) -> String {
    let total = Total::of(fixes);
    let lead = if defaults {
        "From the defaults, these"
    } else {
        "Together, these"
    };
    if total.changes() == 0 {
        String::new()
    } else if total.measured.changes + total.up_to.changes == 0 {
        if defaults {
            "From the defaults, no change's saving was measured.".into()
        } else {
            "No change's saving was measured.".into()
        }
    } else {
        format!("{lead} changes save {}.", total.amount(THIS_HOST))
    }
}

/// One person's answer at a prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    Yes,
    No,
    Quit,
    /// Split a group, asking about each of its changes.
    Split,
}

/// Ask `prompt` until it gets an answer it takes; end of input is a quit.
pub(crate) fn answer(
    prompt: &Prompt,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<Answer> {
    loop {
        write!(output, "{}", prompt.text)?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            writeln!(output)?;
            return Ok(Answer::Quit);
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(Answer::Yes),
            "n" | "no" => return Ok(Answer::No),
            "q" | "quit" => return Ok(Answer::Quit),
            "s" | "split" if prompt.split => return Ok(Answer::Split),
            _ => {}
        }
    }
}

/// One profile root's transcripts, as the opening lists them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub harness: Harness,
    pub root: String,
    pub files: u64,
    pub bytes: u64,
}

impl Found {
    /// Each profile root's transcripts in `discovery`, a file shared by several profiles
    /// counted once, under the first; roots with no transcript are left out.
    pub fn list(discovery: &crate::discovery::Discovery) -> Vec<Found> {
        let roots: IndexMap<&str, (Harness, &str)> = discovery
            .profiles
            .iter()
            .map(|p| (p.id.as_str(), (p.harness, p.root.as_str())))
            .collect();
        let mut found: IndexMap<(Harness, &str), (u64, u64)> = IndexMap::default();
        for snapshot in &discovery.snapshots {
            let Some(&key) = snapshot
                .profiles
                .first()
                .and_then(|id| roots.get(id.as_str()))
            else {
                continue;
            };
            let (files, bytes) = found.entry(key).or_default();
            *files += 1;
            *bytes += snapshot.bytes;
        }
        found
            .into_iter()
            .map(|((harness, root), (files, bytes))| Found {
                harness,
                root: root.to_string(),
                files,
                bytes,
            })
            .collect()
    }
}

/// The wordmark's two rows: `token` in bold, `-max` in the logo's purple.
const WORDMARK: [(&str, &str); 2] = [
    ("▀█▀ █▀█ █▄▀ █▀▀ █▄ █ ", "▄▄ █▀▄▀█ ▄▀█ ▀▄▀"),
    (" █  █▄█ █ █ ██▄ █ ▀█ ", "   █ ▀ █ █▀█ █ █"),
];
/// The logo's purple.
const PURPLE: &str = "35";

/// What doctor shows on a terminal before it asks anything: its name, what it is about to
/// do, the transcripts it found on this host, and the SSH destinations it adds, or how to
/// add one.
pub fn opening(found: &[Found], destinations: &[String], style: &Style) -> String {
    let version = env!("CARGO_PKG_VERSION");
    let mut out = String::from("\n");
    let art = crate::terminal::cells(WORDMARK[0].0) + crate::terminal::cells(WORDMARK[0].1);
    let fits = style
        .width
        .is_none_or(|w| w >= MARGIN + art + 3 + version.len());
    if style.unicode && fits {
        let tails = [("●", GREEN), (version, DIM)];
        for ((token, max), (tail, tail_style)) in WORDMARK.iter().zip(tails) {
            let spans = [
                (*token, BOLD),
                (*max, PURPLE),
                ("  ", ""),
                (tail, tail_style),
            ];
            out.push_str(&style.spans(&spans, None, MARGIN, None));
        }
    } else {
        let spans = [("token", BOLD), ("-max", PURPLE), (" ", ""), (version, DIM)];
        out.push_str(&style.spans(&spans, style.width, MARGIN, None));
    }
    out.push('\n');
    out.push_str(
        &style
            .line("About to scan this host's Claude Code and Codex transcripts for cost savings."),
    );
    out.push_str(&style.dim("Replays them at public API prices; prints no prompt or reply text."));
    out.push('\n');
    if found.is_empty() {
        out.push_str(&style.dim("No transcripts found on this host."));
    }
    let name = |harness| match harness {
        Harness::Claude => "Claude Code",
        Harness::Codex => "Codex",
    };
    let roots: Vec<String> = found
        .iter()
        .map(|f| home_in(&f.root, style.home.as_deref()))
        .collect();
    let counts: Vec<String> = found
        .iter()
        .map(|f| crate::report::count(f.files))
        .collect();
    let sizes: Vec<String> = found.iter().map(|f| crate::report::size(f.bytes)).collect();
    let widest = |column: &[String]| column.iter().map(|c| crate::terminal::cells(c)).max();
    let name_cells = found
        .iter()
        .map(|f| name(f.harness).len())
        .max()
        .unwrap_or(0);
    let root_cells = widest(&roots).unwrap_or(0);
    let count_cells = widest(&counts).unwrap_or(0);
    let size_cells = widest(&sizes).unwrap_or(0);
    let mark = if style.unicode { "✓ " } else { "" };
    let indent = MARGIN + crate::terminal::cells(mark);
    // "transcripts" is 11 cells, between a space and two.
    let table = indent + name_cells + 2 + root_cells + 2 + count_cells + 14 + size_cells;
    // A table wider than the terminal puts each root's figures on the line below it.
    let stacked = style.width.is_some_and(|w| table > w);
    for (((f, root), files), bytes) in found.iter().zip(&roots).zip(&counts).zip(&sizes) {
        let lead = format!("{:<name_cells$}  {}", name(f.harness), root);
        let noun = if f.files == 1 {
            "transcript "
        } else {
            "transcripts"
        };
        let figures = format!("{files:>count_cells$} {noun}  {bytes:>size_cells$}");
        if stacked {
            let spans = [(mark, GREEN), (lead.as_str(), "")];
            out.push_str(&style.spans(&spans, style.width, MARGIN, Some(indent)));
            out.push_str(&style.spans(&[(figures.as_str(), BOLD)], None, indent, None));
        } else {
            let pad = " ".repeat(root_cells - crate::terminal::cells(root));
            let figures = format!("{pad}  {figures}");
            let spans = [(mark, GREEN), (lead.as_str(), ""), (figures.as_str(), BOLD)];
            out.push_str(&style.spans(&spans, None, MARGIN, None));
        }
    }
    if destinations.is_empty() {
        out.push_str(&style.spans(
            &[
                ("+ other machines: ", DIM),
                ("token-max doctor --ssh <host>", ""),
            ],
            style.width,
            MARGIN,
            None,
        ));
    }
    for destination in destinations {
        out.push_str(&style.spans(
            &[("+ ", DIM), (destination, ""), (" over SSH", DIM)],
            style.width,
            MARGIN,
            None,
        ));
    }
    out.push('\n');
    out
}

/// Say that release `latest` is out, then ask whether to upgrade to it and rerun; end of input
/// is a no. Nothing is stored: the next run asks again.
pub fn ask_upgrade(
    latest: &str,
    style: &Style,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<bool> {
    write!(
        output,
        "\n{}",
        style.line(&format!("token-max {}", stale(latest)))
    )?;
    let prompt = style.ask(&format!("Upgrade to {latest} and rerun?"), "[y]es, [n]o");
    loop {
        write!(output, "{prompt}")?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            writeln!(output)?;
            return Ok(false);
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => {}
        }
    }
}

/// That release `latest` is newer than this build, whose advice may have gone stale.
pub fn stale(latest: &str) -> String {
    format!(
        "{latest} is out; this is v{}, and its advice may be out of date.",
        crate::upgrade::VERSION
    )
}

/// A line about the release check or the upgrade, dim when it is only a `note`.
pub fn upgrade_line(text: &str, note: bool, style: &Style) -> String {
    if note {
        style.dim(text)
    } else {
        style.line(text)
    }
}

/// Asks how each Claude profile pays, one at a time across hosts, before the report, so its
/// main conversation's cache TTL is priced against the one default that applies. A skip
/// leaves a profile naming both defaults, and end of input skips the rest, on every host.
/// Nothing is stored: the next run asks again.
#[derive(Debug, Default)]
pub struct BillingQuestions {
    asked: bool,
    ended: bool,
}

impl BillingQuestions {
    /// Ask how the profile at `root` pays, shown from `style`'s home and after its host's
    /// name; none for a skip, or once input has ended.
    pub fn ask(
        &mut self,
        root: &str,
        style: &Style,
        input: &mut impl BufRead,
        output: &mut impl Write,
    ) -> io::Result<Option<Billing>> {
        if self.ended {
            return Ok(None);
        }
        self.asked = true;
        // A destination's root is its own text.
        let profile = crate::safe::terminal_text(&style.file(root));
        let question = format!("How is {profile} paid for?");
        let prompt = style.ask(&question, "[p]lan, [a]PI key or usage credits, [s]kip");
        loop {
            write!(output, "{prompt}")?;
            output.flush()?;
            let mut line = String::new();
            if input.read_line(&mut line)? == 0 {
                writeln!(output)?;
                self.ended = true;
                return Ok(None);
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "p" | "plan" => return Ok(Some(Billing::Plan)),
                "a" | "api" | "api key" | "usage credits" => return Ok(Some(Billing::Api)),
                "s" | "skip" => return Ok(None),
                _ => {}
            }
        }
    }

    /// End the questions, so the report follows as its own section.
    pub fn finish(self, output: &mut impl Write) -> io::Result<()> {
        if self.asked {
            writeln!(output)?;
        }
        Ok(())
    }
}

/// One answer in the walk, by its place in a host's [`Offers`], so the host that owns the
/// files can replay the walk and make exactly the changes it offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Event {
    /// An offered change, answered yes or no.
    Choose { offer: usize, yes: bool },
    /// After a yes, also remove one of the entries the offer named.
    Remove { offer: usize, entry: usize },
    /// Remove the configuration of an MCP server that is off already.
    Cleanup { entry: usize },
}

/// What one host's walk accepted so far, and the answers that accepted it.
#[derive(Debug, Default)]
pub struct Selection {
    /// Each target with accepted changes, by its index among the host's targets.
    accepted: Vec<(usize, Vec<Edit>, Vec<Entry>)>,
    choices: Vec<(Origin, Choice)>,
    total: Total,
    events: Vec<Event>,
    /// The offers answered yes, whose entries a removal may name.
    yes: Vec<usize>,
}

impl Selection {
    /// The answers, in the order given.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// The accepted changes for `target`, empty until one is accepted.
    fn at(&mut self, target: usize) -> &mut (usize, Vec<Edit>, Vec<Entry>) {
        let found = self.accepted.iter().position(|(i, _, _)| *i == target);
        let at = found.unwrap_or_else(|| {
            self.accepted.push((target, vec![], vec![]));
            self.accepted.len() - 1
        });
        &mut self.accepted[at]
    }

    /// Record `event`, one of `offers`' changes, or why it names none.
    pub(crate) fn apply(&mut self, offers: &Offers, event: Event) -> Result<(), String> {
        let missing = || format!("the walk answered a change it was not offered ({event:?})");
        match event {
            Event::Choose { offer: index, yes } => {
                let offer = offers.offers.get(index).ok_or_else(missing)?;
                if yes {
                    self.yes.push(index);
                }
                let choice = if yes {
                    Choice::Accepted
                } else {
                    Choice::Declined
                };
                self.choices
                    .extend(offer.fix.origins.iter().map(|o| (*o, choice.clone())));
                if yes {
                    self.total.add(&offer.fix);
                    let How::Edit(patch) = &offer.fix.how else {
                        unreachable!("offers are edits")
                    };
                    self.at(offer.target).1.extend(patch.edits.iter().cloned());
                }
            }
            Event::Remove { offer, entry } => {
                if !self.yes.contains(&offer) {
                    return Err(format!(
                        "the walk removed an entry of a change it did not accept ({event:?})"
                    ));
                }
                let (target, entry) = offers
                    .offers
                    .get(offer)
                    .and_then(|o| o.entries.get(entry))
                    .ok_or_else(missing)?;
                self.remove(*target, entry);
            }
            Event::Cleanup { entry } => {
                let (target, entry) = offers.cleanup.get(entry).ok_or_else(missing)?;
                self.remove(*target, entry);
            }
        }
        self.events.push(event);
        Ok(())
    }

    fn remove(&mut self, target: usize, entry: &Entry) {
        let removals = &mut self.at(target).2;
        if !removals.contains(entry) {
            removals.push(entry.clone());
        }
    }

    /// What the walk came to on the host whose changes are `offers`.
    pub fn summary(&self, offers: &Offers) -> Summary {
        Summary {
            answered: !self.choices.is_empty()
                || !offers.steps.is_empty()
                || !offers.cleanup_by_hand.is_empty()
                || offers.offered() > 0,
            offered: offers.offered(),
            files: self
                .accepted
                .iter()
                .map(|(target, _, _)| offers.files[*target].clone())
                .collect(),
            changes: self.total.changes(),
            removals: self.accepted.iter().map(|(_, _, r)| r.len()).sum(),
        }
    }
}

/// What one host's walk came to, as the confirm and the closing lines name it.
#[derive(Debug)]
pub struct Summary {
    /// Whether the report had any change to offer or make by hand.
    answered: bool,
    /// How many changes the walk offered.
    offered: usize,
    /// Each file with accepted changes, as its profile names it, in the order first accepted.
    files: Vec<String>,
    /// How many changes were accepted.
    changes: usize,
    /// How many MCP server entries are to be removed.
    removals: usize,
}

impl Summary {
    /// Whether the walk had any change to offer or make by hand.
    pub fn answered(&self) -> bool {
        self.answered
    }

    /// Whether anything is to be written.
    pub fn accepted(&self) -> bool {
        !self.files.is_empty()
    }
}

/// The files the walk may write: each as a profile names it, and its target. Names that
/// resolve to one file, such as two profiles' symlinks to one dotfile, share a target, so
/// each accepted edit lands in the one text written.
#[derive(Default)]
struct Targets {
    all: Vec<Result<Target, Refused>>,
    files: Vec<(String, usize)>,
}

impl Targets {
    /// The index of `file`'s target, opening it the first time.
    fn open(&mut self, file: &str, harness: Harness) -> usize {
        if let Some((_, index)) = self.files.iter().find(|(f, _)| f == file) {
            return *index;
        }
        let target = Target::open(file, harness);
        let shared = target.as_ref().ok().and_then(|t| {
            self.all.iter().position(|o| {
                o.as_ref()
                    .is_ok_and(|o| o.identity == t.identity && o.harness == t.harness)
            })
        });
        let index = shared.unwrap_or_else(|| {
            self.all.push(target);
            self.all.len() - 1
        });
        self.files.push((file.into(), index));
        index
    }

    /// The target of an MCP server's entry, or the step by hand when the doctor cannot
    /// remove it there.
    fn entry(&mut self, entry: &Entry) -> Result<usize, Definition> {
        let index = self.open(&entry.file, entry.harness);
        let refused = match &mut self.all[index] {
            Err(refused) => Some((refused.reason.clone(), refused.file.clone())),
            Ok(target) => {
                target.lock |= entry.live();
                apply(target, &[], std::slice::from_ref(entry))
                    .err()
                    .map(|reason| (reason, target.resolved.display().to_string()))
            }
        };
        match refused {
            None => Ok(index),
            Some((reason, file)) => Err(Definition::Manual {
                step: format!("remove {} from {file}", entry.text()),
                reason,
            }),
        }
    }
}

/// An offered change, with the MCP server entries it may also remove.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Offer {
    target: usize,
    fix: Fix,
    /// Each entry with its target.
    entries: Vec<(usize, Entry)>,
    /// Removals to make by hand: the step, and why.
    manual: Vec<(String, String)>,
}

/// One host's changes as its walk offers them, checked against its own files, so the walk
/// reads no file and may run on another host.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Offers {
    offers: Vec<Offer>,
    /// What became of each change that is a step by hand.
    steps: Vec<(Origin, Choice)>,
    /// The entries of MCP servers that are off already, each with its target, which the
    /// walk's last question offers to remove.
    cleanup: Vec<(usize, Entry)>,
    /// How to look for servers that are off by hand: the step, and why.
    cleanup_by_hand: Vec<(String, String)>,
    /// Each target's file as a profile names it, by index.
    files: Vec<String>,
    /// The host's home directory, which its paths are shown from as `~`.
    pub home: Option<String>,
    /// The host's spend every 30 days, which a share of several hosts' spend adds up.
    monthly: f64,
}

/// What a host's walk does about one line: offers its edit, or names the step by hand.
pub(crate) enum Change<'a> {
    /// The offer's index among the host's, and the change.
    Offer(usize, &'a Fix),
    /// As the list reads it, such as "by hand, since ...: ...".
    ByHand(&'a str),
}

impl Offers {
    /// What the walk does about the line at `origin`, if it names a change.
    pub(crate) fn change(&self, origin: Origin) -> Option<Change<'_>> {
        let offer = self
            .offers
            .iter()
            .position(|o| o.fix.origins.contains(&origin));
        if let Some(index) = offer {
            return Some(Change::Offer(index, &self.offers[index].fix));
        }
        self.steps.iter().find_map(|(o, choice)| match choice {
            Choice::Manual(step) if *o == origin => Some(Change::ByHand(step)),
            _ => None,
        })
    }

    /// How many changes the walk offers.
    pub(crate) fn offered(&self) -> usize {
        self.offers.len() + self.cleanup.len()
    }

    /// Check offers another host sent: each is an edit, and names one of its targets.
    pub fn check(&self) -> Result<(), String> {
        let targets = self
            .offers
            .iter()
            .flat_map(|o| std::iter::once(o.target).chain(o.entries.iter().map(|(t, _)| *t)))
            .chain(self.cleanup.iter().map(|(t, _)| *t));
        let edits = self
            .offers
            .iter()
            .all(|o| matches!(o.fix.how, How::Edit(_)));
        match targets.max() {
            _ if !edits => Err("offered a change it cannot make".into()),
            Some(max) if max >= self.files.len() => Err("offered a change to no file".into()),
            _ => Ok(()),
        }
    }
}

/// The host whose files the walk's changes are for: its targets, read once, and the
/// changes it offers.
pub struct Owner {
    targets: Vec<Result<Target, Refused>>,
    offers: Offers,
}

impl Owner {
    /// Check each change in `fixes` against its file, turning any the doctor cannot make
    /// into a step by hand, and find where each MCP server in `turned_off`, whose tools are
    /// off already, can be removed. `home` and `monthly` are this host's.
    pub fn new(
        fixes: Vec<Fix>,
        turned_off: Vec<Definition>,
        home: Option<String>,
        monthly: f64,
    ) -> Owner {
        let mut targets = Targets::default();
        let mut offers = vec![];
        let by_hand = |step: &str, reason: &str| {
            Choice::Manual(home_in(
                &format!("by hand, since {reason}: {step}"),
                home.as_deref(),
            ))
        };
        let manual = |fix: &Fix| {
            let How::Manual { step, reason } = &fix.how else {
                unreachable!("only steps by hand")
            };
            let step = by_hand(step, reason);
            fix.origins
                .iter()
                .map(|o| (*o, step.clone()))
                .collect::<Vec<_>>()
        };
        let mut steps = vec![];
        // Every entry a never-used server's question may remove, so the last question leaves
        // them out.
        let mut offered_entries: Vec<Entry> = vec![];
        for mut fix in fixes {
            let How::Edit(patch) = &fix.how else {
                steps.extend(manual(&fix));
                continue;
            };
            let index = targets.open(&patch.file, patch.harness);
            // Check the edit applies before offering it.
            let refused = match &targets.all[index] {
                Err(refused) => Some((refused.reason.clone(), refused.file.clone())),
                Ok(target) => apply(target, &patch.edits, &[])
                    .err()
                    .map(|reason| (reason, target.resolved.display().to_string())),
            };
            if let Some((reason, file)) = refused {
                fix.how = How::Manual {
                    step: steps_in(patch, &file),
                    reason,
                };
                steps.extend(manual(&fix));
                continue;
            }
            let mut entries = vec![];
            let mut by_hand = vec![];
            for definition in &fix.removal {
                let definition = match definition {
                    Definition::Entry(entry) => {
                        offered_entries.push(entry.clone());
                        match targets.entry(entry) {
                            Ok(target) => {
                                entries.push((target, entry.clone()));
                                continue;
                            }
                            Err(manual) => manual,
                        }
                    }
                    manual => manual.clone(),
                };
                if let Definition::Manual { step, reason } = definition {
                    by_hand.push((step, reason));
                }
            }
            offers.push(Offer {
                target: index,
                fix,
                entries,
                manual: by_hand,
            });
        }
        // Servers that are off already, apart from those a question above may remove.
        let mut cleanup = vec![];
        let mut cleanup_by_hand = vec![];
        for definition in turned_off {
            let definition = match definition {
                Definition::Entry(entry) if offered_entries.contains(&entry) => continue,
                Definition::Entry(entry) => match targets.entry(&entry) {
                    Ok(target) => {
                        cleanup.push((target, entry));
                        continue;
                    }
                    Err(manual) => manual,
                },
                manual => manual,
            };
            if let Definition::Manual { step, reason } = definition {
                cleanup_by_hand.push((step, reason));
            }
        }
        let mut files = vec![String::new(); targets.all.len()];
        // Each target under the first name that opened it.
        for (file, index) in targets.files.iter().rev() {
            files[*index] = file.clone();
        }
        Owner {
            targets: targets.all,
            offers: Offers {
                offers,
                steps,
                cleanup,
                cleanup_by_hand,
                files,
                home,
                monthly,
            },
        }
    }

    /// The changes this host offers.
    pub fn offers(&self) -> &Offers {
        &self.offers
    }

    /// The plan the walk's `events` make of this host's changes, the files as they were read
    /// when the changes were offered.
    pub fn plan(&self, events: &[Event]) -> Result<Plan, ScanError> {
        let mut selection = Selection::default();
        for event in events {
            selection
                .apply(&self.offers, *event)
                .map_err(|e| ScanError::new(format!("{e}; no settings were changed")))?;
        }
        let Selection {
            accepted,
            mut choices,
            total,
            ..
        } = selection;
        choices.extend(self.offers.steps.iter().cloned());
        let files = accepted
            .into_iter()
            .map(|(index, edits, removals)| {
                // Only a target that opened has an offer or an entry to accept.
                let target = self.targets[index]
                    .as_ref()
                    .expect("offered targets opened");
                Pending {
                    target: target.clone(),
                    edits,
                    removals,
                }
            })
            .collect();
        Ok(Plan {
            files,
            choices,
            total,
        })
    }
}

/// One host's changes in the walk.
pub struct Walking<'a> {
    /// The host's name, when the walk covers another host than this one.
    pub label: Option<&'a str>,
    pub offers: &'a Offers,
    /// How the host's changes print: its paths from its own home.
    pub style: &'a Style,
}

/// An offered change, with the host it is for.
struct Member<'a> {
    host: usize,
    index: usize,
    offer: &'a Offer,
}

/// One question of the walk: a change, or a group of one host's small ones.
struct Question<'a> {
    members: Vec<Member<'a>>,
}

impl Question<'_> {
    /// The members' savings summed as the running total sums them.
    fn together(&self) -> Total {
        let mut total = Total::default();
        for member in &self.members {
            total.add(&member.offer.fix);
        }
        total
    }

    /// What the members save every 30 days, a saving that depends on a default included;
    /// none when it was not measured.
    fn usd(&self) -> Option<f64> {
        let total = self.together();
        (total.unmeasured == 0).then_some(total.measured.usd + total.up_to.usd)
    }

    /// What the members save together, such as ("saves $9.40/mo", "2%"): up to that
    /// when one's saving depends on a default, at least that when one was measured in part.
    fn saving(&self) -> Option<(String, String)> {
        let total = self.together();
        let usd = self.usd()?;
        let verb = if total.up_to.changes > 0 {
            UP_TO
        } else if total.partial > 0 {
            AT_LEAST
        } else {
            "saves"
        };
        let unknown = total.measured.unknown_share || total.up_to.unknown_share;
        let share = (!unknown).then_some(total.measured.share + total.up_to.share);
        let margin = Margin::plus(total.measured.margin, total.up_to.margin);
        saving(verb, Some(usd), margin, share)
    }

    /// A group as the walk offers it, its counter `counter`: what its kind of feature is,
    /// then every member's name. Each member says what it does when the person splits
    /// the group.
    fn card(&self, counter: &str, host: Option<&str>, style: &Style) -> String {
        let first = &self.members[0].offer.fix;
        let group = first.group.as_ref().expect("only groups share a question");
        let groups: Vec<&Group> = self
            .members
            .iter()
            .filter_map(|m| m.offer.fix.group.as_ref())
            .collect();
        // A name two members share, such as a duplicate skill's, is followed by its location.
        let names: Vec<String> = groups
            .iter()
            .map(|g| {
                let shared = groups.iter().filter(|o| o.member == g.member).count() > 1;
                match &g.location {
                    Some(at) if shared => format!("{} ({})", g.member, style.path(at)),
                    _ => g.member.clone(),
                }
            })
            .collect();
        let mut rows = vec![];
        if let Some(host) = host {
            rows.push(("Host", host.to_string()));
        }
        rows.push(("What", crate::descriptions::kind(group.kind).to_string()));
        rows.push(("Which", format!("{}.", names.join(", "))));
        style.card(&Card {
            counter: counter.into(),
            name: &group.label,
            saving: self.saving(),
            no_saving: "savings not measured",
            value: first.value.clone(),
            standing: Standing::of(first.suggested),
            rows,
        })
    }
}

/// What the walk has accepted on every host so far.
struct Walk<'a, 'b> {
    hosts: &'a [Walking<'b>],
    selections: Vec<Selection>,
}

impl Walk<'_, '_> {
    /// Record `event` for host `host`.
    fn record(&mut self, host: usize, event: Event) {
        self.selections[host]
            .apply(self.hosts[host].offers, event)
            .expect("the walk answers only what it offers");
    }

    fn choose(&mut self, member: &Member<'_>, yes: bool) {
        let offer = member.index;
        self.record(member.host, Event::Choose { offer, yes });
    }

    /// The accepted changes' summed saving across hosts after `label`.
    fn total(&self, label: &str) -> String {
        if let [host] = self.hosts {
            let whose = host
                .label
                .map_or(THIS_HOST.to_string(), |l| format!("{l}'s"));
            return self.selections[0].total.text(label, &whose);
        }
        let mut total = Total::default();
        let mut monthly = 0.0;
        for (host, selection) in self.hosts.iter().zip(&self.selections) {
            total.merge(&selection.total);
            monthly += host.offers.monthly;
        }
        total.set_share(monthly);
        total.text(label, "these hosts'")
    }

    /// Record `answer` to `member`, printing the running total after a yes; whether the walk
    /// goes on.
    fn take(
        &mut self,
        member: &Member<'_>,
        answer: Answer,
        output: &mut impl Write,
    ) -> io::Result<bool> {
        self.choose(member, answer == Answer::Yes);
        if answer == Answer::Yes {
            let style = self.hosts[member.host].style;
            write!(output, "{}", style.dim(&self.total("Accepted so far")))?;
        }
        Ok(answer != Answer::Quit)
    }

    /// After a yes to turning never-used MCP servers off, offer to remove their configuration
    /// too, which the confirm then lists; whether the walk goes on. Removals only by hand are
    /// named without a question.
    fn offer_removal(
        &mut self,
        members: &[&Member<'_>],
        input: &mut impl BufRead,
        output: &mut impl Write,
    ) -> io::Result<bool> {
        let style = self.hosts[members[0].host].style;
        let entries: Vec<(&Member<'_>, usize, &Entry)> = members
            .iter()
            .flat_map(|m| {
                m.offer
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(i, (_, e))| (*m, i, e))
            })
            .collect();
        let mut text = String::new();
        if !entries.is_empty() {
            let stays = if members.len() == 1 {
                "It stays"
            } else {
                "They stay"
            };
            text += &style.line(&format!("{stays} configured; token-max can also remove:"));
            for (_, _, entry) in &entries {
                let place = format!("{} in {}", entry.text(), style.path(&entry.file));
                text += &style.raw(&place, MARGIN + 2, DIM);
            }
        }
        for (step, reason) in members.iter().flat_map(|m| &m.offer.manual) {
            text += &style.line(&home_in(
                &format!("To remove it by hand, since {reason}: {step}."),
                style.home.as_deref(),
            ));
        }
        write!(output, "{text}")?;
        if entries.is_empty() {
            return Ok(true);
        }
        let question = if entries.len() == 1 {
            "Remove it too?".to_string()
        } else {
            format!("Remove all {} too?", entries.len())
        };
        let answer = answer(&style.prompt(&question, false), input, output)?;
        if answer == Answer::Yes {
            for (member, entry, _) in entries {
                let offer = member.index;
                self.record(member.host, Event::Remove { offer, entry });
            }
        }
        Ok(answer != Answer::Quit)
    }

    /// A host's last question: remove the configuration of MCP servers that are off already,
    /// all at once or, split, one at a time; whether the walk goes on.
    fn ask_cleanup(
        &mut self,
        host: usize,
        counter: &str,
        input: &mut impl BufRead,
        output: &mut impl Write,
    ) -> io::Result<bool> {
        let Walking {
            label,
            offers,
            style,
        } = self.hosts[host];
        let cleanup = &offers.cleanup;
        let place = |entry: &Entry| format!("{} in {}", entry.text(), style.path(&entry.file));
        let card = |counter: String, name: &str, why: &str, places: Vec<String>| {
            let mut rows = vec![];
            if let Some(label) = label {
                rows.push(("Host", label.to_string()));
            }
            if !why.is_empty() {
                rows.push(("Why", why.to_string()));
            }
            rows.push(("Where", places.join(", ")));
            style.card(&Card {
                counter,
                name,
                saving: None,
                no_saving: "saves nothing",
                value: format!("off {} removed", style.arrow()),
                standing: Standing::Considered,
                rows,
            })
        };
        let why = "Their tools are off already, so no session uses this configuration.";
        let size = cleanup.len();
        if let [(_, entry)] = cleanup.as_slice() {
            let name = format!("MCP server {}", entry.server);
            let text = card(
                format!("[{counter}]"),
                &name,
                &why.replace("Their tools are", "Its tools are"),
                vec![place(entry)],
            );
            write!(output, "\n{text}")?;
            let answer = answer(&style.prompt("Remove it?", false), input, output)?;
            if answer == Answer::Yes {
                self.record(host, Event::Cleanup { entry: 0 });
            }
            return Ok(answer != Answer::Quit);
        }
        let places = cleanup.iter().map(|(_, e)| place(e)).collect();
        let text = card(
            format!("[{counter}]"),
            "Turned-off MCP servers",
            why,
            places,
        );
        write!(output, "\n{text}")?;
        let prompt = style.prompt(&format!("Remove all {size}?"), true);
        match answer(&prompt, input, output)? {
            Answer::Split => {
                for (i, (_, entry)) in cleanup.iter().enumerate() {
                    let name = format!("MCP server {}", entry.server);
                    let text = card(
                        format!("[{counter}, {}/{size}]", i + 1),
                        &name,
                        "",
                        vec![place(entry)],
                    );
                    write!(output, "\n{text}")?;
                    match answer(&style.prompt("Remove it?", false), input, output)? {
                        Answer::Yes => self.record(host, Event::Cleanup { entry: i }),
                        Answer::Quit => return Ok(false),
                        _ => {}
                    }
                }
                Ok(true)
            }
            Answer::Yes => {
                for entry in 0..size {
                    self.record(host, Event::Cleanup { entry });
                }
                Ok(true)
            }
            other => Ok(other != Answer::Quit),
        }
    }
}

/// Walk through each host's changes, biggest saving first across hosts, and return what the
/// person accepted on each, in the order of `hosts`. Features are offered in groups of one
/// host's changes of one kind and reason, which the person can split to answer each, and a
/// question saving under $1/mo in total is not asked. A yes to turning a never-used MCP
/// server off is followed by an offer to remove its configuration, and each host's last
/// question offers to remove the servers that are off already. Nothing is read or written
/// here; end of input is a quit, which leaves the rest unaccepted.
pub fn ask(
    hosts: &[Walking<'_>],
    style: &Style,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<Vec<Selection>> {
    // A feature's change joins the others of its host, kind and reason, whatever it saves,
    // measured apart from unmeasured; the person can split a group to answer each.
    let mut questions: Vec<Question> = vec![];
    for (host, walking) in hosts.iter().enumerate() {
        for (index, offer) in walking.offers.offers.iter().enumerate() {
            let fix = &offer.fix;
            let joins = |q: &Question| {
                let first = &q.members[0];
                let first_fix = &first.offer.fix;
                first.host == host
                    && first_fix.value == fix.value
                    && first_fix.saving.is_some() == fix.saving.is_some()
                    && first_fix
                        .group
                        .as_ref()
                        .zip(fix.group.as_ref())
                        .is_some_and(|(a, b)| a.joins(b))
            };
            let member = Member { host, index, offer };
            match questions.iter_mut().find(|q| joins(q)) {
                Some(question) => question.members.push(member),
                None => questions.push(Question {
                    members: vec![member],
                }),
            }
        }
    }
    // Biggest saving first, then those whose saving was not measured, in the report's order.
    questions.sort_by(|a, b| match (a.usd(), b.usd()) {
        (Some(a), Some(b)) => b.total_cmp(&a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    let (questions, skipped): (Vec<_>, Vec<_>) = questions
        .into_iter()
        .partition(|q| q.usd().is_none_or(|usd| usd >= ASKED_USD));
    let cleanups: Vec<usize> = (0..hosts.len())
        .filter(|h| !hosts[*h].offers.cleanup.is_empty())
        .collect();
    let count = questions.len() + cleanups.len();
    if count > 0 {
        let asked: usize = questions.iter().map(|q| q.members.len()).sum::<usize>()
            + cleanups
                .iter()
                .map(|h| hosts[*h].offers.cleanup.len())
                .sum::<usize>();
        let about = if asked == count {
            String::new()
        } else {
            format!(" about {}", self::changes(asked))
        };
        write!(
            output,
            "{}",
            style.headline(&format!(
                "{count} question{}{about}, biggest saving first.",
                if count == 1 { "" } else { "s" },
            ))
        )?;
    }
    let mut walk = Walk {
        hosts,
        selections: hosts.iter().map(|_| Selection::default()).collect(),
    };
    let mut going = true;
    for (n, question) in questions.iter().enumerate() {
        if !going {
            for member in &question.members {
                walk.choose(member, false);
            }
            continue;
        }
        let host = &hosts[question.members[0].host];
        let style = host.style;
        if let [member] = question.members.as_slice() {
            write!(
                output,
                "\n{}",
                style.entry(
                    &member.offer.fix,
                    &format!("[{}/{count}]", n + 1),
                    host.label
                )
            )?;
            let answer = answer(&style.prompt("Accept?", false), input, output)?;
            going = walk.take(member, answer, output)?;
            if answer == Answer::Yes {
                going = walk.offer_removal(&[member], input, output)?;
            }
            continue;
        }
        write!(
            output,
            "\n{}",
            question.card(&format!("[{}/{count}]", n + 1), host.label, style)
        )?;
        let size = question.members.len();
        let prompt = style.prompt(&format!("Accept all {size}?"), true);
        match answer(&prompt, input, output)? {
            Answer::Split => {
                for (i, member) in question.members.iter().enumerate() {
                    if !going {
                        walk.choose(member, false);
                        continue;
                    }
                    let counter = format!("[{}/{count}, {}/{size}]", n + 1, i + 1);
                    write!(
                        output,
                        "\n{}",
                        style.entry(&member.offer.fix, &counter, host.label)
                    )?;
                    let answer = answer(&style.prompt("Accept?", false), input, output)?;
                    going = walk.take(member, answer, output)?;
                    if answer == Answer::Yes {
                        going = walk.offer_removal(&[member], input, output)?;
                    }
                }
            }
            batch => {
                for member in &question.members {
                    walk.choose(member, batch == Answer::Yes);
                }
                if batch == Answer::Yes {
                    write!(output, "{}", style.dim(&walk.total("Accepted so far")))?;
                    let all: Vec<&Member> = question.members.iter().collect();
                    going = walk.offer_removal(&all, input, output)?;
                } else {
                    going = batch != Answer::Quit;
                }
            }
        }
    }
    // Removing servers that are off saves nothing, so it is asked after every saving.
    for (i, host) in cleanups.iter().enumerate() {
        if !going {
            break;
        }
        let n = questions.len() + i + 1;
        going = walk.ask_cleanup(*host, &format!("{n}/{count}"), input, output)?;
    }
    for walking in hosts {
        for (step, reason) in &walking.offers.cleanup_by_hand {
            let look =
                format!("look for MCP servers that are off by hand, since {reason}: {step}.");
            // Its step names a path on that host.
            let look = match walking.label {
                Some(host) => format!("On {host}, to {look}"),
                None => format!("To {look}"),
            };
            let text = home_in(&look, walking.style.home.as_deref());
            write!(output, "\n{}", walking.style.dim(&text))?;
        }
    }
    // Not accepted, so the list shows them with the changes answered no.
    for member in skipped.iter().flat_map(|q| &q.members) {
        walk.choose(member, false);
    }
    let left: usize = skipped.iter().map(|q| q.members.len()).sum();
    if left > 0 {
        let more = if count > 0 { " more" } else { "" };
        let (was, they) = if left == 1 {
            ("was", "it's")
        } else {
            ("were", "they're")
        };
        let text = format!(
            "{left}{more} change{} in groups saving under {}/mo {was} not asked; {they} in the list below.",
            if left == 1 { "" } else { "s" },
            about(ASKED_USD)
        );
        write!(output, "\n{}", style.dim(&text))?;
    }
    Ok(walk.selections)
}

/// Each file's text as `plan` would change it, then where each is backed up first, for the
/// confirm to show before anything is written.
pub fn preview(plan: &Plan, style: &Style) -> String {
    let mut text = String::new();
    for pending in &plan.files {
        // Every accepted edit was checked to apply when it was offered.
        let after = apply(&pending.target, &pending.edits, &pending.removals)
            .expect("accepted changes apply");
        text += &diff(pending, &after, style);
    }
    // Each file's backup, or why it has none, so nothing is written that the person
    // wasn't told would be.
    let mut kept = String::new();
    for pending in &plan.files {
        let target = &pending.target;
        let line = if let Some(bak) = target.backup() {
            format!(
                "Backs up {} to {} first.",
                style.file(&target.file),
                style.file(&bak.display().to_string())
            )
        } else if target.kept_by_git() {
            format!(
                "Git keeps the previous version of {}, so it gets no backup.",
                style.file(&target.file)
            )
        } else {
            continue;
        };
        kept += &style.line(&line);
    }
    if !kept.is_empty() {
        text += &format!("\n{kept}");
    }
    text
}

/// What the confirm asks to write: every host's previews, and what each host's walk came to.
pub struct Confirm<'a> {
    /// Every host's [`preview`], in order.
    pub preview: &'a str,
    /// Each host's summary, with the style that names its files.
    pub hosts: Vec<(&'a Summary, &'a Style)>,
}

/// Ask once whether to write the accepted changes, after showing each file's text as it
/// would change; end of input is a no. With nothing accepted, there is nothing to ask.
pub fn confirm(
    confirm: &Confirm<'_>,
    style: &Style,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<bool> {
    let names: Vec<String> = confirm
        .hosts
        .iter()
        .flat_map(|(summary, style)| summary.files.iter().map(|f| style.file(f)))
        .collect();
    if names.is_empty() {
        return Ok(false);
    }
    write!(output, "{}", confirm.preview)?;
    let files = match names.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => names.concat(),
    };
    let mut what = vec![];
    let changes: usize = confirm.hosts.iter().map(|(s, _)| s.changes).sum();
    if changes > 0 {
        what.push(format!(
            "{changes} accepted change{}",
            if changes == 1 { "" } else { "s" }
        ));
    }
    let removals: usize = confirm.hosts.iter().map(|(s, _)| s.removals).sum();
    if removals > 0 {
        what.push(format!(
            "{removals} removal{}",
            if removals == 1 { "" } else { "s" }
        ));
    }
    let question = format!("Write the {} to {files}?", what.join(" and "));
    yes_or_no(&question, style, input, output)
}

/// Ask `question` after a blank line until the answer is a yes or a no; end of input is a no.
pub fn yes_or_no(
    question: &str,
    style: &Style,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<bool> {
    let prompt = style.ask(question, "[y]es, [n]o");
    loop {
        write!(output, "\n{prompt}")?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            writeln!(output)?;
            return Ok(false);
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => {}
        }
    }
}

/// The settings whose values the confirm shows unmasked: those an edit sets. A removed value
/// is not one the person chose, so it stays masked.
fn shown(edits: &[Edit]) -> Vec<&[String]> {
    edits
        .iter()
        .filter(|e| e.op != Op::Remove)
        .map(|e| e.path.as_slice())
        .collect()
}

/// How `target`'s text would change to `after`: its name, then each changed line with a
/// line of context on either side, removed lines red and added ones green.
/// Secret values show masked, as `mask` describes.
fn diff(pending: &Pending, after: &str, style: &Style) -> String {
    let target = &pending.target;
    let mask = if style.unicode {
        "••••"
    } else {
        "****"
    };
    let edited = shown(&pending.edits);
    let hide = |text: &str| match target.harness {
        Harness::Claude => mask_json(text, mask, &edited),
        Harness::Codex => mask_toml(text, mask),
    };
    let (before, after) = (hide(target.before.as_deref().unwrap_or("")), hide(after));
    let path = style.file(&target.file);
    let mut text = format!(
        "\n{}",
        style.spans(&[(&path, BOLD)], style.width, MARGIN, None)
    );
    let diff = similar::TextDiff::from_lines(&before, &after);
    for hunk in diff.unified_diff().context_radius(1).iter_hunks() {
        text += &style.raw(&hunk.header().to_string(), MARGIN, DIM);
        for change in hunk.iter_changes() {
            let (sign, color) = match change.tag() {
                similar::ChangeTag::Delete => ("-", RED),
                similar::ChangeTag::Insert => ("+", GREEN),
                similar::ChangeTag::Equal => (" ", DIM),
            };
            let line = change
                .value()
                .trim_end_matches(['\n', '\r'])
                .replace('\t', "    ");
            text += &style.raw(format!("{sign} {line}").trim_end(), MARGIN, color);
        }
    }
    text
}

/// Whether the JSON value at `path` may hold a secret, so the confirm masks it: a variable
/// or header of an MCP server, or a variable in a settings file's `env` block that no edit
/// in `edited` sets.
fn secret_json(path: &[Option<&str>], edited: &[&[String]]) -> bool {
    match path {
        [
            ..,
            Some("mcpServers"),
            Some(_),
            Some("env" | "headers"),
            Some(_),
        ] => true,
        [Some("env"), Some(key)] => !edited
            .iter()
            .any(|p| matches!(p, [env, k] if env == "env" && k == key)),
        _ => false,
    }
}

/// `text` with each scalar value that [`secret_json`] names replaced by `mask` in quotes,
/// keeping every other byte, so lines and their numbers stay as they are.
fn mask_json(text: &str, mask: &str, edited: &[&[String]]) -> String {
    let b = text.as_bytes();
    // Each open container: whether it is an object, and its current key.
    let mut stack: Vec<(bool, Option<String>)> = vec![];
    let mut want_key = false;
    let (mut out, mut copied, mut i) = (String::with_capacity(text.len()), 0, 0);
    while i < b.len() {
        let start = i;
        match b[i] {
            b'{' | b'[' => {
                stack.push((b[i] == b'{', None));
                want_key = b[i] == b'{';
                i += 1;
                continue;
            }
            b'}' | b']' => {
                stack.pop();
                want_key = false;
                i += 1;
                continue;
            }
            b',' => {
                want_key = matches!(stack.last(), Some((true, _)));
                i += 1;
                continue;
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(b.len());
                if want_key {
                    let key = serde_json::from_str::<String>(&text[start..i]).ok();
                    if let Some(top) = stack.last_mut() {
                        top.1 = key;
                    }
                    want_key = false;
                    continue;
                }
            }
            c if c == b'-' || c.is_ascii_alphanumeric() => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b"+-.".contains(&b[i])) {
                    i += 1;
                }
            }
            _ => {
                i += 1;
                continue;
            }
        }
        // A scalar value spans start..i.
        let path: Vec<Option<&str>> = stack
            .iter()
            .map(|(object, key)| if *object { key.as_deref() } else { None })
            .collect();
        if secret_json(&path, edited) {
            out.push_str(&text[copied..start]);
            out.push_str(&format!("\"{mask}\""));
            copied = i;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// A config.toml with each MCP server's `env` and `http_headers` values, and each variable
/// `shell_environment_policy.set` sets, replaced by `mask`, keeping the rest as written.
fn mask_toml(text: &str, mask: &str) -> String {
    use toml_edit::{DocumentMut, Item, TableLike};
    fn table(item: Option<&mut Item>) -> Option<&mut dyn TableLike> {
        item.and_then(Item::as_table_like_mut)
    }
    let Ok(mut doc) = text.parse::<DocumentMut>() else {
        return text.into();
    };
    let hide = |table: Option<&mut dyn TableLike>| {
        for (_, item) in table.into_iter().flat_map(|t| t.iter_mut()) {
            if let Some(value) = item.as_value_mut() {
                let decor = value.decor().clone();
                // A value over several lines keeps its line breaks in a multi-line literal,
                // so the lines after it keep their numbers.
                let mut bare = value.clone();
                bare.decor_mut().clear();
                let breaks = bare.to_string().matches('\n').count();
                *value = if breaks == 0 {
                    mask.into()
                } else {
                    format!("'''{mask}{}'''", "\n".repeat(breaks))
                        .parse()
                        .unwrap_or_else(|_| mask.into())
                };
                *value.decor_mut() = decor;
            }
        }
    };
    if let Some(servers) = table(doc.get_mut("mcp_servers")) {
        for (_, server) in servers.iter_mut() {
            if let Some(server) = server.as_table_like_mut() {
                hide(table(server.get_mut("env")));
                hide(table(server.get_mut("http_headers")));
            }
        }
    }
    if let Some(policy) = table(doc.get_mut("shell_environment_policy")) {
        hide(table(policy.get_mut("set")));
    }
    doc.to_string()
}

/// A file the doctor wrote.
#[derive(Debug, Serialize, Deserialize)]
pub struct Written {
    pub file: String,
    /// Where the edits landed, when the file is a symlink.
    pub resolved: Option<String>,
    /// The copy of the file as it was, when doctor kept one.
    pub backup: Option<String>,
    pub edits: usize,
}

/// The file's text with `edits` applied and `removals` deleted, or why they do not apply.
fn apply(target: &Target, edits: &[Edit], removals: &[Entry]) -> Result<String, String> {
    let shown = target.resolved.display();
    let text = target.before.as_deref().unwrap_or("");
    let paths: Vec<&[String]> = removals.iter().map(|e| e.path.as_slice()).collect();
    match target.harness {
        Harness::Claude => apply_json(text, edits).and_then(|t| remove_json(&t, &paths)),
        Harness::Codex => apply_toml(text, edits).and_then(|t| remove_toml(&t, &paths)),
    }
    .map_err(|reason| format!("{reason} in {shown}"))
}

/// Whether each entry in `removals` holds in `now` what it held in `walked`, the file the
/// confirm's diff was made from, so a removal never deletes what the person did not see.
/// Other keys may change, and an entry already gone leaves nothing to remove.
fn removals_unchanged(walked: &Target, now: &Target, removals: &[Entry]) -> bool {
    if removals.is_empty() {
        return true;
    }
    let parse = |t: &Target| -> Option<Value> { serde_json::from_str(t.before.as_deref()?).ok() };
    let (walked, now) = (parse(walked), parse(now));
    let at = |root: &Option<Value>, path: &[String]| {
        path.iter()
            .try_fold(root.as_ref()?, |node, key| node.get(key))
            .cloned()
    };
    removals.iter().all(|e| match at(&now, &e.path) {
        None => true,
        found => found == at(&walked, &e.path),
    })
}

/// A file's checked change, ready to write.
struct Step<'a> {
    target: Target,
    text: String,
    pending: &'a Pending,
    /// Claude Code's lock on the file, held from when it was read until it is written.
    lock: Option<Lock>,
    staged: Option<Staged>,
}

/// Write each file's accepted changes, after copying each as it was to its backup. Every
/// file is checked first, so a file that changed while the person was answering stops the
/// run before anything is written. A file Claude Code rewrites while it runs is instead
/// locked and read again first, so its removals are made to what it holds then, and stays
/// locked until it is written. Every backup is staged beside its file before any file is
/// written, so a failure to back one up writes nothing; each replaces its earlier backup
/// only once its own file is written.
pub fn write(plan: &Plan) -> Result<Vec<Written>, ScanError> {
    let mut checked = vec![];
    for pending in &plan.files {
        let mut target = pending.target.clone();
        let shown = target.resolved.display().to_string();
        let read = if target.lock {
            target
                .reread()
                .map(|()| removals_unchanged(&pending.target, &target, &pending.removals))
        } else {
            target.unchanged()
        };
        match read {
            Ok(true) => {}
            Ok(false) => {
                return Err(ScanError::new(format!(
                    "{shown} changed while token-max was running; nothing was written, so run it again"
                )));
            }
            Err(e) => {
                return Err(ScanError::new(format!(
                    "cannot read {shown} ({e}); nothing was written"
                )));
            }
        }
        let text = apply(&target, &pending.edits, &pending.removals).map_err(ScanError::new)?;
        checked.push((target, text, pending));
    }
    // A lock another program holds then stops the run before anything is written.
    checked.sort_by_key(|(target, _, _)| !target.lock);
    let mut steps: Vec<Step> = vec![];
    for (mut target, mut text, pending) in checked {
        let shown = target.resolved.display().to_string();
        let lock = if target.lock {
            let lock = Lock::take(Path::new(&target.file), crate::mcp::WAIT).map_err(|e| {
                ScanError::new(format!("cannot lock {shown} ({e}); nothing was written"))
            })?;
            target.reread().map_err(|e| {
                ScanError::new(format!("cannot read {shown} ({e}); nothing was written"))
            })?;
            if !removals_unchanged(&pending.target, &target, &pending.removals) {
                return Err(ScanError::new(format!(
                    "{shown} changed while token-max was running; nothing was written, so run it again"
                )));
            }
            text = apply(&target, &pending.edits, &pending.removals)
                .map_err(|e| ScanError::new(format!("{e}; nothing was written")))?;
            Some(lock)
        } else {
            None
        };
        steps.push(Step {
            target,
            text,
            pending,
            lock,
            staged: None,
        });
    }
    for step in &mut steps {
        step.staged = step.target.stage_backup().map_err(|e| {
            let shown = step.target.resolved.display();
            ScanError::new(format!(
                "cannot back up {shown} ({e}); nothing was written, and no earlier backup was replaced"
            ))
        })?;
    }
    let mut written: Vec<Written> = vec![];
    for step in steps {
        let Step {
            target,
            text,
            pending,
            lock,
            staged,
        } = step;
        let done = |written: &[Written]| {
            let done: Vec<&str> = written.iter().map(|w| w.file.as_str()).collect();
            if done.is_empty() {
                "nothing else was written".to_string()
            } else {
                format!("already wrote {}", done.join(", "))
            }
        };
        let shown = target.resolved.display().to_string();
        if let Err(e) = target.write(&text, lock.as_ref()) {
            return Err(ScanError::new(format!(
                "cannot write {shown} ({e}); {}",
                done(&written)
            )));
        }
        let backup = staged.as_ref().map(|s| s.bak.display().to_string());
        if let Some(staged) = staged {
            let temp = staged.temp.display().to_string();
            staged.commit().map_err(|e| {
                let bak = backup.as_deref().unwrap_or_default();
                ScanError::new(format!(
                    "wrote {shown}, but cannot replace its backup {bak} ({e}); its previous text is kept in {temp}; {}",
                    done(&written)
                ))
            })?;
        }
        // Held through the backup's commit too, so a run that waited for the lock never has
        // its backup replaced by an earlier run's older copy.
        drop(lock);
        written.push(Written {
            file: target.file.clone(),
            resolved: (shown != target.file).then_some(shown),
            backup,
            edits: pending.changes(),
        });
    }
    Ok(written)
}

/// The closing lines for one host: each file it wrote.
pub fn wrote(written: &[Written], style: &Style) -> String {
    let mut text = String::new();
    for w in written {
        let landed = w.resolved.as_ref().map_or_else(String::new, |r| {
            format!(", through its symlink to {}", style.file(r))
        });
        let kept = w.backup.as_ref().map_or_else(String::new, |b| {
            format!(" Backed up the old file to {}.", style.file(b))
        });
        let wrote = format!(
            "Wrote {} edit{} to {}{landed}.{kept}",
            w.edits,
            if w.edits == 1 { "" } else { "s" },
            style.file(&w.file)
        );
        text += &style.spans(&[(&wrote, GREEN)], style.width, MARGIN, None);
    }
    text
}

/// What the doctor did on every host in `hosts`, each with what it wrote: the files, or
/// that it wrote nothing.
pub fn finish(hosts: &[(&Summary, &[Written], &Style)], style: &Style) -> String {
    let mut text = String::from("\n");
    for (_, written, style) in hosts {
        text += &wrote(written, style);
    }
    if hosts.iter().any(|(_, written, _)| !written.is_empty()) {
        return text;
    }
    let summaries = || hosts.iter().map(|(summary, _, _)| summary);
    text += &style.line(if !summaries().any(|s| s.answered) {
        if hosts.len() > 1 {
            "Nothing to change on these hosts."
        } else {
            "Nothing to change on this host."
        }
    } else if summaries().all(|s| s.offered == 0) {
        "token-max cannot make these changes; the steps by hand are listed above."
    } else if summaries().all(|s| s.files.is_empty()) {
        "Nothing was accepted, so nothing was written."
    } else {
        "Nothing was written."
    });
    text
}

/// A settings.json with `edits` applied, keeping its key order and indentation.
fn apply_json(text: &str, edits: &[Edit]) -> Result<String, String> {
    let mut root = if text.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(text).map_err(|_| "the file is not valid JSON".to_string())?
    };
    let Value::Object(map) = &mut root else {
        return Err("the file does not hold a JSON object".into());
    };
    for edit in edits {
        let (last, parents) = edit.path.split_last().expect("edits name a setting");
        if edit.op == Op::Remove {
            let mut node = Some(&mut *map);
            for key in parents {
                node = node.and_then(|n| n.get_mut(key)?.as_object_mut());
            }
            if let Some(node) = node {
                node.shift_remove(last);
            }
            continue;
        }
        let mut node = &mut *map;
        for (i, key) in parents.iter().enumerate() {
            let child = node
                .entry(key.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            node = child
                .as_object_mut()
                .ok_or_else(|| format!("{} is not an object", edit.path[..=i].join(".")))?;
        }
        if edit.op == Op::Set {
            node.insert(last.clone(), edit.value.clone());
            continue;
        }
        let list = node
            .entry(last.clone())
            .or_insert_with(|| Value::Array(vec![]))
            .as_array_mut()
            .ok_or_else(|| format!("{} is not a list", edit.path.join(".")))?;
        if !list.contains(&edit.value) {
            list.push(edit.value.clone());
        }
    }
    let indent = indentation(text);
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    root.serialize(&mut serde_json::Serializer::with_formatter(
        &mut out, formatter,
    ))
    .expect("JSON values serialize");
    let mut out = String::from_utf8(out).expect("JSON is UTF-8");
    if text.ends_with('\n') || text.trim().is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// The indentation of a JSON file's first indented line; two spaces when it has none.
fn indentation(text: &str) -> String {
    for line in text.lines().skip(1) {
        let indent: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        if indent.starts_with('\t') {
            return "\t".into();
        }
        if !indent.is_empty() {
            return indent;
        }
    }
    "  ".into()
}

/// A config.toml with `edits` applied, keeping its comments, order and formatting.
pub(crate) fn apply_toml(text: &str, edits: &[Edit]) -> Result<String, String> {
    use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, TableLike};
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|_| "the file is not valid TOML".to_string())?;
    for edit in edits {
        let (last, parents) = edit.path.split_last().expect("edits name a setting");
        if edit.op == Op::Remove {
            let mut table: Option<&mut dyn TableLike> = Some(doc.as_table_mut());
            for key in parents {
                table = table.and_then(|t| t.get_mut(key)?.as_table_like_mut());
            }
            if let Some(table) = table {
                table.remove(last);
            }
            continue;
        }
        let mut table: &mut dyn TableLike = doc.as_table_mut();
        for (i, key) in parents.iter().enumerate() {
            let item = table.entry(key).or_insert_with(|| {
                let mut new = Table::new();
                new.set_implicit(true);
                Item::Table(new)
            });
            table = item
                .as_table_like_mut()
                .ok_or_else(|| format!("{} is not a table", edit.path[..=i].join(".")))?;
        }
        let path = edit.path.join(".");
        let value = toml_value(&edit.value);
        if edit.op == Op::Set {
            match table.get_mut(last) {
                Some(Item::Value(old)) => {
                    let decor = old.decor().clone();
                    *old = value;
                    *old.decor_mut() = decor;
                }
                Some(_) => return Err(format!("{path} is a table")),
                None => {
                    table.insert(last, Item::Value(value));
                }
            }
            continue;
        }
        // A table joins a list of tables; one with the same path, as a skill's entry has,
        // is updated in place instead.
        if let Value::Object(fields) = &edit.value {
            let matches = |t: &dyn TableLike| {
                fields.get("path").is_some_and(|p| {
                    t.get("path")
                        .and_then(Item::as_value)
                        .is_some_and(|v| same(v, p))
                })
            };
            let update = |t: &mut dyn TableLike| {
                for (key, v) in fields {
                    match t.get_mut(key) {
                        Some(Item::Value(old)) => {
                            let decor = old.decor().clone();
                            *old = toml_value(v);
                            *old.decor_mut() = decor;
                        }
                        _ => {
                            t.insert(key, Item::Value(toml_value(v)));
                        }
                    }
                }
            };
            match table.get_mut(last) {
                None => {
                    let mut list = ArrayOfTables::new();
                    let mut new = Table::new();
                    update(&mut new);
                    list.push(new);
                    table.insert(last, Item::ArrayOfTables(list));
                }
                Some(Item::ArrayOfTables(list)) => {
                    let found = list.iter().position(|t| matches(t));
                    match found {
                        Some(i) => update(list.get_mut(i).expect("found above")),
                        None => {
                            let mut new = Table::new();
                            update(&mut new);
                            list.push(new);
                        }
                    }
                }
                Some(Item::Value(toml_edit::Value::Array(list))) => {
                    let found = list
                        .iter_mut()
                        .find_map(|v| v.as_inline_table_mut().filter(|t| matches(&**t)));
                    match found {
                        Some(t) => update(t),
                        None => list.push(value),
                    }
                }
                Some(_) => return Err(format!("{path} is not a list")),
            }
            continue;
        }
        match table.get_mut(last) {
            None => {
                let mut list = toml_edit::Array::new();
                list.push(value);
                table.insert(last, Item::Value(toml_edit::Value::Array(list)));
            }
            Some(Item::Value(toml_edit::Value::Array(list))) => {
                if !list.iter().any(|v| same(v, &edit.value)) {
                    list.push(value);
                }
            }
            Some(_) => return Err(format!("{path} is not a list")),
        }
    }
    Ok(doc.to_string())
}

/// A Claude Code state file without the keys at `paths`, keeping its key order and
/// indentation. A key already gone is left so.
fn remove_json(text: &str, paths: &[&[String]]) -> Result<String, String> {
    if paths.is_empty() {
        return Ok(text.into());
    }
    let mut root: Value =
        serde_json::from_str(text).map_err(|_| "the file is not valid JSON".to_string())?;
    for path in paths {
        let (last, parents) = path.split_last().expect("entries name a key");
        let mut node = root.as_object_mut();
        for key in parents {
            node = node
                .and_then(|n| n.get_mut(key))
                .and_then(Value::as_object_mut);
        }
        if let Some(node) = node {
            node.shift_remove(last);
        }
    }
    let mut out = Vec::new();
    let indent = indentation(text);
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    root.serialize(&mut serde_json::Serializer::with_formatter(
        &mut out, formatter,
    ))
    .expect("JSON values serialize");
    let mut out = String::from_utf8(out).expect("JSON is UTF-8");
    if text.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// A config.toml without the tables at `paths`, keeping its comments and formatting. A
/// table already gone is left so.
fn remove_toml(text: &str, paths: &[&[String]]) -> Result<String, String> {
    use toml_edit::{DocumentMut, Item};
    if paths.is_empty() {
        return Ok(text.into());
    }
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|_| "the file is not valid TOML".to_string())?;
    for path in paths {
        let (last, parents) = path.split_last().expect("entries name a key");
        let mut table = Some(doc.as_table_mut() as &mut dyn toml_edit::TableLike);
        for key in parents {
            table = table
                .and_then(|t| t.get_mut(key))
                .and_then(Item::as_table_like_mut);
        }
        if let Some(table) = table {
            table.remove(last);
        }
    }
    Ok(doc.to_string())
}

/// A JSON setting value as TOML.
fn toml_value(value: &Value) -> toml_edit::Value {
    match value {
        Value::Bool(b) => (*b).into(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            None => n.as_f64().unwrap_or(f64::NAN).into(),
        },
        Value::String(s) => s.as_str().into(),
        Value::Array(items) => toml_edit::Value::Array(items.iter().map(toml_value).collect()),
        Value::Object(fields) => {
            let mut table = toml_edit::InlineTable::new();
            for (key, v) in fields {
                table.insert(key, toml_value(v));
            }
            toml_edit::Value::InlineTable(table)
        }
        Value::Null => "".into(),
    }
}

/// Whether a TOML scalar equals a JSON one.
fn same(toml: &toml_edit::Value, json: &Value) -> bool {
    match (toml, json) {
        (toml_edit::Value::String(a), Value::String(b)) => a.value() == b,
        (toml_edit::Value::Boolean(a), Value::Bool(b)) => a.value() == b,
        (toml_edit::Value::Integer(a), Value::Number(b)) => b.as_i64() == Some(*a.value()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Workspace;

    fn edit(path: &str, value: impl Into<Value>, append: bool) -> Edit {
        let op = if append { Op::Append } else { Op::Set };
        Edit {
            path: path.split('.').map(String::from).collect(),
            value: value.into(),
            op,
        }
    }

    #[test]
    fn a_switch_is_one_edit_per_settings_file_its_line_covers() {
        use crate::test_support::{make_profile, make_t3_profile};
        let native = make_profile("native", "/synthetic/.claude");
        let t3 = make_t3_profile("agent", "/synthetic/.claude");
        let work = make_profile("work", "/synthetic/.claude-work");
        let deny = || edit("permissions.deny", "Agent", true);
        // Native Claude Code and a T3 instance sharing ~/.claude get one edit there.
        let How::Edit(patch) = switch(&[&native, &t3], deny(), false) else {
            panic!("one shared settings file is one edit")
        };
        assert_eq!(patch.file, "/synthetic/.claude/settings.json");
        // Requests copied into two homes' histories could have come from either.
        let How::Manual { step, reason } = switch(&[&native, &work], deny(), false) else {
            panic!("two settings files are a step by hand")
        };
        assert!(
            step.starts_with(
                "in /synthetic/.claude/settings.json and /synthetic/.claude-work/settings.json, set "
            ),
            "{step}"
        );
        assert!(reason.contains("several profiles"), "{reason}");
    }

    #[cfg(unix)]
    #[test]
    fn a_switch_across_homes_names_outranking_files_and_one_linked_file_is_one_edit() {
        use crate::test_support::make_profile;
        let w = Workspace::new();
        let text = |p: &Path| p.to_string_lossy().into_owned();
        let project = w.home.join("repo/.claude/settings.json");
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(&project, r#"{"enableArtifact": true}"#).unwrap();
        let mut native = make_profile("native", &text(&w.claude));
        native.outranking_files = vec![text(&project)];
        let work = make_profile("work", &text(&w.home.join(".claude-work")));
        let off = || edit("enableArtifact", false, false);
        // A project that turns it back on is named, since the homes' edits would not hold.
        let How::Manual { step, .. } = switch(&[&native, &work], off(), false) else {
            panic!("two homes are a step by hand")
        };
        assert!(
            step.contains(&format!("and in {} where it is set", text(&project))),
            "{step}"
        );
        // Two homes whose settings.json links one dotfile take one edit to it.
        std::fs::write(w.claude.join("settings.json"), "{}").unwrap();
        let alias = w.home.join(".claude-alias");
        std::fs::create_dir_all(&alias).unwrap();
        std::os::unix::fs::symlink(w.claude.join("settings.json"), alias.join("settings.json"))
            .unwrap();
        let linked = make_profile("linked", &text(&alias));
        native.outranking_files.clear();
        assert!(matches!(
            switch(&[&native, &linked], off(), false),
            How::Edit(_)
        ));
    }

    /// A change saving $10/mo, 10% of the host's spend; its report line is its name's
    /// first byte.
    fn change(name: &str, file: &Path, harness: Harness, edits: Vec<Edit>) -> Fix {
        Fix {
            name: name.into(),
            value: "on -> off".into(),
            suggested: false,
            saving: Some(10.0),
            margin: None,
            verb: "saves",
            share: Some(0.1),
            what: String::new(),
            evidence: String::new(),
            origins: vec![Origin::Check(usize::from(name.as_bytes()[0]))],
            group: None,
            removal: vec![],
            how: How::Edit(Patch {
                file: file.to_string_lossy().into_owned(),
                harness,
                edits,
            }),
        }
    }

    /// What became of the change named `name`.
    fn choice<'a>(plan: &'a Plan, name: &str) -> &'a Choice {
        let origin = Origin::Check(usize::from(name.as_bytes()[0]));
        &plan.choices.iter().find(|(o, _)| *o == origin).unwrap().1
    }

    /// The step by hand the change named `name` became.
    fn by_hand<'a>(plan: &'a Plan, name: &str) -> &'a str {
        match choice(plan, name) {
            Choice::Manual(step) => step,
            other => panic!("{name} is {other:?}, not by hand"),
        }
    }

    fn style(w: &Workspace) -> Style {
        Style {
            color: false,
            unicode: false,
            width: None,
            home: Some(w.home.to_string_lossy().into_owned()),
            host: None,
        }
    }

    /// Walk this host's `fixes` and `turned_off` servers as `doctor` does, returning its plan.
    fn ask(
        fixes: Vec<Fix>,
        turned_off: Vec<Definition>,
        style: &Style,
        input: &mut impl BufRead,
        output: &mut impl Write,
    ) -> io::Result<Asked> {
        let owner = Owner::new(fixes, turned_off, style.home.clone(), 100.0);
        let hosts = [Walking {
            label: None,
            offers: owner.offers(),
            style,
        }];
        let selections = super::ask(&hosts, style, input, output)?;
        Ok(Asked {
            plan: owner.plan(selections[0].events()).unwrap(),
            summary: selections[0].summary(owner.offers()),
        })
    }

    /// One host's plan and its summary, as `doctor` makes both from the walk.
    struct Asked {
        plan: Plan,
        summary: Summary,
    }

    impl std::ops::Deref for Asked {
        type Target = Plan;

        fn deref(&self) -> &Plan {
            &self.plan
        }
    }

    /// Confirm one host's plan as `doctor` does.
    fn confirm(
        plan: &Asked,
        style: &Style,
        input: &mut impl BufRead,
        output: &mut impl Write,
    ) -> io::Result<bool> {
        let preview = preview(plan, style);
        let request = Confirm {
            preview: &preview,
            hosts: vec![(&plan.summary, style)],
        };
        super::confirm(&request, style, input, output)
    }

    /// The closing lines for one host.
    fn finish(plan: &Asked, written: &[Written], style: &Style) -> String {
        super::finish(&[(&plan.summary, written, style)], style)
    }

    /// Walk `fixes` answering with `answers`, returning the plan and what was printed.
    fn walk(fixes: Vec<Fix>, w: &Workspace, answers: &str) -> (Asked, String) {
        let mut printed = vec![];
        let plan = ask(
            fixes,
            vec![],
            &style(w),
            &mut answers.as_bytes(),
            &mut printed,
        )
        .unwrap();
        (plan, String::from_utf8(printed).unwrap())
    }

    #[test]
    fn billing_is_asked_per_root_until_answered_and_end_of_input_skips_every_host() {
        let w = Workspace::new();
        let home = w.home.to_string_lossy().into_owned();
        let mut printed = vec![];
        let mut questions = BillingQuestions::default();
        let mut ask = |root: &str, style: &Style, input: &str| {
            questions
                .ask(root, style, &mut input.as_bytes(), &mut printed)
                .unwrap()
        };
        // An unrecognized answer asks the same profile again.
        let local = format!("{home}/.claude-a");
        assert_eq!(ask(&local, &style(&w), "maybe\nP\n"), Some(Billing::Plan));
        assert_eq!(ask(&local, &style(&w), "skip\n"), None);
        assert_eq!(ask(&local, &style(&w), "api\n"), Some(Billing::Api));
        // A destination's roots are shown from its home, after its name, and escaped.
        let remote = Style {
            home: Some("/home/dev".into()),
            host: Some("devbox".into()),
            ..style(&w)
        };
        assert_eq!(ask("/home/dev/.claude", &remote, "a\n"), Some(Billing::Api));
        // Input ends on this one, so no later question is asked.
        assert_eq!(ask("/srv/\u{1b}[2J", &remote, ""), None);
        assert_eq!(ask("/home/dev/.claude", &remote, "p\n"), None);
        questions.finish(&mut printed).unwrap();
        let printed = String::from_utf8(printed).unwrap();
        assert_eq!(printed.matches("How is ~/.claude-a paid for?").count(), 4);
        assert!(
            printed.contains("How is devbox:~/.claude paid for?"),
            "{printed}"
        );
        assert!(
            printed.contains("How is devbox:/srv/\\u001b[2J paid for?"),
            "{printed}"
        );
        assert_eq!(printed.matches("paid for?").count(), 6, "{printed}");
        assert!(
            printed.ends_with("\n\n"),
            "the report follows as its own section"
        );
        let mut printed = vec![];
        BillingQuestions::default().finish(&mut printed).unwrap();
        assert!(printed.is_empty(), "nothing to ask prints nothing");
    }

    #[test]
    fn the_opening_counts_each_file_once_under_its_first_root() {
        use crate::test_support::{make_profile, make_snapshot, make_t3_profile};
        let snapshot = |profiles: &[&str], bytes| crate::model::Snapshot {
            profiles: profiles.iter().map(|p| p.to_string()).collect(),
            bytes,
            ..make_snapshot("", &[], 0)
        };
        let mut codex = make_profile("codex", "/home/u/.codex");
        codex.harness = Harness::Codex;
        let discovery = crate::discovery::Discovery {
            roots: vec![],
            // T3's instance shares the native root, and so its files.
            profiles: vec![
                make_profile("claude", "/home/u/.claude"),
                make_t3_profile("t3", "/home/u/.claude"),
                codex,
            ],
            snapshots: vec![
                snapshot(&["claude", "t3"], 1_500_000_000),
                snapshot(&["t3"], 500),
                snapshot(&["codex"], 1),
            ],
            t3: vec![],
        };
        let found = Found::list(&discovery);
        let claude = Found {
            harness: Harness::Claude,
            root: "/home/u/.claude".into(),
            files: 2,
            bytes: 1_500_000_500,
        };
        let codex = Found {
            harness: Harness::Codex,
            root: "/home/u/.codex".into(),
            files: 1,
            bytes: 1,
        };
        assert_eq!(found, [claude, codex]);

        let style = |unicode, width| Style {
            color: false,
            unicode,
            width,
            home: Some("/home/u".into()),
            host: None,
        };
        let text = opening(&found, &[], &style(true, Some(100)));
        assert!(text.contains("  ▀█▀ █▀█"), "{text}");
        assert!(
            text.contains("\n  ✓ Claude Code  ~/.claude  2 transcripts  1.5 GB\n"),
            "{text}"
        );
        assert!(
            text.contains("\n  ✓ Codex        ~/.codex   1 transcript      1 B\n"),
            "{text}"
        );
        assert!(text.contains("token-max doctor --ssh <host>"), "{text}");
        // A narrow or plain terminal gets the name as text, and SSH destinations replace the hint.
        let devbox = ["devbox".to_string()];
        for narrow in [style(true, Some(40)), style(false, None)] {
            let text = opening(&found, &devbox, &narrow);
            assert!(text.starts_with("\n  token-max "), "{text}");
            assert!(!text.contains("█") && !text.contains("<host>"), "{text}");
            assert!(text.contains("\n  + devbox over SSH\n"), "{text}");
        }
        // A table wider than the terminal puts each root's figures below it.
        let text = opening(&found, &devbox, &style(true, Some(40)));
        assert!(text.lines().all(|l| l.chars().count() <= 40), "{text}");
        assert!(
            text.contains("\n  ✓ Claude Code  ~/.claude\n    2 transcripts  1.5 GB\n"),
            "{text}"
        );
        assert!(
            text.contains("\n  ✓ Codex        ~/.codex\n    1 transcript      1 B\n"),
            "{text}"
        );
        assert!(opening(&[], &[], &style(false, None)).contains("No transcripts found"));
    }

    #[test]
    fn json_edits_keep_key_order_and_indentation_and_add_to_lists_once() {
        let text = "{\n    \"z\": 1,\n    \"permissions\": {\n        \"deny\": [\"Read\"]\n    },\n    \"a\": true\n}\n";
        let edited = apply_json(
            text,
            &[
                edit("permissions.deny", "Agent", true),
                edit("permissions.deny", "Read", true),
                edit("a", false, false),
                edit("env.X", "1", false),
                edit("z", 2, false),
            ],
        )
        .unwrap();
        assert_eq!(
            edited,
            "{\n    \"z\": 2,\n    \"permissions\": {\n        \"deny\": [\n            \"Read\",\n            \"Agent\"\n        ]\n    },\n    \"a\": false,\n    \"env\": {\n        \"X\": \"1\"\n    }\n}\n"
        );
        assert_eq!(
            apply_json("{\n\t\"a\": 1\n}", &[edit("a", 2, false)]).unwrap(),
            "{\n\t\"a\": 2\n}"
        );
        assert_eq!(
            apply_json("", &[edit("a", 1, false)]).unwrap(),
            "{\n  \"a\": 1\n}\n"
        );
        // A removal deletes the key, keeping its parent, and is a no-op where nothing is set.
        let remove = |path: &[&str]| Edit::remove(path);
        assert_eq!(
            apply_json(
                "{\n  \"env\": {\n    \"A\": \"50\",\n    \"B\": \"1\"\n  }\n}\n",
                &[
                    remove(&["env", "A"]),
                    remove(&["env", "C"]),
                    remove(&["x", "y"])
                ],
            )
            .unwrap(),
            "{\n  \"env\": {\n    \"B\": \"1\"\n  }\n}\n"
        );
        assert_eq!(
            apply_toml(
                "a = 1 # keep\nb = 2\n",
                &[remove(&["b"]), remove(&["c", "d"])]
            )
            .unwrap(),
            "a = 1 # keep\n"
        );
        for (text, error) in [
            (r#"{"permissions": "x"}"#, "permissions is not an object"),
            (
                r#"{"permissions": {"deny": "x"}}"#,
                "permissions.deny is not a list",
            ),
            ("[]", "the file does not hold a JSON object"),
        ] {
            assert_eq!(
                apply_json(text, &[edit("permissions.deny", "Agent", true)]),
                Err(error.into())
            );
        }
    }

    #[test]
    fn toml_edits_keep_comments_and_update_a_skill_entry_in_place() {
        let text = "# mine\nmodel = \"gpt\" # keep\n\n[features]\napps = true # apps\n\n[[skills.config]]\npath = \"/a/SKILL.md\"\nenabled = true\n";
        let skill = |path: &str| serde_json::json!({ "path": path, "enabled": false });
        let edited = apply_toml(
            text,
            &[
                edit("features.apps", false, false),
                edit("features.memories", false, false),
                edit("tui.auto_recap", false, false),
                edit("model_auto_compact_token_limit", 200_000, false),
                edit("mcp_servers.docs.disabled_tools", "write", true),
                edit("mcp_servers.docs.disabled_tools", "write", true),
                edit("skills.config", skill("/a/SKILL.md"), true),
                edit("skills.config", skill("/b/SKILL.md"), true),
            ],
        )
        .unwrap();
        for kept in [
            "# mine\n",
            "model = \"gpt\" # keep\n",
            "apps = false # apps\n",
        ] {
            assert!(edited.contains(kept), "{kept:?} in {edited}");
        }
        let parsed: toml::Table = toml::from_str(&edited).unwrap();
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::json!({
                "model": "gpt",
                "model_auto_compact_token_limit": 200_000,
                "features": { "apps": false, "memories": false },
                "skills": { "config": [skill("/a/SKILL.md"), skill("/b/SKILL.md")] },
                "tui": { "auto_recap": false },
                "mcp_servers": { "docs": { "disabled_tools": ["write"] } },
            })
        );
        assert_eq!(
            apply_toml("features = 1\n", &[edit("features.apps", false, false)]),
            Err("features is not a table".into())
        );
        assert_eq!(
            apply_toml("[tui]\n", &[edit("tui", false, false)]),
            Err("tui is a table".into())
        );
    }

    #[test]
    fn a_host_replays_only_answers_to_the_changes_it_offered() {
        let w = Workspace::new();
        let settings = w.claude.join("settings.json");
        std::fs::write(&settings, "{}\n").unwrap();
        let fix = change(
            "A",
            &settings,
            Harness::Claude,
            vec![edit("a", true, false)],
        );
        let owner = Owner::new(vec![fix.clone()], vec![], None, 0.0);
        // As another host's walk reads them.
        let json = serde_json::to_string(owner.offers()).unwrap();
        let offers: Offers = serde_json::from_str(&json).unwrap();
        assert_eq!(offers.offers[0].fix, fix);
        assert_eq!(offers.files, [settings.to_string_lossy()]);
        let choose = |offer, yes| Event::Choose { offer, yes };
        let remove = Event::Remove { offer: 0, entry: 0 };
        for (events, refused) in [
            (vec![choose(1, true)], "not offered"),
            (vec![Event::Cleanup { entry: 0 }], "not offered"),
            // A removal follows only a yes to its change, and names one of its entries.
            (vec![remove], "did not accept"),
            (vec![choose(0, false), remove], "did not accept"),
            (vec![choose(0, true), remove], "not offered"),
        ] {
            let error = owner.plan(&events).unwrap_err().message;
            assert!(error.contains(refused), "{events:?}: {error}");
        }
        let plan = owner
            .plan(&[Event::Choose {
                offer: 0,
                yes: true,
            }])
            .unwrap();
        assert_eq!(
            plan.files
                .iter()
                .map(|p| &p.target.file)
                .collect::<Vec<_>>(),
            [&settings.to_string_lossy()]
        );
        assert_eq!(*choice(&plan, "A"), Choice::Accepted);
        assert_eq!(offers.check(), Ok(()));
        let mut stray = offers.clone();
        stray.offers[0].target = 1;
        assert!(stray.check().is_err());
        let mut manual = offers.clone();
        manual.offers[0].fix.how = How::Manual {
            step: "edit it".into(),
            reason: "why".into(),
        };
        assert!(manual.check().is_err());
        // A saving verb this build never prints is refused.
        assert!(json.contains("\"saves\""));
        let forged = json.replace("\"saves\"", "\"proves\"");
        assert!(serde_json::from_str::<Offers>(&forged).is_err());
    }

    #[test]
    fn the_walk_writes_each_file_once_with_its_accepted_edits() {
        let w = Workspace::new();
        let settings = w.claude.join("settings.json");
        let config = w.codex.join("config.toml");
        std::fs::write(&settings, "{\n  \"keep\": 1\n}\n").unwrap();
        let fixes = vec![
            change(
                "A",
                &settings,
                Harness::Claude,
                vec![edit("a", true, false)],
            ),
            change(
                "B",
                &settings,
                Harness::Claude,
                vec![edit("b", true, false)],
            ),
            change(
                "C",
                &settings,
                Harness::Claude,
                vec![edit("c", true, false)],
            ),
            change("D", &config, Harness::Codex, vec![edit("d", true, false)]),
            change("E", &config, Harness::Codex, vec![edit("e", true, false)]),
            Fix {
                how: How::Manual {
                    step: "set X=1 where it is exported".into(),
                    reason: "the X environment variable sets it".into(),
                },
                ..change("F", &config, Harness::Codex, vec![])
            },
        ];
        // Anything but an answer asks again; a quit leaves the rest unaccepted.
        let (plan, printed) = walk(fixes, &w, "y\nmaybe\nn\nyes\nq\n");
        assert_eq!(printed.matches("Accept? [y]es, [n]o, [q]uit: ").count(), 5);
        assert!(printed.starts_with("\n  5 questions, biggest saving first.\n  Nothing is written until you confirm the list at the end.\n"));
        // A card shows the change, not the edit, which the confirm shows once as a diff.
        assert!(printed.contains(
            "\n  [1/5] A  saves $10/mo  10%\n        on -> off  Recommended\n  Accept? [y]es, [n]o, [q]uit:   Accepted so far: about $10/mo, 10% of this host's spend.\n"
        ), "{printed}");
        assert!(printed.contains(
            "  Accepted so far: about $20/mo, 20% of this host's spend; overlapping changes may save less.\n"
        ));
        for (name, want) in [
            ("A", Choice::Accepted),
            ("B", Choice::Declined),
            ("C", Choice::Accepted),
            ("D", Choice::Declined),
            ("E", Choice::Declined),
        ] {
            assert_eq!(choice(&plan, name), &want, "{name}");
        }
        assert_eq!(
            by_hand(&plan, "F"),
            "by hand, since the X environment variable sets it: set X=1 where it is exported"
        );
        assert_eq!(
            plan.total("Accepted"),
            "Accepted: about $20/mo, 20% of this host's spend; overlapping changes may save less."
        );
        // The confirm shows each file's diff, names each file, and asks again until it gets
        // an answer.
        let mut printed = vec![];
        assert!(confirm(&plan, &style(&w), &mut "sure\ny\n".as_bytes(), &mut printed).unwrap());
        let printed = String::from_utf8(printed).unwrap();
        let shown = format!("~{0}.claude{0}settings.json", std::path::MAIN_SEPARATOR);
        assert!(printed.starts_with(&format!(
            "\n  {shown}\n  @@ -1,3 +1,5 @@\n    {{\n  -   \"keep\": 1\n  +   \"keep\": 1,\n  +   \"a\": true,\n  +   \"c\": true\n    }}\n\n  Backs up {shown} to {shown}.token-max.bak first.\n\n  Write"
        )), "{printed}");
        assert_eq!(
            printed
                .matches(&format!(
                    "\n  Write the 2 accepted changes to {shown}? [y]es, [n]o: "
                ))
                .count(),
            2
        );
        let written = write(&plan).unwrap();
        assert_eq!(
            std::fs::read_to_string(&settings).unwrap(),
            "{\n  \"keep\": 1,\n  \"a\": true,\n  \"c\": true\n}\n"
        );
        assert!(!config.exists());
        assert_eq!(
            finish(&plan, &written, &style(&w)),
            format!(
                "\n  Wrote 2 edits to {shown}. Backed up the old file to {shown}.token-max.bak.\n"
            )
        );
        assert_eq!(
            std::fs::read_to_string(settings.with_extension("json.token-max.bak")).unwrap(),
            "{\n  \"keep\": 1\n}\n"
        );
        // End of input is a quit in the walk and a no at the confirm.
        let z = || {
            vec![change(
                "A",
                &settings,
                Harness::Claude,
                vec![edit("z", 1, false)],
            )]
        };
        let (plan, _) = walk(z(), &w, "");
        assert_eq!(choice(&plan, "A"), &Choice::Declined);
        assert!(!confirm(&plan, &style(&w), &mut "".as_bytes(), &mut vec![]).unwrap());
        assert_eq!(
            finish(&plan, &[], &style(&w)),
            "\n  Nothing was accepted, so nothing was written.\n"
        );
        let (plan, _) = walk(z(), &w, "y\n");
        let mut printed = vec![];
        assert!(!confirm(&plan, &style(&w), &mut "".as_bytes(), &mut printed).unwrap());
        assert!(
            String::from_utf8(printed)
                .unwrap()
                .ends_with("[y]es, [n]o: \n")
        );
        assert_eq!(finish(&plan, &[], &style(&w)), "\n  Nothing was written.\n");
        assert!(
            !std::fs::read_to_string(&settings)
                .unwrap()
                .contains("\"z\"")
        );
        assert_eq!(
            finish(&walk(vec![], &w, "").0, &[], &style(&w)),
            "\n  Nothing to change on this host.\n"
        );
    }

    #[test]
    fn the_walk_groups_small_changes_by_kind_and_reason_and_skips_groups_under_a_dollar() {
        let w = Workspace::new();
        let settings = w.claude.join("settings.json");
        let fix = |name: &str, saving: Option<f64>| Fix {
            saving,
            share: saving.map(|usd| usd / 100.0),
            ..change(name, &settings, Harness::Claude, vec![edit(name, 1, false)])
        };
        let member = |name: &str, saving, reason, label: &str| Fix {
            group: Some(Group {
                kind: ContextKind::Skill,
                reason,
                label: label.into(),
                member: name.to_lowercase(),
                location: None,
            }),
            ..fix(name, saving)
        };
        let unused = "Never-used skills";
        // In the report's order: measured savings, largest first, then unmeasured.
        let fixes = || {
            vec![
                fix("A", Some(50.0)),
                member("B", Some(6.0), Reason::Unused, unused),
                member("S", Some(3.0), Reason::Unused, unused),
                member("M", Some(2.0), Reason::Costly, "Skills over $1 per use"),
                member("T", Some(1.5), Reason::Unused, unused),
                member(
                    "P",
                    Some(0.4),
                    Reason::Unrecorded,
                    "Skills with no recorded use",
                ),
                fix("C", Some(0.3)),
                Fix {
                    suggested: true,
                    ..fix("U", None)
                },
                member("K", None, Reason::Unused, unused),
                member("L", None, Reason::Unused, unused),
            ]
        };
        // Split the measured group, declining B, and the unmeasured one, accepting K.
        let (plan, printed) = walk(fixes(), &w, "y\ns\nn\ny\ny\ny\nn\ns\ny\nn\n");
        let heads: Vec<&str> = printed
            .lines()
            .filter_map(|l| l.strip_prefix("  ").filter(|l| l.starts_with('[')))
            .collect();
        // A change of $5/mo or more joins its group too, the groups rank by their sum, and
        // the unmeasured come last, grouped apart from the measured.
        assert_eq!(
            heads,
            [
                "[1/5] A  saves $50/mo  50%",
                "[2/5] Never-used skills  saves $11/mo  11%",
                "[2/5, 1/3] B  saves $6/mo  6%",
                "[2/5, 2/3] S  saves $3/mo  3%",
                "[2/5, 3/3] T  saves $1.50/mo  2%",
                "[3/5] M  saves $2/mo  2%",
                "[4/5] U  saving not measured",
                "[5/5] Never-used skills  savings not measured",
                "[5/5, 1/2] K  saving not measured",
                "[5/5, 2/2] L  saving not measured",
            ]
        );
        assert!(printed.contains("\n  5 questions about 8 changes, biggest saving first.\n"));
        assert!(printed.contains(
            "\n  [2/5] Never-used skills  saves $11/mo  11%\n        on -> off  Recommended\n        What   Skills are packaged instructions the model can load when a task calls for one; each is listed by name and description in every request.\n        Which  b, s, t.\n  Accept all 3? [y]es, [n]o, [s]plit, [q]uit: "
        ), "{printed}");
        assert!(printed.contains(
            "\n  [5/5] Never-used skills  savings not measured\n        on -> off  Recommended\n        What   Skills are packaged instructions the model can load when a task calls for one; each is listed by name and description in every request.\n        Which  k, l.\n"
        ), "{printed}");
        assert!(printed.ends_with(
            "\n  2 more changes in groups saving under $1/mo were not asked; they're in the list below.\n"
        ), "{printed}");
        for (name, want) in [
            ("A", Some(Choice::Accepted)),
            ("B", Some(Choice::Declined)),
            ("S", Some(Choice::Accepted)),
            ("T", Some(Choice::Accepted)),
            ("M", Some(Choice::Accepted)),
            ("U", Some(Choice::Declined)),
            ("K", Some(Choice::Accepted)),
            ("L", Some(Choice::Declined)),
            // Never asked, so not accepted.
            ("P", Some(Choice::Declined)),
            ("C", Some(Choice::Declined)),
        ] {
            let origin = Origin::Check(usize::from(name.as_bytes()[0]));
            let got = plan
                .choices
                .iter()
                .find(|(o, _)| *o == origin)
                .map(|c| &c.1);
            assert_eq!(got, want.as_ref(), "{name}");
        }
        assert_eq!(
            plan.total("Accepted"),
            "Accepted: about $57/mo, 57% of this host's spend, plus 1 change whose saving was not measured; overlapping changes may save less."
        );
        // A quit at a group leaves it and the rest unaccepted.
        let (plan, printed) = walk(fixes(), &w, "n\nq\n");
        assert!(!printed.contains("[3/5]"));
        for name in ["S", "T", "M", "K"] {
            assert_eq!(choice(&plan, name), &Choice::Declined, "{name}");
        }
        // With nothing worth asking, the walk only says what it left out.
        let (plan, printed) = walk(vec![fix("C", Some(0.3))], &w, "");
        assert_eq!(
            printed,
            "\n  1 change in groups saving under $1/mo was not asked; it's in the list below.\n"
        );
        assert_eq!(choice(&plan, "C"), &Choice::Declined);
        assert_eq!(
            finish(&plan, &[], &style(&w)),
            "\n  Nothing was accepted, so nothing was written.\n"
        );
    }

    #[test]
    fn a_group_names_the_location_of_members_that_share_a_name() {
        let w = Workspace::new();
        let settings = w.claude.join("settings.json");
        let home = w.home.to_string_lossy().into_owned();
        let copy = |line: &str, member: &str, at: Option<&str>| Fix {
            saving: Some(2.0),
            group: Some(Group {
                kind: ContextKind::Skill,
                reason: Reason::Duplicate,
                label: "Duplicate copies of skills".into(),
                member: member.into(),
                location: at.map(|at| format!("{home}/{at}/SKILL.md")),
            }),
            ..change(line, &settings, Harness::Claude, vec![edit(line, 1, false)])
        };
        let (_, printed) = walk(
            vec![
                copy("A", "review", Some("a")),
                copy("B", "review", Some("b")),
                copy("C", "lint", Some("c")),
            ],
            &w,
            "n\n",
        );
        assert!(
            printed.contains("  Which  review (~/a/SKILL.md), review (~/b/SKILL.md), lint.\n"),
            "{printed}"
        );
    }

    #[test]
    fn a_report_line_whose_edit_another_makes_is_offered_once_with_both_lines() {
        let w = Workspace::new();
        let settings = w.claude.join("settings.json");
        let same = || vec![edit("a", 1, false)];
        let fixes = fixes_from(vec![
            change("A", &settings, Harness::Claude, same()),
            change("B", &settings, Harness::Claude, same()),
        ]);
        assert_eq!(fixes.len(), 1);
        let (plan, _) = walk(fixes, &w, "y\n");
        assert_eq!(choice(&plan, "A"), &Choice::Accepted);
        assert_eq!(choice(&plan, "B"), &Choice::Accepted);
        assert_eq!(
            plan.total("Accepted"),
            "Accepted: about $10/mo, 10% of this host's spend."
        );
    }

    #[test]
    fn savings_across_hosts_add_with_independent_margins_and_a_share_of_all_their_spend() {
        let file = Path::new("/synthetic/settings.json");
        let fix = |usd: Option<f64>, margin| Fix {
            saving: usd,
            margin,
            ..change("A", file, Harness::Claude, vec![])
        };
        // Each host's own changes add their margins, since they may share requests.
        let host = |fixes: &[Fix], monthly| {
            let mut total = Total::of(fixes);
            total.set_share(monthly);
            total
        };
        let mut total = host(
            &[
                fix(Some(30.0), Some(Margin::Usd(3.0))),
                fix(Some(10.0), Some(Margin::Usd(1.0))),
            ],
            400.0,
        );
        total.merge(&host(
            &[fix(Some(60.0), Some(Margin::Usd(3.0))), fix(None, None)],
            600.0,
        ));
        total.set_share(1000.0);
        // 4 and 3 combine in quadrature; the share is of both hosts' spend together.
        assert_eq!(
            total.text("Changes offered", "these hosts'"),
            "Changes offered: ~$100/mo ± $5, 10% of these hosts' spend, plus 1 change whose saving was not measured; overlapping changes may save less."
        );
        assert_eq!(total.changes(), 4);
    }

    #[test]
    fn the_total_keeps_savings_that_depend_on_a_default_or_were_priced_in_part_qualified() {
        let file = Path::new("/synthetic/settings.json");
        let fix = |verb: &'static str, usd: f64| Fix {
            verb,
            saving: Some(usd),
            share: Some(usd / 100.0),
            ..change("A", file, Harness::Claude, vec![])
        };
        let total = |fixes: &[Fix]| {
            let mut total = Total::default();
            for fix in fixes {
                total.add(fix);
            }
            total.text("Accepted", THIS_HOST)
        };
        // An unset TTL's gap may save nothing, so it is never summed as a sure saving.
        assert_eq!(
            total(&[fix(UP_TO, 30.0)]),
            "Accepted: up to $30/mo, 30% of this host's spend, from 1 change whose saving depends on the default it replaces."
        );
        assert_eq!(
            total(&[fix("saves", 10.0), fix(AT_LEAST, 5.0), fix(UP_TO, 30.0)]),
            "Accepted: about $15/mo, 15% of this host's spend, more where 1 change was priced only in part, plus up to $30/mo, 30% of this host's spend, from 1 change whose saving depends on the default it replaces; overlapping changes may save less."
        );
        let unmeasured = Fix {
            saving: None,
            ..fix("saves", 0.0)
        };
        assert_eq!(
            total(&[unmeasured.clone(), unmeasured]),
            "Accepted: 2 changes whose savings were not measured."
        );
        assert_eq!(total(&[]), "");
        // Extrapolated savings carry a ~, their margins added since they may share requests,
        // and one with no spread to measure leaves the total's margin unknown.
        let extrapolated = |usd, margin| Fix {
            margin: Some(margin),
            ..fix("saves", usd)
        };
        assert_eq!(
            total(&[
                fix("saves", 10.0),
                extrapolated(20.0, Margin::Usd(2.0)),
                extrapolated(5.0, Margin::Usd(1.0))
            ]),
            "Accepted: ~$35/mo ± $3, 35% of this host's spend; overlapping changes may save less."
        );
        assert_eq!(
            total(&[
                extrapolated(20.0, Margin::Usd(2.0)),
                extrapolated(5.0, Margin::Unmeasured)
            ]),
            "Accepted: ~$25/mo, margin unknown, 25% of this host's spend; overlapping changes may save less."
        );
        // A group's heading sums its members the same way.
        let w = Workspace::new();
        let settings = w.claude.join("settings.json");
        let small = |name: &str, verb, usd| Fix {
            verb,
            saving: Some(usd),
            share: Some(usd / 100.0),
            group: Some(Group {
                kind: ContextKind::Tool,
                reason: Reason::Unused,
                label: "Never-used tools".into(),
                member: name.into(),
                location: None,
            }),
            ..change(name, &settings, Harness::Claude, vec![edit(name, 1, false)])
        };
        let (_, printed) = walk(
            vec![small("A", "saves", 1.0), small("B", UP_TO, 3.0)],
            &w,
            "y\n",
        );
        // The heading's figure holds only up to its sum; the running total says which part.
        assert!(
            printed.contains("  [1/1] Never-used tools  saves up to $4/mo  4%\n"),
            "{printed}"
        );
        assert!(printed.contains(
            "  Accepted so far: about $1/mo, 1% of this host's spend, plus up to $3/mo, 3% of this host's spend, from 1 change whose saving depends on the default it replaces; overlapping changes may save less.\n"
        ), "{printed}");
    }

    #[test]
    fn a_card_puts_its_saving_at_the_right_and_hangs_its_paragraphs() {
        let style = |width, color| Style {
            color,
            unicode: true,
            width,
            home: None,
            host: None,
        };
        let card = Card {
            counter: "[1/4]".into(),
            name: "WebSearch tool",
            saving: Some(("saves $206/mo".into(), "39%".into())),
            no_saving: "saving not measured",
            value: "on → off".into(),
            standing: Standing::Recommended,
            rows: vec![
                (
                    "What",
                    "Lets Claude search the web and read the results.".into(),
                ),
                ("Why", "Never used; it cost $99 over the window.".into()),
            ],
        };
        assert_eq!(
            style(Some(50), false).card(&card),
            "  [1/4] WebSearch tool          saves $206/mo  39%\n        on → off  Recommended\n        What  Lets Claude search the web and read\n              the results.\n        Why   Never used; it cost $99 over the\n              window.\n"
        );
        // A wide terminal still aligns the saving at the card's width.
        let wide = style(Some(200), false).card(&card);
        let head = wide.lines().next().unwrap();
        assert_eq!(crate::terminal::cells(head), CARD_CELLS);
        // Color carries the verdict and the saving; the words stay the same.
        let colored = style(Some(50), true).card(&card);
        assert!(colored.contains("\x1b[31mRecommended\x1b[0m"), "{colored}");
        assert!(
            colored.contains("\x1b[32msaves $206/mo\x1b[0m"),
            "{colored}"
        );
        // Without room for the saving at the right, it follows the name.
        let narrow = style(Some(30), false).card(&card);
        assert!(
            narrow.starts_with("  [1/4] WebSearch tool  saves\n"),
            "{narrow}"
        );
    }

    #[test]
    fn the_walk_leaves_out_the_sentence_naming_the_setting_its_edit_shows() {
        assert_eq!(
            why(
                "Replayed at 5m and 1h; 1h costs less. Set in ~/.claude/settings.json:promptCacheTtl."
            ),
            "Replayed at 5m and 1h; 1h costs less."
        );
        assert_eq!(
            why(
                "Never used; it cost $3 over the window. Turn off: \"permissions\": {\"deny\": [\"X\"]}."
            ),
            "Never used; it cost $3 over the window."
        );
        // A setting named mid-evidence stays.
        let kept = "Set it lower. Commands past the cap keep running.";
        assert_eq!(why(kept), kept);
    }

    #[test]
    fn a_file_that_changed_or_cannot_take_the_edit_is_not_written() {
        let w = Workspace::new();
        let settings = w.claude.join("settings.json");
        std::fs::write(&settings, r#"{"permissions": "all"}"#).unwrap();
        let deny = change(
            "Agent tool",
            &settings,
            Harness::Claude,
            vec![edit("permissions.deny", "Agent", true)],
        );
        let (plan, printed) = walk(vec![deny.clone()], &w, "");
        assert!(!printed.contains("Apply?"));
        let shown = home_in(&settings.display().to_string(), style(&w).home.as_deref());
        assert_eq!(
            by_hand(&plan, "A"),
            format!(
                "by hand, since permissions is not an object in {shown}: in {shown}, set \"permissions\": {{\"deny\": [\"Agent\"]}} (added to the list)"
            )
        );
        assert_eq!(
            finish(&plan, &[], &style(&w)),
            "\n  token-max cannot make these changes; the steps by hand are listed above.\n"
        );
        std::fs::write(&settings, "{}").unwrap();
        let (plan, _) = walk(vec![deny], &w, "y\n");
        std::fs::write(&settings, r#"{"theirs": 1}"#).unwrap();
        let error = write(&plan).unwrap_err().message;
        assert!(
            error.contains("changed while token-max was running"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(&settings).unwrap(),
            r#"{"theirs": 1}"#
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_written_through_unless_their_target_is_read_only() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let w = Workspace::new();
        let dotfiles = w.home.join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        let target = dotfiles.join("settings.json");
        std::fs::write(&target, "{}\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        let settings = w.claude.join("settings.json");
        symlink(&target, &settings).unwrap();
        let fix = change("A", &settings, Harness::Claude, vec![edit("a", 1, false)]);
        let (plan, _) = walk(vec![fix.clone()], &w, "y\n");
        let written = write(&plan).unwrap();
        assert!(
            std::fs::symlink_metadata(&settings)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "{\n  \"a\": 1\n}\n"
        );
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
        assert!(finish(&plan, &written, &style(&w)).starts_with(
            "\n  Wrote 1 edit to ~/.claude/settings.json, through its symlink to ~/dotfiles/settings.json. Backed up the old file to ~/dotfiles/settings.json.token-max.bak.\n"
        ));
        // The backup sits beside the target, with its permissions, and not beside the link.
        let bak = dotfiles.join("settings.json.token-max.bak");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "{}\n");
        assert_eq!(
            std::fs::metadata(&bak).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(!w.claude.join("settings.json.token-max.bak").exists());
        // Root can write anything, so only another user sees a read-only target.
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o444)).unwrap();
            let (plan, printed) = walk(vec![fix], &w, "");
            assert!(!printed.contains("Apply?"));
            let shown = home_in(&target.display().to_string(), style(&w).home.as_deref());
            assert_eq!(
                by_hand(&plan, "A"),
                format!("by hand, since {shown} is read-only: in {shown}, set \"a\": 1")
            );
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        // Two profiles that link one dotfile write it once, with both accepted edits.
        let work = w.home.join(".claude-work");
        std::fs::create_dir_all(&work).unwrap();
        let other = work.join("settings.json");
        symlink(&target, &other).unwrap();
        let (plan, _) = walk(
            vec![
                change("A", &settings, Harness::Claude, vec![edit("b", 2, false)]),
                change("B", &other, Harness::Claude, vec![edit("c", 3, false)]),
            ],
            &w,
            "y\ny\n",
        );
        assert_eq!(plan.files.len(), 1);
        write(&plan).unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}\n"
        );
        // So do two lines that set one setting through either name: the second is by hand.
        let fixes = super::fixes_from(vec![
            change("A", &settings, Harness::Claude, vec![edit("b", 4, false)]),
            change("B", &other, Harness::Claude, vec![edit("b", 5, false)]),
        ]);
        assert!(matches!(fixes[1].how, How::Manual { .. }), "{fixes:?}");
        // A new file is readable only by its owner.
        let fresh = w.home.join("fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        let created = fresh.join("settings.json");
        let (plan, _) = walk(
            vec![change(
                "A",
                &created,
                Harness::Claude,
                vec![edit("a", 1, false)],
            )],
            &w,
            "y\n",
        );
        write(&plan).unwrap();
        let mode = std::fs::metadata(&created).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let dangling = w.claude.join("dangling.json");
        symlink(w.home.join("missing.json"), &dangling).unwrap();
        let (plan, _) = walk(
            vec![change(
                "A",
                &dangling,
                Harness::Claude,
                vec![edit("a", 1, false)],
            )],
            &w,
            "",
        );
        let step = by_hand(&plan, "A");
        assert!(step.contains("is a symlink to a missing file: "), "{step}");
    }

    /// A never-used MCP server's change saving `usd`, turned off in `settings`, whose
    /// configuration is `removal`.
    fn server(name: &str, usd: f64, settings: &Path, removal: Vec<Definition>) -> Fix {
        Fix {
            saving: Some(usd),
            share: Some(usd / 100.0),
            group: Some(Group {
                kind: ContextKind::McpServer,
                reason: Reason::Unused,
                label: "Never-used MCP servers".into(),
                member: name.to_lowercase(),
                location: None,
            }),
            removal,
            ..change(
                name,
                settings,
                Harness::Claude,
                vec![edit("permissions.deny", format!("mcp__{name}"), true)],
            )
        }
    }

    fn entry(file: &Path, harness: Harness, path: &[&str]) -> Entry {
        Entry {
            server: path.last().unwrap().to_string(),
            file: file.to_string_lossy().into_owned(),
            harness,
            path: path.iter().map(|p| p.to_string()).collect(),
        }
    }

    #[test]
    fn a_yes_to_never_used_mcp_servers_offers_to_remove_their_configuration() {
        let w = Workspace::new();
        let settings = w.claude.join("settings.json");
        let state = w.home.join(".claude.json");
        let config = w.codex.join("config.toml");
        std::fs::write(
            &state,
            "{\n  \"numStartups\": 3,\n  \"mcpServers\": {\n    \"A\": {},\n    \"B\": {},\n    \"keep\": {\n      \"env\": {\n        \"K\": \"s3cret\"\n      }\n    }\n  },\n  \"projects\": {\n    \"/p\": {\n      \"mcpServers\": {\n        \"B\": {\n          \"headers\": {\n            \"Authorization\": \"Bearer s3cret\"\n          }\n        }\n      }\n    }\n  }\n}\n",
        )
        .unwrap();
        std::fs::write(
            &config,
            "model = \"m\" # mine\n\n[mcp_servers.off]\nenabled = false\n[mcp_servers.off.env]\nX = \"1\"\n\n[mcp_servers.on]\ncommand = \"x\"\n",
        )
        .unwrap();
        let a = entry(&state, Harness::Claude, &["mcpServers", "A"]);
        let b = entry(&state, Harness::Claude, &["mcpServers", "B"]);
        let b_local = entry(
            &state,
            Harness::Claude,
            &["projects", "/p", "mcpServers", "B"],
        );
        let plugin = Definition::Manual {
            step: "uninstall or disable the plugin that provides it".into(),
            reason: "a plugin adds it".into(),
        };
        let fixes = || {
            vec![
                // Offered for another reason, so asked alone, and its removal too.
                Fix {
                    group: Some(Group {
                        reason: Reason::Costly,
                        label: "MCP servers over $1 per use".into(),
                        member: "a".into(),
                        kind: ContextKind::McpServer,
                        location: None,
                    }),
                    ..server("A", 6.0, &settings, vec![Definition::Entry(a.clone())])
                },
                server(
                    "B",
                    2.0,
                    &settings,
                    vec![
                        Definition::Entry(b.clone()),
                        Definition::Entry(b_local.clone()),
                    ],
                ),
                server("C", 1.0, &settings, vec![plugin.clone()]),
            ]
        };
        let off = entry(&config, Harness::Codex, &["mcp_servers", "off"]);
        // A server a question above offers is not offered again as off already.
        let turned_off = vec![Definition::Entry(off.clone()), Definition::Entry(b.clone())];
        let mut printed = vec![];
        let plan = ask(
            fixes(),
            turned_off.clone(),
            &style(&w),
            &mut "y\nn\ny\ny\ny\n".as_bytes(),
            &mut printed,
        )
        .unwrap();
        let printed = String::from_utf8(printed).unwrap();
        assert!(
            printed.starts_with("\n  3 questions about 4 changes,"),
            "{printed}"
        );
        let sep = std::path::MAIN_SEPARATOR;
        assert!(printed.contains(&format!(
            "  It stays configured; token-max can also remove:\n    mcpServers.A in ~{sep}.claude.json\n  Remove it too? [y]es, [n]o, [q]uit: "
        )), "{printed}");
        // A group's removal is one question, naming what only a person can remove.
        assert!(printed.contains(&format!(
            "  They stay configured; token-max can also remove:\n    mcpServers.B in ~{sep}.claude.json\n    mcpServers.B of project /p in ~{sep}.claude.json\n  To remove it by hand, since a plugin adds it: uninstall or disable the plugin that provides it.\n  Remove all 2 too? [y]es, [n]o, [q]uit: "
        )), "{printed}");
        assert!(printed.contains(&format!(
            "\n  [3/3] MCP server off  saves nothing\n        off -> removed  Worth considering\n        Why    Its tools are off already, so no session uses this configuration.\n        Where  [mcp_servers.off] in ~{sep}.codex{sep}config.toml\n  Remove it? [y]es, [n]o, [q]uit: "
        )), "{printed}");
        for name in ["A", "B", "C"] {
            assert_eq!(choice(&plan, name), &Choice::Accepted, "{name}");
        }
        let mut shown = vec![];
        assert!(confirm(&plan, &style(&w), &mut "y\n".as_bytes(), &mut shown).unwrap());
        let shown = String::from_utf8(shown).unwrap();
        // Removals show as the lines they delete, with a line of context.
        assert!(shown.contains(&format!(
            "\n  ~{sep}.claude.json\n  @@ -4,3 +4,2 @@\n        \"A\": {{}},\n  -     \"B\": {{}},\n        \"keep\": {{\n"
        )), "{shown}");
        assert!(shown.contains(
            "\n  - [mcp_servers.off]\n  - enabled = false\n  - [mcp_servers.off.env]\n  - X = \"****\"\n  -\n    [mcp_servers.on]\n"
        ), "{shown}");
        assert!(
            shown.contains("\n  -             \"Authorization\": \"****\"\n"),
            "{shown}"
        );
        // A removal names each variable and header but never prints a value.
        assert!(
            !shown.contains("s3cret") && !shown.contains("\"1\""),
            "{shown}"
        );
        assert!(
            shown.contains("Write the 3 accepted changes and 3 removals to "),
            "{shown}"
        );
        // An entry it would remove changed during the walk: nothing is written, since the
        // diff showed another version.
        let walked = std::fs::read_to_string(&state).unwrap();
        std::fs::write(
            &state,
            walked.replace(
                "\"B\": {},\n    \"keep",
                "\"B\": {\"command\": \"b\"},\n    \"keep",
            ),
        )
        .unwrap();
        let err = write(&plan).unwrap_err().to_string();
        assert!(
            err.contains("changed while token-max was running; nothing was written"),
            "{err}"
        );
        let bak = |file: &Path| PathBuf::from(format!("{}.token-max.bak", file.display()));
        assert!(!bak(&state).exists() && !bak(&config).exists());
        let before_config = std::fs::read_to_string(&config).unwrap();
        assert!(
            std::fs::read_to_string(&config)
                .unwrap()
                .contains("[mcp_servers.off]")
        );
        // Claude Code saved its state file during the walk: the removal applies to what it
        // holds now rather than refusing it.
        std::fs::write(
            &state,
            "{\n  \"numStartups\": 4,\n  \"mcpServers\": {\n    \"A\": {},\n    \"B\": {},\n    \"keep\": {\n      \"env\": {\n        \"K\": \"s3cret\"\n      }\n    }\n  },\n  \"projects\": {\n    \"/p\": {\n      \"mcpServers\": {\n        \"B\": {\n          \"headers\": {\n            \"Authorization\": \"Bearer s3cret\"\n          }\n        }\n      }\n    }\n  }\n}\n",
        )
        .unwrap();
        let reread = std::fs::read_to_string(&state).unwrap();
        let written = write(&plan).unwrap();
        // The state file's backup holds what the locked re-read returned, not what the
        // walk read.
        assert_eq!(std::fs::read_to_string(bak(&state)).unwrap(), reread);
        assert_eq!(
            std::fs::read_to_string(bak(&config)).unwrap(),
            before_config
        );
        assert_eq!(
            std::fs::read_to_string(&state).unwrap(),
            "{\n  \"numStartups\": 4,\n  \"mcpServers\": {\n    \"A\": {},\n    \"keep\": {\n      \"env\": {\n        \"K\": \"s3cret\"\n      }\n    }\n  },\n  \"projects\": {\n    \"/p\": {\n      \"mcpServers\": {}\n    }\n  }\n}\n"
        );
        assert!(!w.home.join(".claude.json.lock").exists());
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "model = \"m\" # mine\n\n[mcp_servers.on]\ncommand = \"x\"\n"
        );
        assert_eq!(written.iter().map(|w| w.edits).sum::<usize>(), 6);

        // A no to the removal keeps the change; a no to the change asks nothing more.
        let mut printed = vec![];
        let plan = ask(
            fixes(),
            vec![],
            &style(&w),
            &mut "y\nn\nn\n".as_bytes(),
            &mut printed,
        )
        .unwrap();
        let printed = String::from_utf8(printed).unwrap();
        assert_eq!(printed.matches("too? ").count(), 1, "{printed}");
        assert_eq!(choice(&plan, "A"), &Choice::Accepted);
        assert!(plan.files.iter().all(|p| p.removals.is_empty()));
    }

    /// A change adding `key` to `file`, as the walk accepts it.
    fn accepted(w: &Workspace, files: &[(&Path, Harness)]) -> Asked {
        let fixes = files
            .iter()
            .enumerate()
            .map(|(i, (file, harness))| {
                let name = ["A", "B", "C"][i];
                change(name, file, *harness, vec![edit("key", 1, false)])
            })
            .collect();
        walk(fixes, w, "y\ny\ny\n").0
    }

    #[test]
    fn each_write_replaces_the_one_backup_with_the_file_as_it_was() {
        let w = Workspace::new();
        let settings = w.claude.join("settings.json");
        let bak = w.claude.join("settings.json.token-max.bak");
        std::fs::write(&settings, "{}\n").unwrap();
        write(&accepted(&w, &[(&settings, Harness::Claude)])).unwrap();
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "{}\n");
        let first = std::fs::read_to_string(&settings).unwrap();
        // Only the latest change is undone by copying the backup back.
        std::fs::write(&settings, format!("{first}\n")).unwrap();
        let (plan, _) = walk(
            vec![change(
                "A",
                &settings,
                Harness::Claude,
                vec![edit("other", 2, false)],
            )],
            &w,
            "y\n",
        );
        write(&plan).unwrap();
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), format!("{first}\n"));
        // A file that does not exist yet has nothing to keep.
        let fresh = w.codex.join("config.toml");
        let printed = {
            let plan = accepted(&w, &[(&fresh, Harness::Codex)]);
            let mut printed = vec![];
            confirm(&plan, &style(&w), &mut "y\n".as_bytes(), &mut printed).unwrap();
            write(&plan).unwrap();
            String::from_utf8(printed).unwrap()
        };
        assert!(
            !printed.contains("Backs up") && !printed.contains("Git"),
            "{printed}"
        );
        assert!(!w.codex.join("config.toml.token-max.bak").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_backup_that_cannot_be_staged_writes_nothing_and_keeps_every_earlier_backup() {
        let w = Workspace::new();
        let settings = w.claude.join("settings.json");
        let config = w.codex.join("config.toml");
        std::fs::write(&settings, "{}\n").unwrap();
        std::fs::write(&config, "a = 1\n").unwrap();
        std::fs::write(w.claude.join("settings.json.token-max.bak"), "older\n").unwrap();
        let plan = accepted(
            &w,
            &[(&settings, Harness::Claude), (&config, Harness::Codex)],
        );
        // Something already sits where the second file's backup is staged.
        let blocker = w
            .codex
            .join(format!(".config.toml.token-max.bak-{}", std::process::id()));
        std::fs::write(&blocker, "theirs").unwrap();
        let error = write(&plan).unwrap_err().message;
        assert!(
            error.contains("cannot back up") && error.contains("nothing was written"),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{}\n");
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "a = 1\n");
        assert_eq!(
            std::fs::read_to_string(w.claude.join("settings.json.token-max.bak")).unwrap(),
            "older\n"
        );
        // The first file's staged copy is cleaned up, and the blocker is not ours to remove.
        let left: Vec<_> = std::fs::read_dir(&w.claude)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), 3, "{left:?}");
        assert!(blocker.exists());
        std::fs::remove_file(&blocker).unwrap();

        // A backup that cannot replace its earlier one is reported, after its file's write.
        let bak = w.codex.join("config.toml.token-max.bak");
        std::fs::create_dir(&bak).unwrap();
        std::fs::write(bak.join("x"), "").unwrap();
        let error = write(&plan).unwrap_err().message;
        assert!(
            error.contains("cannot replace its backup") && error.contains("already wrote"),
            "{error}"
        );
        // The file as it was survives in the staged copy the error names.
        let kept = w
            .codex
            .join(format!(".config.toml.token-max.bak-{}", std::process::id()));
        assert!(error.contains(&kept.display().to_string()), "{error}");
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "a = 1\n");
        assert_eq!(
            std::fs::read_to_string(w.claude.join("settings.json.token-max.bak")).unwrap(),
            "{}\n"
        );
        assert!(std::fs::read_to_string(&config).unwrap().contains("key"));
    }

    #[test]
    fn a_write_that_fails_after_another_names_the_file_already_written() {
        let w = Workspace::new();
        let settings = w.claude.join("settings.json");
        let config = w.codex.join("config.toml");
        let bak = w.codex.join("config.toml.token-max.bak");
        std::fs::write(&settings, "{}\n").unwrap();
        std::fs::write(&config, "a = 1\n").unwrap();
        std::fs::write(&bak, "older\n").unwrap();
        let plan = accepted(
            &w,
            &[(&settings, Harness::Claude), (&config, Harness::Codex)],
        );
        // Something already sits where the second file's new text is staged.
        let blocker = w
            .codex
            .join(format!(".config.toml.token-max-{}", std::process::id()));
        std::fs::write(&blocker, "theirs").unwrap();
        let error = write(&plan).unwrap_err().message;
        let shown = |file: &Path| file.display().to_string();
        assert!(
            error.starts_with(&format!("cannot write {} (", shown(&config)))
                && error.ends_with(&format!("; already wrote {}", shown(&settings))),
            "{error}"
        );
        assert!(std::fs::read_to_string(&settings).unwrap().contains("key"));
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "a = 1\n");
        // The second file's earlier backup stays, its staged one is cleaned up, and the
        // blocker is not ours to remove.
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "older\n");
        let mut left: Vec<_> = std::fs::read_dir(&w.codex)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let blocker = blocker.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            left,
            [
                blocker.as_str(),
                "config.toml",
                "config.toml.token-max.bak",
                "sessions"
            ]
        );
    }

    #[test]
    fn turned_off_mcp_servers_are_removed_together_or_one_by_one() {
        let w = Workspace::new();
        let config = w.codex.join("config.toml");
        let text = "[mcp_servers.a]\nenabled = false\n\n[mcp_servers.b]\nenabled = false\n\n[mcp_servers.c]\nenabled = false\n\n[mcp_servers.on]\ncommand = \"x\"\n";
        let off =
            |name: &str| Definition::Entry(entry(&config, Harness::Codex, &["mcp_servers", name]));
        let run = |answers: &str| {
            std::fs::write(&config, text).unwrap();
            let mut printed = vec![];
            let plan = ask(
                vec![],
                vec![off("a"), off("b"), off("c")],
                &style(&w),
                &mut answers.as_bytes(),
                &mut printed,
            )
            .unwrap();
            write(&plan).unwrap();
            (
                std::fs::read_to_string(&config).unwrap(),
                String::from_utf8(printed).unwrap(),
            )
        };
        // The servers left configured.
        let servers = |text: &str| -> Vec<String> {
            let table: toml::Table = toml::from_str(text).unwrap();
            table["mcp_servers"]
                .as_table()
                .unwrap()
                .keys()
                .cloned()
                .collect()
        };
        let (after, printed) = run("y\n");
        assert!(
            printed.ends_with("  Remove all 3? [y]es, [n]o, [s]plit, [q]uit: "),
            "{printed}"
        );
        assert_eq!(servers(&after), ["on"]);
        assert_eq!(run("n\n").0, text);
        // A split asks each server alone, numbered within the batch.
        let (after, printed) = run("s\nn\ny\nn\n");
        let sep = std::path::MAIN_SEPARATOR;
        assert!(printed.contains(&format!(
            "\n  [1/1, 2/3] MCP server b  saves nothing\n             off -> removed  Worth considering\n             Where  [mcp_servers.b] in ~{sep}.codex{sep}config.toml\n  Remove it? [y]es, [n]o, [q]uit: "
        )), "{printed}");
        assert_eq!(printed.matches("Remove it?").count(), 3, "{printed}");
        assert_eq!(servers(&after), ["a", "c", "on"]);
        // A quit asks nothing more and keeps the removals already accepted.
        let (after, printed) = run("s\ny\nq\n");
        assert!(!printed.contains("3/3"), "{printed}");
        assert_eq!(servers(&after), ["b", "c", "on"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_git_tracks_with_no_uncommitted_changes_gets_no_backup() {
        use std::process::Command;
        let w = Workspace::new();
        let repo = w.home.join("project");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        let settings = repo.join("settings.json");
        std::fs::write(&settings, "{}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "x"]);
        let confirm_text = |plan: &Asked| {
            let mut printed = vec![];
            confirm(plan, &style(&w), &mut "y\n".as_bytes(), &mut printed).unwrap();
            String::from_utf8(printed).unwrap()
        };
        let plan = accepted(&w, &[(&settings, Harness::Claude)]);
        let printed = confirm_text(&plan);
        assert!(
            printed.contains(
                "Git keeps the previous version of ~/project/settings.json, so it gets no backup."
            ),
            "{printed}"
        );
        let written = write(&plan).unwrap();
        assert!(written[0].backup.is_none());
        assert!(!repo.join("settings.json.token-max.bak").exists());
        // Uncommitted changes are not in git, so the file is backed up.
        let plan = accepted(&w, &[(&settings, Harness::Claude)]);
        let printed = confirm_text(&plan);
        assert!(
            printed.contains(
                "Backs up ~/project/settings.json to ~/project/settings.json.token-max.bak first."
            ),
            "{printed}"
        );
        write(&plan).unwrap();
        assert!(repo.join("settings.json.token-max.bak").exists());
        // Git status hides changes to a file marked skip-worktree or assume-unchanged.
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "y", "--allow-empty"]);
        git(&["update-index", "--skip-worktree", "settings.json"]);
        std::fs::write(&settings, "{\"mine\": 1}\n").unwrap();
        let plan = accepted(&w, &[(&settings, Harness::Claude)]);
        assert!(confirm_text(&plan).contains("Backs up"));
        git(&["update-index", "--no-skip-worktree", "settings.json"]);
        git(&["update-index", "--assume-unchanged", "settings.json"]);
        let plan = accepted(&w, &[(&settings, Harness::Claude)]);
        assert!(confirm_text(&plan).contains("Backs up"));
        // A name with glob characters is matched literally: this ignored file is not the
        // tracked `a1.json` beside it.
        std::fs::write(repo.join("a1.json"), "{}\n").unwrap();
        git(&["add", "a1.json"]);
        git(&["commit", "-q", "-m", "z"]);
        std::fs::write(repo.join(".git/info/exclude"), "a[[]1].json\n").unwrap();
        let globbed = repo.join("a[1].json");
        std::fs::write(&globbed, "{}\n").unwrap();
        let written = write(&accepted(&w, &[(&globbed, Harness::Claude)])).unwrap();
        assert!(written[0].backup.is_some());
        // So is a file git does not track.
        let loose = repo.join("local.json");
        std::fs::write(&loose, "{}\n").unwrap();
        let written = write(&accepted(&w, &[(&loose, Harness::Claude)])).unwrap();
        assert!(written[0].backup.is_some());
    }

    #[test]
    fn the_confirm_keeps_a_removed_value_masked() {
        let edits = [
            Edit::set("env.DISABLE_X", "1"),
            Edit::remove(&["env", "CLAUDE_CODE_AUTO_COMPACT_WINDOW"]),
        ];
        let text = "{\n  \"env\": {\"DISABLE_X\": \"1\", \"CLAUDE_CODE_AUTO_COMPACT_WINDOW\": \"sk-pasted\"}\n}\n";
        assert_eq!(
            mask_json(text, "****", &shown(&edits)),
            "{\n  \"env\": {\"DISABLE_X\": \"1\", \"CLAUDE_CODE_AUTO_COMPACT_WINDOW\": \"****\"}\n}\n"
        );
    }

    #[test]
    fn the_confirm_masks_secret_json_values_and_keeps_the_lines() {
        let text = "{\n  \"env\": {\"API_KEY\": \"sk-\\\"x\", \"DISABLE_X\": \"1\"},\n  \"mcpServers\": {\n    \"gh\": {\n      \"command\": \"gh\", \"args\": [\"a\"],\n      \"env\": {\"GITHUB_TOKEN\": \"ghp_1\", \"N\": 3}\n    }\n  },\n  \"projects\": {\"/p\": {\"mcpServers\": {\"web\": {\"headers\": {\"Authorization\": \"Bearer t\"}, \"url\": \"https://x\"}}}}\n}\n";
        let edited = [vec!["env".to_string(), "DISABLE_X".to_string()]];
        let edited: Vec<&[String]> = edited.iter().map(Vec::as_slice).collect();
        assert_eq!(
            mask_json(text, "****", &edited),
            "{\n  \"env\": {\"API_KEY\": \"****\", \"DISABLE_X\": \"1\"},\n  \"mcpServers\": {\n    \"gh\": {\n      \"command\": \"gh\", \"args\": [\"a\"],\n      \"env\": {\"GITHUB_TOKEN\": \"****\", \"N\": \"****\"}\n    }\n  },\n  \"projects\": {\"/p\": {\"mcpServers\": {\"web\": {\"headers\": {\"Authorization\": \"****\"}, \"url\": \"https://x\"}}}}\n}\n"
        );
    }

    #[test]
    fn the_confirm_masks_secret_toml_values_and_keeps_the_rest() {
        let text = "model = \"m\" # mine\n\n[mcp_servers.gh]\ncommand = \"gh\"\nenv = { GITHUB_TOKEN = \"ghp_1\" }\n\n[mcp_servers.web]\nurl = \"https://x\"\n[mcp_servers.web.http_headers]\nAuthorization = \"Bearer t\"  # theirs\nCookie = \"\"\"\na\nb\"\"\"\nX = [\n  \"c\",\n]\n\n[shell_environment_policy.set]\nKEY = \"v\"\n";
        let masked = mask_toml(text, "****");
        assert_eq!(
            masked,
            "model = \"m\" # mine\n\n[mcp_servers.gh]\ncommand = \"gh\"\nenv = { GITHUB_TOKEN = \"****\" }\n\n[mcp_servers.web]\nurl = \"https://x\"\n[mcp_servers.web.http_headers]\nAuthorization = \"****\"  # theirs\nCookie = '''****\n\n'''\nX = '''****\n\n'''\n\n[shell_environment_policy.set]\nKEY = \"****\"\n"
        );
        // A value over several lines keeps them, so the diff's line numbers match the file.
        assert_eq!(masked.lines().count(), text.lines().count());
    }
}
