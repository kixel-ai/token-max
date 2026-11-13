//! The repositories a session worked in, resolved on the host that ran it, for the session
//! export.

use std::fs;

use serde::{Deserialize, Serialize};

use crate::discovery::{absent, error, existing_directory, git_dir, join, project_directories};
use crate::safe::ScanError;

/// Where a session's repositories came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The transcript recorded it, as Codex does when a session starts.
    Recorded,
    /// Read now from the checkout at the session's directory, which may have changed since.
    Checkout,
}

/// A remote URL as it may be printed: without userinfo, so no credential leaves the host, and
/// None when it is empty or holds control characters.
pub fn url(value: &str) -> Option<String> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    let (prefix, rest) = match value.find("://") {
        Some(at) => value.split_at(at + 3),
        // Git reads `[user@]host:path` as SSH when a colon comes before any slash.
        None => match value.find([':', '/']) {
            Some(colon) if value[colon..].starts_with(':') => ("", value),
            _ => return Some(value.into()),
        },
    };
    let host_end = if prefix.is_empty() {
        rest.find(':').expect("checked above")
    } else {
        rest.find(['/', '?', '#']).unwrap_or(rest.len())
    };
    let url = match rest[..host_end].rfind('@') {
        Some(at) => format!("{prefix}{}", &rest[at + 1..]),
        None => value.into(),
    };
    (!url.is_empty()).then_some(url)
}

/// The remote URLs of the Git checkout containing `cwd`, in the order its config lists them.
///
/// None when `cwd` no longer exists, is in no checkout, or the checkout has no remote.
pub fn checkout(cwd: &str) -> Result<Option<Vec<String>>, ScanError> {
    if !existing_directory(cwd)? {
        return Ok(None);
    }
    let paths = project_directories(cwd, &[".git".into()])?;
    let root = paths.last().expect("cwd is included");
    let Some(git) = git_dir(root)? else {
        return Ok(None);
    };
    let file = join(&[&git.common, "config"]);
    let text = match fs::read(&file) {
        Ok(bytes) => bytes,
        // Git metadata that a removed main checkout left dangling names no remote.
        Err(e) if absent(e.kind()) => return Ok(None),
        Err(_) => return Err(error("cannot read git config", &file, 0)),
    };
    let text = String::from_utf8_lossy(&text);
    let urls = remotes(&text).map_err(|line| error("invalid git config", &file, line))?;
    let mut unique: Vec<String> = vec![];
    for found in urls.iter().filter_map(|u| url(u)) {
        if !unique.contains(&found) {
            unique.push(found);
        }
    }
    Ok((!unique.is_empty()).then_some(unique))
}

/// Every `remote.<name>.url` in one Git config file, or the 1-based line Git would reject.
///
/// Includes are not followed, and `url.<base>.insteadOf` rewrites are not applied.
fn remotes(text: &str) -> Result<Vec<String>, u64> {
    let mut urls = vec![];
    let mut remote = false;
    let mut line = 1;
    // Git skips a leading UTF-8 byte order mark, as some Windows editors write.
    let mut chars = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .chars()
        .peekable();
    while let Some(&c) = chars.peek() {
        match c {
            '\n' => {
                chars.next();
                line += 1;
            }
            c if c.is_whitespace() => {
                chars.next();
            }
            '#' | ';' => skip_line(&mut chars, &mut line),
            '[' => {
                chars.next();
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    if !(c.is_ascii_alphanumeric() || c == '-' || c == '.') {
                        break;
                    }
                    name.push(c);
                    chars.next();
                }
                let mut subsection = false;
                if chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
                    while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
                    if chars.next() != Some('"') {
                        return Err(line);
                    }
                    loop {
                        match chars.next() {
                            Some('"') => break,
                            Some('\\') => {
                                chars.next().filter(|c| *c != '\n').ok_or(line)?;
                            }
                            Some('\n') | None => return Err(line),
                            Some(_) => {}
                        }
                    }
                    subsection = true;
                }
                if chars.next() != Some(']') || name.is_empty() {
                    return Err(line);
                }
                // `[remote "origin"]`, or the deprecated `[remote.origin]`.
                let (section, dotted) = match name.split_once('.') {
                    Some((section, sub)) => (section, !sub.is_empty()),
                    None => (&*name, false),
                };
                remote = section.eq_ignore_ascii_case("remote") && (subsection || dotted);
            }
            c if c.is_ascii_alphabetic() => {
                let mut key = String::new();
                while let Some(c) = chars.next_if(|c| c.is_ascii_alphanumeric() || *c == '-') {
                    key.push(c);
                }
                while chars.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
                let value = match chars.peek() {
                    Some('=') => {
                        chars.next();
                        Some(value(&mut chars, &mut line)?)
                    }
                    // A key alone is a boolean true.
                    None | Some('\n' | '\r' | '#' | ';') => {
                        skip_line(&mut chars, &mut line);
                        None
                    }
                    Some(_) => return Err(line),
                };
                if remote
                    && key.eq_ignore_ascii_case("url")
                    && let Some(value) = value
                {
                    urls.push(value);
                }
            }
            _ => return Err(line),
        }
    }
    Ok(urls)
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// Skip the rest of a comment's or a value-less key's line.
fn skip_line(chars: &mut Chars, line: &mut u64) {
    for c in chars.by_ref() {
        if c == '\n' {
            *line += 1;
            break;
        }
    }
}

/// One value after its `=`, through the end of its last continued line, as Git reads it.
fn value(chars: &mut Chars, line: &mut u64) -> Result<String, u64> {
    let mut value = String::new();
    // Unquoted whitespace, kept only when more of the value follows it.
    let mut space = String::new();
    let mut quoted = false;
    loop {
        let Some(c) = chars.next() else {
            return if quoted { Err(*line) } else { Ok(value) };
        };
        match c {
            '\n' if quoted => return Err(*line),
            '\n' => {
                *line += 1;
                return Ok(value);
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '#' | ';' if !quoted => {
                skip_line(chars, line);
                return Ok(value);
            }
            ' ' | '\t' if !quoted => {
                if !value.is_empty() {
                    space.push(c);
                }
            }
            '"' => {
                value.push_str(&std::mem::take(&mut space));
                quoted = !quoted;
            }
            '\\' => {
                let escaped = match chars.next() {
                    Some('\n') => {
                        *line += 1;
                        continue;
                    }
                    Some('\r') if chars.next_if_eq(&'\n').is_some() => {
                        *line += 1;
                        continue;
                    }
                    Some('n') => '\n',
                    Some('t') => '\t',
                    Some('b') => '\u{8}',
                    Some(c @ ('"' | '\\')) => c,
                    _ => return Err(*line),
                };
                value.push_str(&std::mem::take(&mut space));
                value.push(escaped);
            }
            c => {
                value.push_str(&std::mem::take(&mut space));
                value.push(c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Workspace;

    #[test]
    fn urls_lose_their_userinfo_and_keep_everything_else() {
        for (given, printed) in [
            (
                "https://github.com/o/r.git",
                Some("https://github.com/o/r.git"),
            ),
            (
                "https://user:token@github.com/o/r.git",
                Some("https://github.com/o/r.git"),
            ),
            (
                "https://a@b:c@host:8443/p@q?x=@#@",
                Some("https://host:8443/p@q?x=@#@"),
            ),
            ("ssh://git@host/o/r", Some("ssh://host/o/r")),
            ("git@github.com:o/r.git", Some("github.com:o/r.git")),
            ("host:o/r", Some("host:o/r")),
            ("/srv/a@b:c/r.git", Some("/srv/a@b:c/r.git")),
            ("../sibling", Some("../sibling")),
            ("file:///srv/r", Some("file:///srv/r")),
            ("", None),
            ("/srv/my repo", Some("/srv/my repo")),
            ("https://host/\u{1b}[0m", None),
        ] {
            assert_eq!(url(given).as_deref(), printed, "{given}");
        }
    }

    #[test]
    fn remotes_reads_every_remote_url_as_git_parses_it() {
        let config = "[core]\n\tbare = false\n\turl = not-a-remote\n\
            [remote \"origin\"]\n\tURL = https://github.com/o/r.git ; comment\n\
            \tfetch = +refs/heads/*:refs/remotes/origin/*\n\
            [Remote \"up \\\"q\\\"\"] url = \"git@host:o/\"r.git # trailing\n\
            [remote.old]\n\turl = a\\\n b\n\
            [branch \"main\"]\n\tremote = origin\n\turl = not-a-remote\n\
            [remote \"bool\"]\n\turl\n";
        assert_eq!(
            remotes(config),
            Ok(vec![
                "https://github.com/o/r.git".into(),
                "git@host:o/r.git".into(),
                "a b".into(),
            ])
        );
        assert_eq!(
            remotes("\u{feff}[remote \"o\"]\nurl = x\n"),
            Ok(vec!["x".into()])
        );
        for (config, line) in [
            // Only a leading byte order mark is skipped.
            ("\n\u{feff}[core]\n", 2),
            ("[remote \"origin\"\n", 1),
            ("\n[remote \"o]\n", 2),
            ("[core]\n\n  = x\n", 3),
            ("[remote \"o\"]\nurl = \"open\n", 2),
            ("[remote \"o\"]\nurl = bad\\escape\n", 2),
            ("[remote \"o\"]\nurl x\n", 2),
        ] {
            assert_eq!(remotes(config), Err(line), "{config:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_checkout_resolves_from_subdirectories_and_linked_worktrees() {
        let w = Workspace::new();
        let text = |p: &std::path::Path| p.to_str().unwrap().to_string();
        let main = w.home.join("main");
        std::fs::create_dir_all(main.join("src/deep")).unwrap();
        std::fs::create_dir_all(main.join(".git/worktrees/branch")).unwrap();
        std::fs::write(
            main.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://u:secret@host/o/r.git\n\
             [remote \"fork\"]\n\turl = git@host:me/r.git\n\
             [remote \"mirror\"]\n\turl = https://host/o/r.git\n",
        )
        .unwrap();
        let work = w.home.join("worktree");
        std::fs::create_dir_all(work.join("sub")).unwrap();
        std::fs::write(
            work.join(".git"),
            format!("gitdir: {}\n", text(&main.join(".git/worktrees/branch"))),
        )
        .unwrap();
        std::fs::write(main.join(".git/worktrees/branch/commondir"), "../..\n").unwrap();
        let expected = Some(vec![
            "https://host/o/r.git".to_string(),
            "host:me/r.git".to_string(),
        ]);
        for cwd in [&main, &main.join("src/deep"), &work.join("sub")] {
            assert_eq!(checkout(&text(cwd)).unwrap(), expected, "{cwd:?}");
        }

        // A submodule's own directory, a checkout without remotes, outside any checkout, gone.
        let module = w.home.join("super/.git/modules/sub");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(module.join("config"), "[remote \"origin\"]\nurl = ../sub\n").unwrap();
        let sub = w.home.join("super/sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".git"), "gitdir: ../.git/modules/sub\n").unwrap();
        assert_eq!(
            checkout(&text(&sub)).unwrap(),
            Some(vec!["../sub".to_string()])
        );
        let bare = w.home.join("bare");
        std::fs::create_dir_all(bare.join(".git")).unwrap();
        std::fs::write(bare.join(".git/config"), "[core]\nbare = false\n").unwrap();
        assert_eq!(checkout(&text(&bare)).unwrap(), None);
        let outside = w.home.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        assert_eq!(checkout(&text(&outside)).unwrap(), None);
        assert_eq!(checkout(&text(&main.join("gone"))).unwrap(), None);
        assert_eq!(checkout("relative").unwrap(), None);

        std::fs::write(main.join(".git/config"), "[remote \"o\"\n").unwrap();
        let error = checkout(&text(&main)).unwrap_err();
        assert_eq!(error.message, "invalid git config");
        assert_eq!(error.location.unwrap().line, 1);
    }
}
