//! The `token-audit` agent skill: its prompt, and installing its stub where harnesses look.

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use crate::safe::{ScanError, display};

/// The skill's `SKILL.md`, which only runs `token-max skill audit`, so installs never go stale.
pub const STUB: &str = include_str!("../skills/token-audit/SKILL.md");

/// The stub's published copy, for comparing with one that differs.
const SOURCE: &str = "https://github.com/kixel-ai/token-max/blob/main/skills/token-audit/SKILL.md";

/// What `token-max skill audit` prints: the audit's instructions.
pub const PROMPT: &str = include_str!("skill/audit.md");

/// What `token-max skill filter` prints: a `jq` program that keeps a transcript line's
/// structure and drops its text, for the agent to read flagged lines through.
pub const FILTER: &str = include_str!("skill/slice.jq");

/// Where to install the stub.
#[derive(Debug, Default)]
pub struct Install {
    /// Claude Code homes; when any is named, only the named ones.
    pub claude_dirs: Vec<PathBuf>,
    /// The current directory's `.agents` and `.claude` skills instead of the user's.
    pub project: Option<PathBuf>,
    /// Replace a `SKILL.md` that differs from this build's.
    pub force: bool,
    /// The user's home, holding the default `.claude`, `.codex` and `.agents`.
    pub home: Option<PathBuf>,
    /// `CLAUDE_CONFIG_DIR`, which moves Claude Code's home.
    pub claude_env: Option<PathBuf>,
    /// `CODEX_HOME`, which moves Codex's home.
    pub codex_env: Option<PathBuf>,
}

/// One skills directory to hold `token-audit/SKILL.md`.
struct Target {
    /// The `SKILL.md` path as the person knows it.
    path: PathBuf,
    /// The same path with every existing ancestor's symlinks resolved, to write each file once.
    resolved: PathBuf,
}

/// What happened to one target.
enum State {
    Current,
    Missing,
    Different,
}

/// Write the stub into every selected skills directory, returning what each line did.
pub fn install(options: &Install) -> Result<String, ScanError> {
    let mut targets: Vec<Target> = vec![];
    for root in roots(options)? {
        let path = root.join("skills").join("token-audit").join("SKILL.md");
        let resolved = resolve(&path).map_err(|_| {
            ScanError::new(format!("cannot read {}", display(&path.to_string_lossy())))
        })?;
        if !targets.iter().any(|t| t.resolved == resolved) {
            targets.push(Target { path, resolved });
        }
    }
    let mut states = vec![];
    for target in &targets {
        states.push(match fs::read(&target.resolved) {
            Ok(bytes) if bytes == STUB.as_bytes() => State::Current,
            Ok(_) => State::Different,
            Err(e) if e.kind() == io::ErrorKind::NotFound => State::Missing,
            Err(_) => return Err(cannot("read", &target.path)),
        });
    }
    // Refuse before writing anything, so a refusal leaves every file as it was.
    let refused: Vec<String> = targets
        .iter()
        .zip(&states)
        .filter(|(_, s)| matches!(s, State::Different) && !options.force)
        .map(|(t, _)| display(&t.path.to_string_lossy()))
        .collect();
    if !refused.is_empty() {
        return Err(ScanError::new(format!(
            "{} differs from this token-max's token-audit skill, so it may hold your edits; compare it with {SOURCE}, then rerun with --force to replace it",
            refused.join(", ")
        )));
    }
    let mut out = String::new();
    for (target, state) in targets.iter().zip(&states) {
        let shown = display(&target.path.to_string_lossy());
        if matches!(state, State::Current) {
            out.push_str(&format!("up to date: {shown}\n"));
            continue;
        }
        write(&target.resolved).map_err(|_| cannot("write", &target.path))?;
        out.push_str(&format!("installed: {shown}\n"));
    }
    Ok(out)
}

/// The directories whose `skills` directory gets the stub.
fn roots(options: &Install) -> Result<Vec<PathBuf>, ScanError> {
    if let Some(cwd) = &options.project {
        let mut roots = vec![cwd.join(".agents")];
        if cwd.join(".claude").is_dir() {
            roots.push(cwd.join(".claude"));
        }
        return Ok(roots);
    }
    if !options.claude_dirs.is_empty() {
        for dir in &options.claude_dirs {
            if !dir.is_dir() {
                return Err(ScanError::new(format!(
                    "{} is not a directory",
                    display(&dir.to_string_lossy())
                )));
            }
        }
        return Ok(options.claude_dirs.clone());
    }
    let home = options.home.as_ref();
    let claude = options
        .claude_env
        .clone()
        .or_else(|| home.map(|h| h.join(".claude")))
        .filter(|dir| dir.is_dir());
    let codex = options
        .codex_env
        .clone()
        .or_else(|| home.map(|h| h.join(".codex")))
        .filter(|dir| dir.is_dir());
    // Codex reads user skills from ~/.agents/skills for every home; its `$CODEX_HOME/skills`
    // is deprecated, and a copy in both would list the skill twice.
    let agents = home
        .map(|h| h.join(".agents"))
        .filter(|dir| codex.is_some() || dir.is_dir());
    let found: Vec<PathBuf> = [claude, agents].into_iter().flatten().collect();
    if found.is_empty() {
        return Err(ScanError::new(
            "found no Claude Code, Codex or ~/.agents directory; use --claude-dir or --project",
        ));
    }
    Ok(found)
}

/// `path` with its deepest existing ancestor's symlinks resolved and the rest appended.
fn resolve(path: &Path) -> io::Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let mut rest = vec![];
    let mut existing = path.as_path();
    loop {
        match fs::canonicalize(existing) {
            Ok(mut resolved) => {
                resolved.extend(rest.iter().rev());
                return Ok(resolved);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let Some(parent) = existing.parent() else {
                    return Err(e);
                };
                match existing.components().next_back() {
                    Some(Component::Normal(name)) => rest.push(name.to_owned()),
                    _ => return Err(e),
                }
                existing = parent;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Replace `path` with the stub atomically, creating its directories.
fn write(path: &Path) -> io::Result<()> {
    let dir = path.parent().expect("SKILL.md has a directory");
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".SKILL.md.token-max-{}", std::process::id()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(STUB.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// A failed read or write of `path`.
fn cannot(verb: &str, path: &Path) -> ScanError {
    ScanError::new(format!(
        "cannot {verb} {}; nothing after it was installed",
        display(&path.to_string_lossy())
    ))
}
