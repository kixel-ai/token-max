//! `token-max doctor`'s changes: the settings edit behind each recommendation, offered one
//! at a time on a terminal and written to a user settings file only after a yes.
//!
//! Only a profile's own `settings.json` or `config.toml` is edited. A change some other
//! owner would still override, such as a shell variable, a project or managed settings file
//! or T3, is a step to take by hand that names that owner.

use std::fs;
use std::io::{self, BufRead, ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::context::{json_value, setting_text};
use crate::model::{BackgroundFeature, Bucket, CheckResult, Control, Harness, Profile, Report};
use crate::report::{Recommendation, Source, recommendations};
use crate::safe::{ScanError, terminal_text};
use crate::terminal::{WrapOptions, wrap_text};

/// One recommendation and how to carry it out.
#[derive(Clone, Debug, PartialEq)]
pub struct Fix {
    /// The report line's name, such as "Main-agent prompt cache TTL".
    pub name: String,
    /// The current value and the one to change to, such as "5m -> 1h".
    pub value: String,
    /// The kind of claim it rests on: Measured, Official or Opinion.
    pub claim: &'static str,
    /// Worth considering, rather than a recommended change.
    pub suggested: bool,
    pub how: How,
}

/// Whether the doctor can make a change, and what it is.
#[derive(Clone, Debug, PartialEq)]
pub enum How {
    /// Edits to a profile's own settings file.
    Edit(Patch),
    /// A change only the person can make: what to do, and why the doctor cannot.
    Manual { step: String, reason: String },
}

/// Edits to one settings file.
#[derive(Clone, Debug, PartialEq)]
pub struct Patch {
    pub file: String,
    /// Claude's `settings.json` is JSON; Codex's `config.toml` is TOML.
    pub harness: Harness,
    pub edits: Vec<Edit>,
}

/// Set `path` to `value`, or add `value` to the list at `path`.
#[derive(Clone, Debug, PartialEq)]
pub struct Edit {
    pub path: Vec<String>,
    pub value: Value,
    pub append: bool,
}

impl From<&Control> for Edit {
    fn from(control: &Control) -> Self {
        Edit {
            path: control.path.clone(),
            value: json_value(&control.value),
            append: control.append,
        }
    }
}

impl Edit {
    fn set(path: &str, value: impl Into<Value>) -> Self {
        Edit {
            path: path.split('.').map(String::from).collect(),
            value: value.into(),
            append: false,
        }
    }

    /// The edit as a settings fragment, such as `"promptCacheTtl": "1h"`.
    pub fn text(&self, harness: Harness) -> String {
        let text = setting_text(&self.path, self.value.clone(), self.append, harness);
        if self.append {
            format!("{text} (added to the list)")
        } else {
            text
        }
    }
}

/// Every change the report recommends or suggests, in report order, each with its edit
/// or the step to take by hand. An edit another line already makes is listed once.
pub fn fixes(report: &Report, unicode: bool) -> Vec<Fix> {
    let mut fixes: Vec<Fix> = vec![];
    for line in recommendations(report, unicode) {
        let mut fix = fix(report, line);
        if fixes
            .iter()
            .any(|f| matches!(f.how, How::Edit(_)) && f.how == fix.how)
        {
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

/// Whether `how` sets a setting `patch` also sets, to another value.
fn conflicts(how: &How, patch: &Patch) -> bool {
    let How::Edit(other) = how else { return false };
    other.file == patch.file
        && other.edits.iter().any(|a| {
            patch
                .edits
                .iter()
                .any(|b| !a.append && !b.append && a.path == b.path && a.value != b.value)
        })
}

/// The edits as steps to take by hand.
fn steps(patch: &Patch) -> String {
    let edits: Vec<String> = patch.edits.iter().map(|e| e.text(patch.harness)).collect();
    format!("in {}, set {}", patch.file, edits.join(" and "))
}

fn fix(report: &Report, line: Recommendation<'_>) -> Fix {
    let profile_id = match line.source {
        Source::Check(result) => check_profile(result),
        Source::Piece(c) => c.profiles.first().map_or("", String::as_str),
    };
    let profile = report.scan.profiles.iter().find(|p| p.id == profile_id);
    let how = match (profile, line.source) {
        (None, _) => How::Manual {
            step: format!("change it where {profile_id} is configured"),
            reason: "its profile is no longer selected".into(),
        },
        (Some(profile), Source::Check(result)) => check_how(result, profile),
        (Some(profile), Source::Piece(c)) => match &c.control {
            Some(control) => How::Edit(Patch {
                file: profile.settings_file.clone(),
                harness: profile.harness,
                edits: vec![control.into()],
            }),
            None => How::Manual {
                step: "disable it in config.toml with [[skills.config]]".into(),
                reason: "the recorded catalog does not give its SKILL.md path".into(),
            },
        },
    };
    Fix {
        name: line.name,
        value: line.value,
        claim: line.claim,
        suggested: line.suggested,
        how,
    }
}

fn check_profile(result: &CheckResult) -> &str {
    match result {
        CheckResult::Ttl(c) => &c.base.profile,
        CheckResult::Compaction(c) => &c.base.profile,
        CheckResult::Idle(c) => &c.base.profile,
        CheckResult::T3Idle(c) => &c.base.profile,
        CheckResult::Speed(c) => &c.base.profile,
        CheckResult::Background(c) => &c.base.profile,
        CheckResult::BashCap(c) => &c.base.profile,
        CheckResult::Keepalive(c) => &c.base.profile,
        CheckResult::Memory(c) => &c.base.profile,
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
                        append: false,
                    }],
                ));
            }
            owned(owner, profile, candidates, step)
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
        CheckResult::BashCap(b) => {
            let Some(cap) = b.base.recommended else {
                return unknown(&b.base.owner);
            };
            let cap = cap.to_string();
            let step = format!("set {}", b.replacement);
            let max = "BASH_MAX_TIMEOUT_MS";
            let mut how = owned(
                &b.base.owner,
                profile,
                vec![(format!("env.{max}"), vec![env(max, &cap)])],
                step.clone(),
            );
            // A default above the cap raises the cap again, so it comes down too.
            let default = "BASH_DEFAULT_TIMEOUT_MS";
            if let (How::Edit(patch), Some(control)) = (&mut how, &profile.bash)
                && b.replacement.contains(default)
            {
                if !names(&control.default_owner, file, &format!("env.{default}")) {
                    return manual(&control.default_owner, profile, step);
                }
                patch.edits.push(env(default, &cap));
            }
            how
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
    }
}

/// A variable in a Claude settings file's `env` block, which holds strings.
fn env(variable: &str, value: &str) -> Edit {
    Edit {
        path: vec!["env".into(), variable.into()],
        value: value.into(),
        append: false,
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
pub struct Described {
    pub head: String,
    /// Each line, and whether it is a settings fragment that must not wrap.
    pub body: Vec<(String, bool)>,
}

/// How a fix reads in a list or at a prompt, with paths under `home` shown from `~`.
pub fn describe(fix: &Fix, home: Option<&str>) -> Described {
    let home_in = |text: &str| match home {
        Some(dir) => text
            .replace(&format!("{dir}/"), "~/")
            .replace(&format!("{dir}\\"), "~\\"),
        None => text.to_string(),
    };
    let kind = if fix.suggested {
        format!("{}, worth considering", fix.claim)
    } else {
        fix.claim.to_string()
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
            vec![(home_in(&format!("By hand, since {reason}: {step}.")), false)]
        }
    };
    Described { head, body }
}

/// How the walk prints.
pub struct Style {
    pub color: bool,
    pub width: Option<usize>,
    /// The home directory, which paths are shown from as `~`.
    pub home: Option<String>,
}

impl Style {
    fn line(&self, text: &str, indent: usize) -> String {
        let text = wrap_text(
            &terminal_text(text),
            WrapOptions {
                width: self.width,
                indent,
                continuation: None,
            },
        );
        format!("{text}\n")
    }

    fn bold(&self, text: &str, indent: usize) -> String {
        let text = self.line(text, indent);
        if self.color {
            text.split_inclusive('\n')
                .map(|row| match row.strip_suffix('\n') {
                    Some(row) => format!("\x1b[1m{row}\x1b[0m\n"),
                    None => format!("\x1b[1m{row}\x1b[0m"),
                })
                .collect()
        } else {
            text
        }
    }

    /// A fix's heading after `prefix`, then its edit or step, `indent` cells in.
    fn fix(&self, fix: &Fix, prefix: &str, indent: usize) -> String {
        let described = describe(fix, self.home.as_deref());
        let mut text = self.bold(&format!("{prefix}{}", described.head), indent);
        for (row, raw) in described.body {
            if raw {
                // A settings fragment stays whole, so it can be copied at any width.
                let pad = " ".repeat(indent + 2);
                text += &format!("{pad}{}\n", terminal_text(&row));
            } else {
                text += &self.line(&row, indent + 2);
            }
        }
        text
    }
}

/// A settings file the walk may write.
#[derive(Debug)]
struct Target {
    /// The file as its profile names it.
    file: String,
    /// Where an edit lands: the file, or the end of its symlinks.
    resolved: PathBuf,
    harness: Harness,
    /// Its text when the walk began; None when it did not exist.
    before: Option<String>,
}

impl Target {
    /// The file as it is now, or why the doctor cannot write it.
    fn open(file: &str, harness: Harness) -> Result<Target, String> {
        let path = Path::new(file);
        let link = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
        let resolved = if link {
            fs::canonicalize(path).map_err(|e| match e.kind() {
                ErrorKind::NotFound => format!("{file} is a symlink to a missing file"),
                _ => format!("{file} cannot be resolved ({e})"),
            })?
        } else {
            path.to_path_buf()
        };
        let shown = resolved.display().to_string();
        let before = match fs::read(&resolved) {
            Ok(bytes) => {
                Some(String::from_utf8(bytes).map_err(|_| format!("{shown} is not UTF-8 text"))?)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{shown} cannot be read ({e})")),
        };
        let directory = resolved.parent().filter(|d| !d.as_os_str().is_empty());
        let directory = directory.unwrap_or(Path::new("."));
        if !writable(directory) || (before.is_some() && !writable(&resolved)) {
            return Err(format!("{shown} is read-only"));
        }
        Ok(Target {
            file: file.into(),
            resolved,
            harness,
            before,
        })
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

    /// Replace the file with `text` atomically, keeping its permissions and any symlink.
    fn write(&self, text: &str) -> io::Result<()> {
        let name = self
            .resolved
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let temp = self
            .resolved
            .with_file_name(format!(".{name}.token-max-{}", std::process::id()));
        let result = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(text.as_bytes())?;
            if let Ok(metadata) = fs::metadata(&self.resolved) {
                file.set_permissions(metadata.permissions())?;
            }
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, &self.resolved)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
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

/// What the person accepted, declined, and must do by hand.
#[derive(Debug)]
pub struct Plan {
    /// Each file with accepted edits, in the order first accepted.
    files: Vec<(Target, Vec<Edit>)>,
    /// Edits the person declined or left after quitting, to make by hand if they want.
    declined: Vec<Fix>,
    /// Changes only the person can make.
    manual: Vec<Fix>,
}

/// One person's answer at a prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Yes,
    No,
    Quit,
}

/// Offer each fix the doctor can make, one prompt each, and return what to write.
/// Nothing is written here; end of input is a quit.
pub fn ask(
    fixes: Vec<Fix>,
    style: &Style,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<Plan> {
    let mut targets: Vec<Result<Target, String>> = vec![];
    let mut files: Vec<String> = vec![];
    let mut offers = vec![];
    let mut manual = vec![];
    for mut fix in fixes {
        let How::Edit(patch) = &fix.how else {
            manual.push(fix);
            continue;
        };
        let index = match files.iter().position(|f| *f == patch.file) {
            Some(index) => index,
            None => {
                files.push(patch.file.clone());
                targets.push(Target::open(&patch.file, patch.harness));
                files.len() - 1
            }
        };
        // Check the edit applies before offering it.
        let refused = match &targets[index] {
            Err(reason) => Some(reason.clone()),
            Ok(target) => apply(target, &patch.edits).err(),
        };
        if let Some(reason) = refused {
            fix.how = How::Manual {
                step: steps(patch),
                reason,
            };
            manual.push(fix);
            continue;
        }
        offers.push((index, fix));
    }
    let mut accepted: Vec<(usize, Vec<Edit>)> = vec![];
    let mut declined = vec![];
    let total = offers.len();
    if total > 0 {
        write!(
            output,
            "\n{}",
            style.bold(
                &format!(
                "token-max doctor: {total} change{} to review. Each is written only after a yes.",
                if total == 1 { "" } else { "s" }
            ), 0)
        )?;
    }
    let mut quit = false;
    for (n, (index, fix)) in offers.into_iter().enumerate() {
        if quit {
            declined.push(fix);
            continue;
        }
        write!(
            output,
            "\n{}",
            style.fix(&fix, &format!("[{}/{total}] ", n + 1), 0)
        )?;
        let answer = loop {
            write!(output, "Apply? [y]es, [n]o, [q]uit: ")?;
            output.flush()?;
            let mut line = String::new();
            if input.read_line(&mut line)? == 0 {
                writeln!(output)?;
                break Answer::Quit;
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => break Answer::Yes,
                "n" | "no" => break Answer::No,
                "q" | "quit" => break Answer::Quit,
                _ => {}
            }
        };
        let How::Edit(patch) = &fix.how else {
            unreachable!("offers are edits")
        };
        match answer {
            Answer::Yes => match accepted.iter_mut().find(|(i, _)| *i == index) {
                Some((_, edits)) => edits.extend(patch.edits.iter().cloned()),
                None => accepted.push((index, patch.edits.clone())),
            },
            Answer::No => declined.push(fix),
            Answer::Quit => {
                quit = true;
                declined.push(fix);
            }
        }
    }
    let mut targets: Vec<Option<Target>> = targets.into_iter().map(Result::ok).collect();
    let files = accepted
        .into_iter()
        .map(|(index, edits)| {
            let target = targets[index].take().expect("accepted edits have a target");
            (target, edits)
        })
        .collect();
    Ok(Plan {
        files,
        declined,
        manual,
    })
}

/// A file the doctor wrote.
#[derive(Debug)]
pub struct Written {
    pub file: String,
    /// Where the edits landed, when the file is a symlink.
    pub resolved: Option<String>,
    pub edits: usize,
}

/// The file's text with `edits` applied, or why they do not apply.
fn apply(target: &Target, edits: &[Edit]) -> Result<String, String> {
    let shown = target.resolved.display();
    let text = target.before.as_deref().unwrap_or("");
    match target.harness {
        Harness::Claude => apply_json(text, edits),
        Harness::Codex => apply_toml(text, edits),
    }
    .map_err(|reason| format!("{reason} in {shown}"))
}

/// Write each file's accepted edits. Every file is checked first, so a file that changed
/// while the person was answering stops the run before anything is written.
pub fn write(plan: &Plan) -> Result<Vec<Written>, ScanError> {
    let mut texts = vec![];
    for (target, edits) in &plan.files {
        let shown = target.resolved.display();
        match target.unchanged() {
            Ok(true) => {}
            Ok(false) => {
                return Err(ScanError::new(format!(
                    "{shown} changed while token-max doctor was running; nothing was written, so run it again"
                )));
            }
            Err(e) => {
                return Err(ScanError::new(format!(
                    "cannot read {shown} ({e}); nothing was written"
                )));
            }
        }
        texts.push(apply(target, edits).map_err(ScanError::new)?);
    }
    let mut written = vec![];
    for ((target, edits), text) in plan.files.iter().zip(texts) {
        if let Err(e) = target.write(&text) {
            let done: Vec<&str> = written.iter().map(|w: &Written| w.file.as_str()).collect();
            let done = if done.is_empty() {
                "nothing else was written".into()
            } else {
                format!("already wrote {}", done.join(", "))
            };
            return Err(ScanError::new(format!(
                "cannot write {} ({e}); {done}",
                target.resolved.display()
            )));
        }
        let resolved = target.resolved.display().to_string();
        written.push(Written {
            file: target.file.clone(),
            resolved: (resolved != target.file).then_some(resolved),
            edits: edits.len(),
        });
    }
    Ok(written)
}

/// What the walk did: the files written, then what is left to do by hand.
pub fn finish(plan: &Plan, written: &[Written], style: &Style) -> String {
    let home = style.home.as_deref();
    let shown = |path: &str| crate::report::home_path(home, path);
    let mut text = String::from("\n");
    for w in written {
        let landed = w.resolved.as_ref().map_or_else(String::new, |r| {
            format!(", through its symlink to {}", shown(r))
        });
        text += &style.line(
            &format!(
                "Wrote {} edit{} to {}{landed}.",
                w.edits,
                if w.edits == 1 { "" } else { "s" },
                shown(&w.file)
            ),
            0,
        );
    }
    if written.is_empty() && plan.declined.is_empty() && plan.manual.is_empty() {
        text += &style.line("Nothing to change on this host.", 0);
    } else if written.is_empty() {
        text += &style.line("No settings were changed.", 0);
    }
    if !plan.declined.is_empty() {
        text += "\n";
        text += &style.bold("Not applied; to make these yourself:", 0);
        for fix in &plan.declined {
            text += &style.fix(fix, "", 2);
        }
    }
    if !plan.manual.is_empty() {
        text += "\n";
        text += &style.bold("To change by hand:", 0);
        for fix in &plan.manual {
            text += &style.fix(fix, "", 2);
        }
    }
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
        let mut node = &mut *map;
        for (i, key) in parents.iter().enumerate() {
            let child = node
                .entry(key.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            node = child
                .as_object_mut()
                .ok_or_else(|| format!("{} is not an object", edit.path[..=i].join(".")))?;
        }
        if !edit.append {
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
        if !edit.append {
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
        Edit {
            path: path.split('.').map(String::from).collect(),
            value: value.into(),
            append,
        }
    }

    fn change(name: &str, file: &Path, harness: Harness, edits: Vec<Edit>) -> Fix {
        Fix {
            name: name.into(),
            value: "on -> off".into(),
            claim: "Measured",
            suggested: false,
            how: How::Edit(Patch {
                file: file.to_string_lossy().into_owned(),
                harness,
                edits,
            }),
        }
    }

    fn style(w: &Workspace) -> Style {
        Style {
            color: false,
            width: None,
            home: Some(w.home.to_string_lossy().into_owned()),
        }
    }

    /// Walk `fixes` answering with `answers`, returning the plan and what was printed.
    fn walk(fixes: Vec<Fix>, w: &Workspace, answers: &str) -> (Plan, String) {
        let mut printed = vec![];
        let plan = ask(fixes, &style(w), &mut answers.as_bytes(), &mut printed).unwrap();
        (plan, String::from_utf8(printed).unwrap())
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
        // Anything but an answer asks again; a quit leaves the rest unapplied.
        let (plan, printed) = walk(fixes, &w, "y\nmaybe\nn\nyes\nq\n");
        assert_eq!(printed.matches("Apply? [y]es, [n]o, [q]uit: ").count(), 5);
        assert!(printed.starts_with("\ntoken-max doctor: 5 changes to review."));
        let shown = format!("~{0}.claude{0}settings.json", std::path::MAIN_SEPARATOR);
        assert!(printed.contains(&format!(
            "\n[1/5] A: on -> off (Measured)\n  In {shown}:\n    \"a\": true\n"
        )));
        let written = write(&plan).unwrap();
        assert_eq!(
            std::fs::read_to_string(&settings).unwrap(),
            "{\n  \"keep\": 1,\n  \"a\": true,\n  \"c\": true\n}\n"
        );
        assert!(!config.exists());
        let summary = finish(&plan, &written, &style(&w));
        assert!(summary.starts_with(&format!("\nWrote 2 edits to {shown}.\n")));
        let not_applied = &summary[summary.find("Not applied").unwrap()..];
        for name in ["B:", "D:", "E:"] {
            assert!(not_applied.contains(name), "{name} in {summary}");
        }
        assert!(summary.contains(
            "To change by hand:\n  F: on -> off (Measured)\n    By hand, since the X environment variable sets it: set X=1 where it is exported.\n"
        ));
        // End of input is a quit, and nothing accepted writes nothing.
        let (plan, _) = walk(
            vec![change(
                "A",
                &settings,
                Harness::Claude,
                vec![edit("z", 1, false)],
            )],
            &w,
            "",
        );
        assert!(write(&plan).unwrap().is_empty());
        assert!(
            !std::fs::read_to_string(&settings)
                .unwrap()
                .contains("\"z\"")
        );
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
        let How::Manual { reason, .. } = &plan.manual[0].how else {
            panic!("an edit that cannot apply is a step by hand")
        };
        assert_eq!(
            reason,
            &format!("permissions is not an object in {}", settings.display())
        );
        std::fs::write(&settings, "{}").unwrap();
        let (plan, _) = walk(vec![deny], &w, "y\n");
        std::fs::write(&settings, r#"{"theirs": 1}"#).unwrap();
        let error = write(&plan).unwrap_err().message;
        assert!(
            error.contains("changed while token-max doctor was running"),
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
            "\nWrote 1 edit to ~/.claude/settings.json, through its symlink to ~/dotfiles/settings.json.\n"
        ));
        // Root can write anything, so only another user sees a read-only target.
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o444)).unwrap();
            let (plan, printed) = walk(vec![fix], &w, "");
            assert!(!printed.contains("Apply?"));
            let How::Manual { step, reason } = &plan.manual[0].how else {
                panic!("a read-only target is a step by hand")
            };
            assert_eq!(reason, &format!("{} is read-only", target.display()));
            assert_eq!(step, &format!("in {}, set \"a\": 1", settings.display()));
        }
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
        let How::Manual { reason, .. } = &plan.manual[0].how else {
            panic!("a dangling symlink is a step by hand")
        };
        assert!(
            reason.ends_with("is a symlink to a missing file"),
            "{reason}"
        );
    }
}
