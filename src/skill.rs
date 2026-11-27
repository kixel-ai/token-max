//! The `token-audit` agent skill: its prompt, installing its stub where harnesses look, and
//! `vibes`' card for the profiles that lack it.

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use crate::doctor::How;
use crate::model::{Harness, Profile};
use crate::safe::{ScanError, display};
use crate::vibes::Item;

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
    // A checkout's own files can link a project's skills anywhere, so its install stays inside.
    let inside = match &options.project {
        Some(cwd) => Some(fs::canonicalize(cwd).map_err(|_| {
            ScanError::new(format!("cannot read {}", display(&cwd.to_string_lossy())))
        })?),
        None => None,
    };
    let mut targets: Vec<Target> = vec![];
    for root in roots(options)? {
        let path = root.join("skills").join("token-audit").join("SKILL.md");
        let shown = display(&path.to_string_lossy());
        let resolved =
            resolve(&path).map_err(|_| ScanError::new(format!("cannot read {shown}")))?;
        if inside
            .as_ref()
            .is_some_and(|dir| !resolved.starts_with(dir))
        {
            return Err(ScanError::new(format!(
                "{shown} links outside the current directory, so --project leaves it alone"
            )));
        }
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
    // Refuse before writing anything, so a refusal leaves every file as it was. A write
    // replaces a linked SKILL.md's target, which can be any file, or a dangling link itself,
    // so --force never does.
    for (target, state) in targets.iter().zip(&states) {
        let Ok(link) = fs::read_link(&target.path) else {
            continue;
        };
        if !matches!(state, State::Current) {
            return Err(ScanError::new(format!(
                "{} is a link to {}, which is not this token-max's token-audit skill; replace the link with a copy of {SOURCE} or remove it",
                display(&target.path.to_string_lossy()),
                display(&link.to_string_lossy())
            )));
        }
    }
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

/// Whether a profile's harness finds the skill, read when its profile is discovered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Found {
    /// A `token-audit/SKILL.md` is where the harness reads user skills, this build's or not.
    pub installed: bool,
    /// `token-max skill install` without `--claude-dir` writes this profile's copy.
    pub default: bool,
}

/// Whether the profile rooted at `root` finds the skill: Claude Code in its home's `skills`,
/// Codex in `~/.agents/skills` or its deprecated `$CODEX_HOME/skills`. `active` is whether
/// it is the Claude Code home the shell selects, which for a T3 instance is its own. Only
/// metadata is read, through any link.
pub fn found(harness: Harness, root: &str, home: &str, active: bool) -> Found {
    let at = |dir: &Path| {
        dir.join("skills")
            .join("token-audit")
            .join("SKILL.md")
            .is_file()
    };
    match harness {
        Harness::Claude => Found {
            installed: at(Path::new(root)),
            default: active,
        },
        Harness::Codex => Found {
            installed: at(&Path::new(home).join(".agents")) || at(Path::new(root)),
            // Codex's copy goes in ~/.agents whatever its home, which a bare install writes
            // when it or ~/.codex exists.
            default: Path::new(home).join(".agents").is_dir()
                || Path::new(home).join(".codex").is_dir(),
        },
    }
}

const WHAT: &str = "An agent skill for Claude Code and Codex that audits their transcripts for token waste beyond settings.";

const PICK: &str = "The maintainer's pick, from token-max's own maintainer: https://github.com/kixel-ai/token-max/blob/main/docs/reference.md#token-audit-skill";

/// `vibes`' card for the skill on one host: quiet when every profile finds it, else the
/// install step for those that don't. None when no profile was looked at.
pub fn item(profiles: &[Profile], home: Option<&str>) -> Option<Item> {
    let shown = |path: &str| crate::report::home_path(home, path);
    let mut seen: Vec<(&Profile, Found)> = vec![];
    for p in profiles {
        // A disabled T3 instance starts no session to use it.
        let Some(found) = p.skill.filter(|_| p.t3.as_ref().is_none_or(|t| t.enabled)) else {
            continue;
        };
        if !seen
            .iter()
            .any(|(s, _)| s.harness == p.harness && s.root == p.root)
        {
            seen.push((p, found));
        }
    }
    if seen.is_empty() {
        return None;
    }
    let missing: Vec<&(&Profile, Found)> = seen.iter().filter(|(_, f)| !f.installed).collect();
    let card = |text: String, how: Option<How>| Item {
        name: "Token audit skill".into(),
        value: String::new(),
        what: WHAT.into(),
        text,
        sources: vec![PICK.into()],
        quiet: how.is_none(),
        how,
        after: vec![],
        defers: None,
    };
    if missing.is_empty() {
        return Some(card(
            "Installed for every Claude Code and Codex profile here: ask for /token-audit in Claude Code or $token-audit in Codex.".into(),
            None,
        ));
    }
    let names: Vec<String> = missing
        .iter()
        .map(|(p, _)| match p.harness {
            Harness::Claude => format!("Claude Code in {}", shown(&p.root)),
            Harness::Codex => format!("Codex in {}", shown(&p.root)),
        })
        .collect();
    // A T3 instance's own environment selects its home, which the shell may not.
    let bare =
        |p: &Profile, f: &Found| f.default && (p.harness == Harness::Codex || p.t3.is_none());
    let named: String = missing
        .iter()
        .filter(|(p, f)| p.harness == Harness::Claude && !bare(p, f))
        .map(|(p, _)| format!(" --claude-dir {}", arg(&p.root, shown(&p.root))))
        .collect();
    let mut steps = vec![];
    if !named.is_empty() {
        steps.push(format!("`token-max skill install{named}`"));
    }
    // With neither ~/.agents nor ~/.codex, a bare install has nowhere to put Codex's copy.
    let agents = missing
        .iter()
        .any(|(p, f)| p.harness == Harness::Codex && !f.default);
    // A bare install writes the shell's Claude Code home and ~/.agents, which Codex reads.
    if agents || missing.iter().any(|(p, f)| bare(p, f)) {
        steps.push("`token-max skill install`".to_string());
    }
    let create = match (agents, home) {
        (false, _) => String::new(),
        (true, Some(dir)) => format!(
            "create {}, where Codex reads skills, then ",
            shown(&Path::new(dir).join(".agents").to_string_lossy())
        ),
        (true, None) => "create .agents in your home, where Codex reads skills, then ".into(),
    };
    let text = format!(
        "Not installed for {}. /token-audit has an agent run token-max audit and rank by dollars the waste no setting changes: commands that bust the prompt cache, bloated or repeated tool calls, always-loaded context the sessions never needed, and context they kept rediscovering. It offers each fix and makes it only after a yes. What it saves is not measured.",
        crate::lsp::list(&names)
    );
    // npx's global install reaches only the default homes, not a named Claude Code one.
    let npx = if named.is_empty() {
        ", or `npx skills add kixel-ai/token-max --skill token-audit -g`"
    } else {
        ""
    };
    Some(card(
        text,
        Some(How::Manual {
            step: format!(
                "{create}run {}{npx}, then start a new session",
                steps.join(" and ")
            ),
            reason: "doctor and vibes write only settings files, not skills".into(),
        }),
    ))
}

/// `path` as one shell argument: `shown` when it needs no quoting, else the whole path in
/// single quotes, since a quoted `~` doesn't expand.
fn arg(path: &str, shown: String) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "/\\._-:@+,=".contains(c);
    if path.chars().all(plain) {
        shown
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Workspace, make_profile, make_t3_profile};

    fn put(dir: &Path) {
        let dir = dir.join("skills/token-audit");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "synthetic\n").unwrap();
    }

    #[test]
    fn each_harness_finds_the_skill_where_it_reads_skills() {
        let w = Workspace::new();
        let text = |p: &Path| p.to_string_lossy().into_owned();
        let (home, claude, codex) = (text(&w.home), text(&w.claude), text(&w.codex));
        let installed = |h, root: &str| found(h, root, &home, true).installed;
        assert!(!installed(Harness::Claude, &claude) && !installed(Harness::Codex, &codex));
        // ~/.agents is Codex's, never Claude Code's.
        put(&w.home.join(".agents"));
        assert!(!installed(Harness::Claude, &claude) && installed(Harness::Codex, &codex));
        fs::remove_dir_all(w.home.join(".agents")).unwrap();
        // Codex still reads its deprecated home copy.
        put(&w.codex);
        assert!(installed(Harness::Codex, &codex));
        // A link to a SKILL.md counts; a dangling one doesn't.
        #[cfg(unix)]
        {
            let dir = w.claude.join("skills/token-audit");
            fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(w.home.join("elsewhere"), dir.join("SKILL.md")).unwrap();
            assert!(!installed(Harness::Claude, &claude));
            fs::write(w.home.join("elsewhere"), "synthetic\n").unwrap();
            assert!(installed(Harness::Claude, &claude));
        }
        // Codex's copy is the bare install's, whichever home it reads, while ~/.codex or
        // ~/.agents exists for it to write.
        assert!(found(Harness::Codex, &codex, &home, false).default);
        assert!(!found(Harness::Claude, &claude, &home, false).default);
        fs::remove_dir_all(&w.codex).unwrap();
        assert!(!found(Harness::Codex, &codex, &home, true).default);
        fs::create_dir(w.home.join(".agents")).unwrap();
        assert!(found(Harness::Codex, &codex, &home, true).default);
    }

    #[test]
    fn the_card_names_each_profile_missing_it_and_the_install_that_reaches_it() {
        let profile = |id: &str, harness, root: &str, installed, default| {
            let mut p = make_profile(id, root);
            p.harness = harness;
            p.skill = Some(Found { installed, default });
            p
        };
        let home = Some("/home/me");
        // No profile looked: no card, as under --defaults.
        assert_eq!(item(&[make_profile("a", "/home/me/.claude")], home), None);
        let claude = profile("a", Harness::Claude, "/home/me/.claude", true, true);
        let codex = profile("b", Harness::Codex, "/home/me/.codex", true, true);
        let card = item(&[claude.clone(), codex.clone()], home).unwrap();
        assert!(card.quiet && card.how.is_none(), "{card:?}");
        let step = |card: Item| match card.how {
            Some(How::Manual { step, .. }) => step,
            other => panic!("{other:?}"),
        };
        // One harness missing it: the bare install, which writes ~/.agents for Codex.
        let missing = Found {
            installed: false,
            default: true,
        };
        let partial = item(
            &[
                claude.clone(),
                Profile {
                    skill: Some(missing),
                    ..codex.clone()
                },
            ],
            home,
        )
        .unwrap();
        assert!(!partial.quiet);
        let codex_only = "Not installed for Codex in ~/.codex. "; // slash ok: roots pass through unchanged
        assert!(partial.text.starts_with(codex_only));
        assert!(step(partial).starts_with("run `token-max skill install`, or `npx "));
        // A Claude home the shell doesn't select gets --claude-dir, beside the bare install.
        let work = profile("c", Harness::Claude, "/home/me/.claude-work", false, false);
        let both = item(
            &[
                claude.clone(),
                work.clone(),
                Profile {
                    skill: Some(missing),
                    ..codex
                },
            ],
            home,
        )
        .unwrap();
        let text = "Not installed for Claude Code in ~/.claude-work and Codex in ~/.codex. "; // slash ok: roots pass through unchanged
        assert!(both.text.starts_with(text), "{}", both.text);
        let named = "run `token-max skill install --claude-dir ~/.claude-work` and `token-max skill install`, then start a new session"; // slash ok: roots pass through unchanged
        assert_eq!(step(both), named);
        // A disabled T3 instance starts no session, so it is not counted; aliases share a card.
        let mut t3 = make_t3_profile("t", "/state/claude");
        t3.skill = Some(Found {
            installed: false,
            default: false,
        });
        t3.t3.as_mut().unwrap().enabled = false;
        let card = item(&[claude.clone(), claude.clone(), t3.clone()], home).unwrap();
        assert!(card.quiet);
        // An enabled one reads its own home, which its environment selects but the shell
        // may not, so it is named; so is a path that needs quoting, whole.
        t3.t3.as_mut().unwrap().enabled = true;
        t3.skill = Some(missing);
        let spaced = profile("d", Harness::Claude, "/home/me/my claude", false, false);
        let card = item(&[claude.clone(), t3, spaced], home).unwrap();
        let named = "run `token-max skill install --claude-dir /state/claude --claude-dir '/home/me/my claude'`, then start a new session"; // slash ok: roots pass through unchanged
        assert_eq!(step(card), named);
        // A Codex home whose bare install has no ~/.agents or ~/.codex to write creates it.
        let alone = profile("e", Harness::Codex, "/srv/codex", false, false);
        let card = item(&[claude, alone], home).unwrap();
        let create = format!(
            "create {}, where Codex reads skills, then run `token-max skill install`, or `npx ",
            crate::test_support::native_path("~/.agents")
        );
        assert!(step(card).starts_with(&create));
    }
}
