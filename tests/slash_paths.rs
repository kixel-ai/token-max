//! Fails a test that expects a path with a hard-coded `/` where the code under test may build it
//! with the platform's separator, a mistake that otherwise surfaces only in the Windows CI job.
//!
//! It reads the Rust sources and flags, in test code that compiles on Windows, a string literal
//! holding a relative or `~/` path that is either an assertion's expected value or bound by `let`
//! and later asserted, and a path joined from such a literal that is asserted as a string.
//! Absolute literals such as `/synthetic/settings.json` and `{ROOT}/...` pass: no real Windows
//! path starts with `/`, so a test can only get one back when the code passed it through.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const OPT_OUT: &str = "// slash ok:";

#[test]
fn no_test_expects_a_slash_separated_path() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in ["src", "tests"] {
        collect(&root.join(dir), &mut files);
    }
    let mut sources = HashMap::new();
    for file in &files {
        let source = std::fs::read_to_string(file).unwrap();
        sources.insert(file.clone(), lex(&source));
    }
    let modules = modules(root, &sources);

    let mut found = Vec::new();
    for file in &files {
        let module = modules.get(file).copied().unwrap_or(Module {
            test: file.starts_with(root.join("tests")),
            windows: true,
        });
        if !module.windows {
            continue;
        }
        for hit in scan(&sources[file], module.test) {
            let shown = file.strip_prefix(root).unwrap().display();
            found.push(format!("{shown}:{}: {:?}", hit.line, hit.path));
        }
    }
    assert!(
        found.is_empty(),
        // slash ok: the message's examples
        "These tests expect a path with a hard-coded `/` that the code may build with the \
         platform's separator, which fails only on Windows:\n  {}\n\
         Build the expected path with `native_path`, from `crate::test_support` in a unit test or \
         `support` in tests/, such as `native_path(\"repo/.codex/config.toml\")` or \
         `format!(\"in {{}}\", native_path(\"~/.claude/settings.json\"))`. If the code passes the \
         path through unchanged or it is not a file path, end its line or the line above with \
         `{OPT_OUT} <why>`.",
        found.join("\n  ")
    );
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, files);
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    files.sort();
}

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Str(String),
    Ident(String),
    Punct(char),
    Comment(String),
}

#[derive(Clone, Debug)]
struct Token {
    kind: Kind,
    line: usize,
}

impl Token {
    fn is(&self, c: char) -> bool {
        self.kind == Kind::Punct(c)
    }

    fn ident(&self) -> Option<&str> {
        match &self.kind {
            Kind::Ident(s) => Some(s),
            _ => None,
        }
    }
}

/// Rust source as strings, identifiers, punctuation and line comments; enough to find literals.
fn lex(source: &str) -> Vec<Token> {
    let s: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let (mut i, mut line) = (0, 1);
    let word = |i: usize| i > 0 && (s[i - 1].is_alphanumeric() || s[i - 1] == '_');
    while i < s.len() {
        let c = s[i];
        let next = s.get(i + 1).copied();
        if c == '\n' {
            line += 1;
            i += 1;
        } else if c.is_whitespace() {
            i += 1;
        } else if c == '/' && next == Some('/') {
            let end = s[i..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(s.len(), |n| i + n);
            let text = s[i..end].iter().collect();
            tokens.push(Token {
                kind: Kind::Comment(text),
                line,
            });
            i = end;
        } else if c == '/' && next == Some('*') {
            let mut depth = 0;
            loop {
                if s[i] == '/' && s.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if s[i] == '*' && s.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    line += usize::from(s[i] == '\n');
                    i += 1;
                }
                if depth == 0 {
                    break;
                }
            }
        } else if let Some((start, hashes)) = raw_string(&s, i).filter(|_| !word(i)) {
            let close: Vec<char> = std::iter::once('"')
                .chain("#".repeat(hashes).chars())
                .collect();
            let end = (start..).find(|&j| s[j..].starts_with(&close)).unwrap();
            let text: String = s[start..end].iter().collect();
            tokens.push(Token {
                kind: Kind::Str(text.clone()),
                line,
            });
            line += text.matches('\n').count();
            i = end + close.len();
        } else if c == '"' || (c == 'b' && next == Some('"') && !word(i)) {
            let mut j = i + if c == 'b' { 2 } else { 1 };
            let mut text = String::new();
            while s[j] != '"' {
                let n = if s[j] == '\\' { 2 } else { 1 };
                text.extend(&s[j..j + n]);
                j += n;
            }
            tokens.push(Token {
                kind: Kind::Str(text.clone()),
                line,
            });
            line += text.matches('\n').count();
            i = j + 1;
        } else if c == '\'' {
            // A char literal, skipped so a quote in it opens no string; else a lifetime.
            i += match (next, s.get(i + 2)) {
                (Some('\\'), _) => 3 + s[i + 3..].iter().position(|&c| c == '\'').unwrap() + 1,
                (_, Some('\'')) => 3,
                _ => 1,
            };
        } else if c.is_alphanumeric() || c == '_' {
            let end = (i..s.len()).find(|&j| !(s[j].is_alphanumeric() || s[j] == '_'));
            let end = end.unwrap_or(s.len());
            tokens.push(Token {
                kind: Kind::Ident(s[i..end].iter().collect()),
                line,
            });
            i = end;
        } else {
            tokens.push(Token {
                kind: Kind::Punct(c),
                line,
            });
            i += 1;
        }
    }
    tokens
}

/// Where a raw string's text starts and how many `#` close it, if one starts at `i`.
fn raw_string(s: &[char], i: usize) -> Option<(usize, usize)> {
    let mut j = i + usize::from(s[i] == 'b');
    if s.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let hashes = s[j..].iter().take_while(|&&c| c == '#').count();
    (s.get(j + hashes) == Some(&'"')).then_some((j + hashes + 1, hashes))
}

/// Whether a file is test code, and whether it compiles on Windows.
#[derive(Clone, Copy, Debug)]
struct Module {
    test: bool,
    windows: bool,
}

/// Each module file's flags, from the `mod name;` that declares it and the file's own `#![cfg]`.
fn modules(root: &Path, sources: &HashMap<PathBuf, Vec<Token>>) -> HashMap<PathBuf, Module> {
    let mut found: HashMap<PathBuf, Module> = HashMap::new();
    let mut queue: Vec<(PathBuf, Module)> = ["lib.rs", "main.rs"]
        .iter()
        .map(|f| {
            (
                root.join("src").join(f),
                Module {
                    test: false,
                    windows: true,
                },
            )
        })
        .collect();
    for file in sources
        .keys()
        .filter(|f| f.parent() == Some(&root.join("tests")))
    {
        queue.push((
            file.clone(),
            Module {
                test: true,
                windows: true,
            },
        ));
    }
    while let Some((file, mut module)) = queue.pop() {
        let Some(tokens) = sources.get(&file) else {
            continue;
        };
        let regions = regions(tokens);
        // Inner attributes such as `#![cfg(unix)]` come first, after any doc comments.
        let code: Vec<Token> = tokens
            .iter()
            .filter(|t| !matches!(t.kind, Kind::Comment(_)))
            .cloned()
            .collect();
        let mut k = 0;
        while code.len() > k + 3 && code[k].is('#') && code[k + 1].is('!') && code[k + 2].is('[') {
            let close = if code[k + 3].ident() == Some("cfg") {
                let (windows, close) = cfg(&code, k + 3);
                module.windows &= windows;
                close + 1
            } else {
                k + code[k..].iter().position(|t| t.is(']')).unwrap()
            };
            k = close + 1;
        }
        found.insert(file.clone(), module);
        let dir = match file.file_name().and_then(|n| n.to_str()) {
            Some("lib.rs" | "main.rs" | "mod.rs") => file.parent().unwrap().to_path_buf(),
            _ if file.parent() == Some(&root.join("tests")) => root.join("tests"),
            _ => file.with_extension(""),
        };
        for k in 0..tokens.len().saturating_sub(2) {
            let (Some("mod"), Some(name)) = (tokens[k].ident(), tokens[k + 1].ident()) else {
                continue;
            };
            if !tokens[k + 2].is(';') {
                continue;
            }
            let child = Module {
                test: module.test || regions[k].test,
                windows: module.windows && regions[k].windows,
            };
            for path in [
                dir.join(format!("{name}.rs")),
                dir.join(name).join("mod.rs"),
            ] {
                if sources.contains_key(&path) && !found.contains_key(&path) {
                    queue.push((path, child));
                }
            }
        }
    }
    found
}

/// Each token's flags, from the `#[cfg]` attributes on the items around it.
fn regions(tokens: &[Token]) -> Vec<Module> {
    let mut flags = vec![
        Module {
            test: false,
            windows: true
        };
        tokens.len()
    ];
    for k in 0..tokens.len().saturating_sub(3) {
        if !(tokens[k].is('#') && tokens[k + 1].is('[') && tokens[k + 2].ident() == Some("cfg")) {
            continue;
        }
        let (windows, close) = cfg(tokens, k + 2);
        let test = tokens[k + 2..close]
            .iter()
            .any(|t| t.ident() == Some("test"));
        let end = item_end(tokens, close + 2);
        for flag in &mut flags[k..end] {
            flag.test |= test;
            flag.windows &= windows;
        }
    }
    flags
}

/// The index past the item starting at `start`: its `;` or `,` or its first block's `}`.
fn item_end(tokens: &[Token], start: usize) -> usize {
    let mut depth = 0i32;
    for (k, t) in tokens.iter().enumerate().skip(start) {
        match t.kind {
            Kind::Punct('{' | '(' | '[') => depth += 1,
            Kind::Punct('}') if depth == 1 => return k + 1,
            Kind::Punct('}' | ')' | ']') => {
                depth -= 1;
                if depth < 0 {
                    return k;
                }
            }
            Kind::Punct(';' | ',') if depth == 0 => return k + 1,
            _ => {}
        }
    }
    tokens.len()
}

/// Whether the `cfg(...)` at `k` can hold on Windows, and the index of its closing `)`.
fn cfg(tokens: &[Token], k: usize) -> (bool, usize) {
    // A predicate Windows doesn't settle, such as `target_arch`, takes whichever value keeps the
    // item live: `unknown` is true outside a `not` and false inside one.
    fn predicate(t: &[Token], k: usize, unknown: bool) -> (bool, usize) {
        let name = t[k].ident().unwrap_or_default();
        if t[k + 1].is('(') {
            let inner = if name == "not" { !unknown } else { unknown };
            let (mut values, mut j) = (Vec::new(), k + 2);
            while !t[j].is(')') {
                let (value, next) = predicate(t, j, inner);
                values.push(value);
                j = next + usize::from(t[next].is(','));
            }
            let value = match name {
                "not" => !values[0],
                "all" => values.iter().all(|&v| v),
                "any" => values.iter().any(|&v| v),
                _ => unknown,
            };
            return (value, j + 1);
        }
        if t[k + 1].is('=') {
            let windows = match (name, &t[k + 2].kind) {
                ("target_os" | "target_family", Kind::Str(value)) => value == "windows",
                _ => unknown,
            };
            return (windows, k + 3);
        }
        let windows = match name {
            "unix" => false,
            "windows" | "test" => true,
            _ => unknown,
        };
        (windows, k + 1)
    }
    predicate(tokens, k + 2, true)
}

#[derive(Debug, PartialEq)]
struct Hit {
    line: usize,
    path: String,
}

/// What a `let` bound that a later assertion may compare.
#[derive(Clone)]
enum Bound {
    /// A string literal holding a path, or one formatted from it.
    Text(usize, String),
    /// A path joined from such a literal, a problem only once it becomes a string.
    Joined(usize, String),
}

const ASSERTS: &[&str] = &[
    "assert!",
    "assert_eq!",
    "assert_ne!",
    "debug_assert!",
    "debug_assert_eq!",
    "debug_assert_ne!",
    "contains",
    "ends_with",
    "starts_with",
    "matches",
    "strip_prefix",
    "strip_suffix",
    "find",
];

/// Wrappers that keep a literal the expected value rather than the input to something.
const WRAPPERS: &[&str] = &[
    "",
    "format!",
    "vec!",
    "Some",
    "Ok",
    "String",
    "from",
    "into",
    "to_string",
    "to_owned",
    "as_str",
    "as_ref",
];

const STRINGIFY: &[&str] = &["to_str", "to_string_lossy", "display", "to_string"];

/// The flagged literals in one file's tokens, all of it test code when `test`.
fn scan(tokens: &[Token], test: bool) -> Vec<Hit> {
    let regions = regions(tokens);
    let opted: Vec<usize> = tokens
        .iter()
        .filter(|t| matches!(&t.kind, Kind::Comment(c) if c.starts_with(OPT_OUT) && c.len() > OPT_OUT.len() + 1))
        .map(|t| t.line)
        .collect();
    let opted = |line: usize| opted.contains(&line) || opted.contains(&(line - 1));
    let calls = calls(tokens);
    // Bindings are per function, not per block: a shadowing `let` in an inner block hides an outer
    // binding for the rest of the function, a rare miss not worth tracking scopes for.
    let mut bound: HashMap<String, Bound> = HashMap::new();
    let mut hits = Vec::new();
    let mut flag = |line: usize, path: String| {
        if !opted(line) {
            hits.push(Hit { line, path });
        }
    };
    let code: Vec<usize> = (0..tokens.len())
        .filter(|&k| !matches!(tokens[k].kind, Kind::Comment(_)))
        .collect();
    let at = |n: usize| code.get(n).map(|&k| &tokens[k]);
    for n in 0..code.len() {
        let k = code[n];
        let token = &tokens[k];
        let live = (test || regions[k].test) && regions[k].windows;
        if token.ident() == Some("fn") {
            bound.clear();
        }
        // A declaration Windows never compiles must not bind or shadow a name.
        if token.ident() == Some("let") && live {
            let mut j = n + 1;
            if at(j).and_then(Token::ident) == Some("mut") {
                j += 1;
            }
            let (Some(name), Some(eq)) = (at(j).and_then(Token::ident), at(j + 1)) else {
                continue;
            };
            if !eq.is('=') {
                continue;
            }
            let name = name.to_string();
            let init: Vec<&Token> = (j + 2..code.len())
                .map(|m| &tokens[code[m]])
                .take_while({
                    let mut depth = 0i32;
                    move |t| {
                        match t.kind {
                            Kind::Punct('(' | '[' | '{') => depth += 1,
                            Kind::Punct(')' | ']' | '}') => depth -= 1,
                            Kind::Punct(';') if depth == 0 => return false,
                            _ => {}
                        }
                        true
                    }
                })
                .collect();
            let names = |t: &Token| t.ident() == Some(&name);
            let previous = bound.remove(&name);
            // `let x = x.to_str()...` makes a joined path a string; any other rebinding is new.
            if let (Some(Bound::Joined(line, path)), [first, dot, method, ..]) =
                (&previous, &init[..])
                && names(first)
                && dot.is('.')
                && method.ident().is_some_and(|m| STRINGIFY.contains(&m))
            {
                bound.insert(name, Bound::Text(*line, path.clone()));
                continue;
            }
            if init.iter().any(|t| names(t)) {
                continue;
            }
            let literal = match &init[..] {
                [
                    Token {
                        kind: Kind::Str(s),
                        line,
                    },
                    ..,
                ] => Some((false, *line, s)),
                [
                    f,
                    bang,
                    open,
                    Token {
                        kind: Kind::Str(s),
                        line,
                    },
                    ..,
                ] if f.ident() == Some("format") && bang.is('!') && open.is('(') => {
                    Some((false, *line, s))
                }
                _ => init.windows(3).enumerate().find_map(|(i, w)| match w {
                    [
                        m,
                        open,
                        Token {
                            kind: Kind::Str(s),
                            line,
                        },
                    ] if matches!(m.ident(), Some("join" | "push")) && open.is('(') => {
                        // `root.join("a/b").to_string_lossy()` is already a string.
                        let text = init[i + 3..].windows(2).any(|w| {
                            w[0].is('.') && w[1].ident().is_some_and(|m| STRINGIFY.contains(&m))
                        });
                        Some((!text, *line, s))
                    }
                    _ => None,
                }),
            };
            if let Some((joined, line, s)) = literal
                && let Some(path) = paths(s).next()
            {
                let path = path.to_string();
                let value = if joined {
                    Bound::Joined(line, path)
                } else {
                    Bound::Text(line, path)
                };
                bound.insert(name, value);
            }
            continue;
        }
        if !live {
            continue;
        }
        match &token.kind {
            Kind::Ident(name) if bound.contains_key(name) => {
                let before = n.checked_sub(1).and_then(at);
                if before.is_some_and(|t| t.is('.') || t.is(':')) {
                    continue;
                }
                let method = at(n + 1)
                    .filter(|t| t.is('.'))
                    .and_then(|_| at(n + 2))
                    .and_then(Token::ident);
                let expected = expected(&calls[k]);
                match (&bound[name], method) {
                    (Bound::Text(..), None | Some("as_str" | "clone" | "to_string"))
                        if expected => {}
                    (Bound::Joined(..), Some(m)) if expected && STRINGIFY.contains(&m) => {}
                    _ => continue,
                }
                let (Bound::Text(line, path) | Bound::Joined(line, path)) =
                    bound.remove(name).unwrap();
                flag(line, path);
            }
            Kind::Str(s) if expected(&calls[k]) => {
                if let Some(path) = paths(s).next() {
                    flag(token.line, path.to_string());
                }
            }
            _ => {}
        }
    }
    hits
}

/// For each token, what each open delimiter around it was opened by, innermost last: a macro as
/// `name!`, a call or block after an identifier as its name, and anything else as "".
fn calls(tokens: &[Token]) -> Vec<Vec<String>> {
    let mut stack: Vec<String> = Vec::new();
    let mut out = Vec::with_capacity(tokens.len());
    let mut previous: [Option<&Token>; 2] = [None, None];
    for token in tokens {
        out.push(stack.clone());
        match token.kind {
            Kind::Comment(_) => continue,
            Kind::Punct('(' | '[' | '{') => {
                let callee = match previous {
                    [Some(name), Some(bang)] if bang.is('!') => {
                        format!("{}!", name.ident().unwrap_or_default())
                    }
                    [_, Some(name)] => name.ident().unwrap_or_default().to_string(),
                    _ => String::new(),
                };
                stack.push(callee);
            }
            Kind::Punct(')' | ']' | '}') => {
                stack.pop();
            }
            _ => {}
        }
        previous = [previous[1], Some(token)];
    }
    out
}

/// Whether a literal or name with these delimiters around it is an assertion's expected value.
fn expected(calls: &[String]) -> bool {
    for callee in calls.iter().rev() {
        if ASSERTS.contains(&callee.as_str()) {
            return true;
        }
        if callee == "native_path" || !WRAPPERS.contains(&callee.as_str()) {
            return false;
        }
    }
    false
}

/// The relative or `~/` paths in a literal: a `/`-joined run with a dotted name or `~` in it.
fn paths(text: &str) -> impl Iterator<Item = &str> {
    // An absolute path, even one with spaces, is a fake root the code passed through.
    let absolute = text.starts_with('/');
    let part = |c: char| c.is_alphanumeric() || "._-~@+{}/".contains(c);
    text.split(move |c| !part(c)).filter(move |run| {
        // After a `:` is a remote or URL path, such as `devbox:~/.claude` or `git@host:o/r.git`.
        let at = run.as_ptr().addr() - text.as_ptr().addr();
        if absolute || !run.contains('/') || run.starts_with('/') || text[..at].ends_with(':') {
            return false;
        }
        let segments: Vec<&str> = run.split('/').filter(|s| !s.is_empty()).collect();
        // `{ROOT}` names a constant, so it too is a fake root rather than a real directory.
        let constant = segments[0]
            .strip_prefix('{')
            .is_some_and(|s| s.starts_with(|c: char| c.is_ascii_uppercase()));
        segments.len() >= 2 && !constant && segments.iter().any(|s| named(s))
    })
}

/// Whether a path segment is `~`, a dotted name such as `.claude`, or a file such as `a.json`.
fn named(segment: &str) -> bool {
    if segment == "~" {
        return true;
    }
    let Some((name, extension)) = segment.rsplit_once('.') else {
        return false;
    };
    let extension_ok = extension.starts_with(|c: char| c.is_ascii_alphabetic())
        && extension.len() <= 8
        && extension.chars().all(|c| c.is_ascii_alphanumeric());
    (name.is_empty() || !name.ends_with('.')) && extension_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(source: &str) -> Vec<usize> {
        scan(&lex(source), true).iter().map(|h| h.line).collect()
    }

    #[test]
    fn an_expected_literal_is_flagged_wherever_the_assertion_puts_it() {
        let source = r##"
fn t() {
    assert!(text.contains("repo/.codex/config.toml"));
    assert_eq!(text, "in ~/.claude/settings.json\n");
    assert!(text.ends_with(&format!("{} in ~/.claude/settings.json", x)));
    assert_eq!(notes, [Some(r#"in ~/.claude."#)]);
}
"##;
        assert_eq!(lines(source), [3, 4, 5, 6]);
    }

    #[test]
    fn a_literal_bound_first_is_flagged_where_it_is_bound() {
        let source = r#"
fn t() {
    let project = format!("{cwd}/.claude/settings.json");
    std::fs::write(&project, "{}").unwrap();
    assert_eq!(u.control.file, project);
    let expected = "a/b.json";
    assert_eq!(actual, expected);
}
"#;
        assert_eq!(lines(source), [3, 6]);
    }

    #[test]
    fn a_joined_path_is_flagged_only_when_asserted_as_a_string() {
        let source = r#"
fn t() {
    let local = project.join(".claude/settings.local.json");
    std::fs::write(&local, "{}").unwrap();
    assert_eq!(found, local);
    let local = local.to_str().unwrap().to_string();
    assert_eq!(control(1), (Some(Inbound::Refuse), local));
}
fn u() {
    let file = w.home.join("a/b.json");
    assert_eq!(found.as_deref(), file.to_str());
}
fn v() {
    let data = w.home.join("a/b.json");
    p.files = vec![data.to_string_lossy().into_owned()];
    let canonical = w.home.join("c/d.json");
    let canonical = std::fs::canonicalize(&canonical).unwrap();
    assert_eq!(found, canonical.to_string_lossy());
}
fn w() {
    let expected = root.join("e/f.json").to_string_lossy().into_owned();
    assert_eq!(actual, expected);
}
"#;
        assert_eq!(lines(source), [3, 10, 21]);
    }

    #[test]
    fn input_data_and_fake_roots_are_not_expected_paths() {
        let source = r#"
fn t() {
    write(w.claude.join("projects/p/main.jsonl"), "{}");
    assert_eq!(row(&rows, "repo/CLAUDE.md").requests, 2);
    assert!(memory(r"cat ~/.codex/memories/notes.md"));
    assert_eq!(file, "/synthetic/.claude/settings.json");
    assert_eq!(file, format!("{ROOT}/settings.json"));
    assert!(text.contains("/Library/Application Support/ClaudeCode/x.json"));
    assert!(text.contains("https://code.claude.com/docs/en/env-vars.md"));
    assert!(text.contains("$25/mo, input/output, a/b, 1/2.5"));
    assert!(text.contains("How is devbox:~/.claude paid for? git@host:o/r.git"));
    assert!(text.contains(&native_path("~/.claude/settings.json")));
    let json = json!({"cwd": "repo/x.md"});
}
"#;
        assert_eq!(lines(source), Vec::<usize>::new());
    }

    #[test]
    fn an_opt_out_with_a_reason_covers_its_line_and_the_next() {
        let source = r#"
fn t() {
    assert!(text.contains("~/.claude")); // slash ok: a fake home passed through
    // slash ok: prose naming the file
    assert!(text.contains("a .codex/config.toml"));
    assert!(text.contains("~/.codex")); // slash ok:
}
"#;
        assert_eq!(lines(source), [6]);
    }

    #[test]
    fn code_that_skips_windows_or_tests_is_not_scanned() {
        let source = r#"
#[cfg(unix)]
mod unix {
    fn t() { assert!(text.contains("~/.claude/settings.json")); }
}
#[cfg(all(test, not(windows)))]
fn u() { assert!(text.contains("~/.claude/settings.json")); }
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn v() { assert!(text.contains("~/.claude/settings.json")); }
#[cfg(test)]
mod tests {
    fn w() { assert!(text.contains("~/.claude/settings.json")); }
}
fn product() { assert!(text.contains("~/.claude/settings.json")); }
#[cfg(all(test, not(target_arch = "aarch64")))]
fn x() { assert!(text.contains("~/.claude/settings.json")); }
#[cfg(all(test, not(any(unix, feature = "f"))))]
fn y() { assert!(text.contains("~/.claude/settings.json")); }
#[cfg(test)]
fn z() {
    #[cfg(windows)]
    let expected = native_path("a/b.json");
    #[cfg(unix)]
    let expected = "a/b.json";
    assert_eq!(actual, expected);
}
"#;
        assert_eq!(
            scan(&lex(source), false)
                .iter()
                .map(|h| h.line)
                .collect::<Vec<_>>(),
            [12, 16, 18]
        );
    }

    #[test]
    fn a_module_file_takes_its_declarations_flags_and_its_own_inner_cfg() {
        let root = Path::new("/r");
        let sources = HashMap::from([
            (
                root.join("tests").join("a.rs"),
                lex("//! Doc.\n#![cfg(unix)]\nmod b;\n"),
            ),
            (root.join("tests").join("b").join("mod.rs"), lex("")),
            (
                root.join("src").join("lib.rs"),
                lex("mod c;\n#[cfg(test)]\nmod d;\n"),
            ),
            (
                root.join("src").join("c.rs"),
                lex("#[cfg(not(windows))]\nmod e;\n"),
            ),
            (root.join("src").join("c").join("e.rs"), lex("")),
            (root.join("src").join("d.rs"), lex("")),
        ]);
        let modules = modules(root, &sources);
        let flags = |parts: &[&str]| {
            let m = modules[&parts
                .iter()
                .fold(root.to_path_buf(), |p, part| p.join(part))];
            (m.test, m.windows)
        };
        assert_eq!(flags(&["tests", "a.rs"]), (true, false));
        assert_eq!(flags(&["tests", "b", "mod.rs"]), (true, false));
        assert_eq!(flags(&["src", "c.rs"]), (false, true));
        assert_eq!(flags(&["src", "c", "e.rs"]), (false, false));
        assert_eq!(flags(&["src", "d.rs"]), (true, true));
    }

    #[test]
    fn the_lexer_keeps_quotes_in_chars_raw_strings_and_comments_out_of_literals() {
        let tokens = lex(
            "let q = '\"'; let l: &'a str; /* \"x\" /* nested */ */ r##\"a \"# b\"## b\"c\" // \"d\"\n'\\''",
        );
        let strings: Vec<&Kind> = tokens
            .iter()
            .map(|t| &t.kind)
            .filter(|k| matches!(k, Kind::Str(_)))
            .collect();
        assert_eq!(
            strings,
            [&Kind::Str("a \"# b".into()), &Kind::Str("c".into())]
        );
    }
}
