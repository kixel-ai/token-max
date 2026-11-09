//! Historical Codex skill paths, resolved lexically without reading installed files.

use std::collections::HashMap;

/// Resolve a rendered catalog's root aliases and entry file suffixes.
pub(crate) fn paths<'a>(body: &'a str, entries: &[(&str, &'a str)]) -> Vec<Option<String>> {
    let mut roots: HashMap<&str, Option<&str>> = HashMap::new();
    for line in body.lines() {
        let Some((alias, root)) = line.strip_prefix("- `").and_then(|s| s.split_once("` = `"))
        else {
            continue;
        };
        let Some(root) = root.strip_suffix('`').filter(|p| absolute(p)) else {
            continue;
        };
        roots
            .entry(alias)
            .and_modify(|r| {
                if *r != Some(root) {
                    *r = None;
                }
            })
            .or_insert(Some(root));
    }
    entries
        .iter()
        .map(|(_, span)| {
            let (_, suffix) = span.rsplit_once("(file: ")?;
            let path = suffix.trim_end_matches('\r').strip_suffix(')')?;
            if absolute(path) && skill_file(path) && plain(path) {
                return Some(normalize(path));
            }
            let (alias, rest) = path.split_once('/')?;
            // Root aliases are a rendered transport convention, not filesystem paths.
            let root = roots.get(alias).copied().flatten()?;
            if !skill_file(rest) || !plain(rest) {
                return None;
            }
            if !plain(root) {
                return None;
            }
            let root = if windows(root) {
                root.trim_end_matches(['/', '\\'])
            } else {
                root.trim_end_matches('/')
            };
            Some(normalize(&format!("{root}/{rest}")))
        })
        .collect()
}

/// Windows roots and read events must hash the same lexical separator form.
pub(crate) fn normalize(path: &str) -> String {
    if windows(path) {
        let mut normalized = path.replace('/', "\\");
        if normalized.as_bytes().get(1) == Some(&b':') {
            normalized.replace_range(..1, &normalized[..1].to_ascii_uppercase());
        }
        normalized
    } else {
        path.into()
    }
}

fn windows(path: &str) -> bool {
    path.starts_with("\\\\") || path.as_bytes().get(1) == Some(&b':')
}

// `..` can cross a symlink. Historical transcripts cannot prove its resolved identity.
fn plain(path: &str) -> bool {
    let separators: &[char] = if windows(path) { &['/', '\\'] } else { &['/'] };
    let body = path
        .strip_prefix("\\\\")
        .or_else(|| path.strip_prefix('/'))
        .unwrap_or(path)
        .trim_end_matches(separators);
    !body
        .split(separators)
        .any(|part| part == "." || part == ".." || part.is_empty())
        || path == "/"
}

pub(crate) fn absolute(path: &str) -> bool {
    path.starts_with('/')
        || path.starts_with("\\\\")
        || (path.as_bytes().get(1) == Some(&b':')
            && path.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && path
                .as_bytes()
                .get(2)
                .is_some_and(|c| *c == b'/' || *c == b'\\'))
}

pub(crate) fn skill_file(path: &str) -> bool {
    path.ends_with("/SKILL.md") || path.ends_with("\\SKILL.md")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_paths_spaces_and_conflicts_do_not_require_a_live_filesystem() {
        let body = "- `r0` = `/remote/root with spaces`\n- `r1` = `/first`\n- `r1` = `/conflict`";
        let entries = [
            (
                "plugin:skill",
                "- plugin:skill: description (file: r0/name/SKILL.md)",
            ),
            ("win", r"- win: x (file: C:\skills\quoted\SKILL.md)"),
            ("abs", "- abs: x (file: /remote/a (b)/SKILL.md)\r"),
            ("unknown", "- unknown: x (file: r9/name/SKILL.md)"),
            ("conflict", "- conflict: x (file: r1/name/SKILL.md)"),
            ("traversal", "- traversal: x (file: r0/../name/SKILL.md)"),
            ("missing", "- missing: x"),
        ];
        assert_eq!(
            paths(body, &entries),
            vec![
                Some("/remote/root with spaces/name/SKILL.md".into()),
                Some(r"C:\skills\quoted\SKILL.md".into()),
                Some("/remote/a (b)/SKILL.md".into()),
                None,
                None,
                None,
                None,
            ]
        );
    }
}
