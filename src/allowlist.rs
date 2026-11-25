//! Claude Code's `availableModels` allowlist: the list the settings read now give, and
//! whether it permits a model a `vibes` card would suggest, by Claude Code's matching rules
//! (https://code.claude.com/docs/en/model-config#restrict-model-selection).

use serde_json::Value;

use crate::controls::SettingsScope;
use crate::model::Allowlist;

/// The setting that holds the allowlist.
const KEY: &str = "availableModels";

/// The model families an entry or an alias can name.
const FAMILIES: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];

/// Resolve the allowlist from `scopes`, lowest precedence first, where the scope whose file
/// is `managed_file` is the managed one. `remote` is Claude Code's cache of the admin
/// console's settings, with its file. None when no settings file sets a list.
pub fn control(
    scopes: &[SettingsScope],
    managed_file: &str,
    remote: SettingsScope,
) -> Option<Allowlist> {
    // The admin console outranks the managed file, and its cache may be stale, so a list
    // there leaves what Claude Code applies unknown.
    if remote.config.get(KEY).is_some_and(|v| !v.is_null()) {
        return Some(Allowlist::Unknown(format!(
            "the admin console's settings, cached in {}, set {KEY}, which token-max doesn't resolve",
            remote.file
        )));
    }
    let set = |s: &&SettingsScope| s.config.get(KEY).is_some_and(|v| !v.is_null());
    // A managed list alone applies; otherwise user, project and local lists concatenate.
    let mut sources: Vec<&SettingsScope> = vec![];
    match scopes.iter().find(|s| s.file == managed_file) {
        Some(managed) if set(&managed) => sources.push(managed),
        _ => {
            for scope in scopes.iter().filter(|s| s.file != managed_file).filter(set) {
                // A checkout's root and its session's directory can name one local file.
                if !sources
                    .iter()
                    .any(|s| crate::discovery::same_file(s.file, scope.file))
                {
                    sources.push(scope);
                }
            }
        }
    }
    if sources.is_empty() {
        return None;
    }
    let mut entries: Vec<String> = vec![];
    for s in &sources {
        let Some(list) = names(&s.config[KEY]) else {
            return Some(Allowlist::Unknown(format!(
                "{}:{KEY} is not a list of model names",
                s.file
            )));
        };
        for entry in list {
            if !entries.contains(&entry) {
                entries.push(entry);
            }
        }
    }
    Some(Allowlist::Read {
        entries,
        owners: sources
            .iter()
            .map(|s| format!("{}:{KEY}", s.file))
            .collect(),
    })
}

/// A list of strings, or None for any other value.
fn names(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(String::from))
        .collect()
}

/// What an allowlist makes of a model a card would name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The model runs as named.
    Permitted,
    /// An alias whose family the list narrows to these entries: on the Anthropic API it runs
    /// as the newest version they permit.
    Narrowed(Vec<String>),
    /// The list blocks it, with why, naming the setting.
    Blocked(String),
    /// Whether the list permits it is unknown, and why.
    Unknown(String),
}

/// How an entry matches, once normalized.
enum Entry<'a> {
    /// A family alias, which permits the whole family unless a specific entry names it.
    Family(&'static str),
    /// A version prefix or full model ID in a family, which permits itself and the IDs
    /// that extend it by segments.
    Specific(&'static str, &'a str),
    /// Anything else, such as a provider's model ID, which token-max can't match and never
    /// shows.
    Opaque,
}

fn entry(name: &str) -> Entry<'_> {
    if let Some(family) = FAMILIES.iter().find(|f| **f == name) {
        return Entry::Family(family);
    }
    // Only a well-formed ID is shown as a version; anything else could be a pasted secret.
    match name
        .strip_prefix("claude-")
        .and_then(|rest| rest.split('-').next())
        .and_then(|f| FAMILIES.iter().find(|family| **family == f))
    {
        Some(family) if crate::advisor::recognized(name) => Entry::Specific(family, name),
        _ => Entry::Opaque,
    }
}

/// The family of a model a card names, an alias or a `claude-<family>-` ID, and whether it
/// is an alias.
fn family(model: &str) -> Option<(&'static str, bool)> {
    match entry(model) {
        Entry::Family(f) => Some((f, true)),
        Entry::Specific(f, _) => Some((f, false)),
        Entry::Opaque => None,
    }
}

impl Allowlist {
    /// What the list makes of `model`, an alias or a Claude model ID, as Claude Code reads
    /// one: in any case and without a context-window suffix.
    pub(crate) fn verdict(&self, model: &str) -> Verdict {
        let (entries, owners) = match self {
            Allowlist::Unknown(why) => return Verdict::Unknown(why.clone()),
            Allowlist::Read { entries, owners } => (entries, owners),
        };
        let model = crate::advisor::normalize(model);
        let normalized: Vec<String> = entries
            .iter()
            .map(|e| crate::advisor::normalize(e))
            .collect();
        let parsed: Vec<Entry> = normalized.iter().map(|e| entry(e)).collect();
        // The setting, as a reason names it.
        let setting = match owners.as_slice() {
            [one] => one.clone(),
            many => format!(
                "{KEY}, set in {},",
                many.iter()
                    .map(|o| o.strip_suffix(&format!(":{KEY}")).unwrap_or(o))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
        };
        let blocked = || {
            Verdict::Blocked(if entries.is_empty() {
                format!("{setting} is empty, which blocks every model a setting names")
            } else {
                format!("{setting} excludes it")
            })
        };
        let opaque = parsed.iter().any(|e| matches!(e, Entry::Opaque));
        let unknown = || {
            Verdict::Unknown(format!(
                "{setting} lists an entry token-max can't match to a model"
            ))
        };
        let Some((family, alias)) = family(&model) else {
            return if opaque { unknown() } else { blocked() };
        };
        // A specific entry in the family turns off its family entry.
        let specific: Vec<&str> = parsed
            .iter()
            .filter_map(|e| match e {
                Entry::Specific(f, name) if *f == family => Some(*name),
                _ => None,
            })
            .collect();
        let wildcard = specific.is_empty()
            && parsed
                .iter()
                .any(|e| matches!(e, Entry::Family(f) if *f == family));
        if alias {
            if !specific.is_empty() {
                return Verdict::Narrowed(specific.iter().map(|s| s.to_string()).collect());
            }
        } else if specific
            .iter()
            .any(|e| model == *e || model.starts_with(&format!("{e}-")))
        {
            return Verdict::Permitted;
        }
        // An entry token-max can't read may name this family, which would turn its family
        // entry off, or permit the model itself.
        if opaque {
            return unknown();
        }
        if wildcard {
            return Verdict::Permitted;
        }
        blocked()
    }
}

/// Whether some entry of `entries`, a narrowed alias's, permits a version Claude Code's
/// pairing table takes as `main`'s advisor, or may: a version prefix such as
/// `claude-opus-5` reaches every minor version of its major.
pub(crate) fn pairs(main: &str, entries: &[String]) -> bool {
    entries.iter().any(|e| {
        let ceiling = match e.split('-').count() {
            3 => format!("{e}-99"),
            _ => e.clone(),
        };
        crate::advisor::accepts(main, &ceiling) != Some(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, json};

    fn config(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    const MANAGED: &str = "/etc/claude-code/managed-settings.json";

    fn resolve(user: Value, project: Value, managed: Value, remote: Value) -> Option<Allowlist> {
        let (user, project, managed, remote) = (
            config(user),
            config(project),
            config(managed),
            config(remote),
        );
        let scopes = [
            SettingsScope {
                config: &user,
                file: "/u/settings.json",
            },
            SettingsScope {
                config: &project,
                file: "/p/.claude/settings.json",
            },
            SettingsScope {
                config: &managed,
                file: MANAGED,
            },
        ];
        let remote = SettingsScope {
            config: &remote,
            file: "/u/remote-settings.json",
        };
        control(&scopes, MANAGED, remote)
    }

    fn read(entries: &[&str]) -> Allowlist {
        Allowlist::Read {
            entries: entries.iter().map(|e| e.to_string()).collect(),
            owners: vec!["/u/settings.json:availableModels".into()],
        }
    }

    #[test]
    fn a_managed_list_alone_applies_and_others_concatenate() {
        assert_eq!(resolve(json!({}), json!({}), json!({}), json!({})), None);
        assert_eq!(
            resolve(
                json!({"availableModels": ["sonnet", "opus"]}),
                json!({"availableModels": ["opus", "haiku"]}),
                json!({}),
                json!({})
            ),
            Some(Allowlist::Read {
                entries: vec!["sonnet".into(), "opus".into(), "haiku".into()],
                owners: vec![
                    "/u/settings.json:availableModels".into(),
                    "/p/.claude/settings.json:availableModels".into()
                ],
            })
        );
        // Lower scopes can't extend a managed list, even an empty one.
        assert_eq!(
            resolve(
                json!({"availableModels": ["opus"]}),
                json!({}),
                json!({"availableModels": []}),
                json!({})
            ),
            Some(Allowlist::Read {
                entries: vec![],
                owners: vec![format!("{MANAGED}:availableModels")],
            })
        );
        // A null sets no list.
        assert_eq!(
            resolve(
                json!({"availableModels": null}),
                json!({}),
                json!({"availableModels": null}),
                json!({})
            ),
            None
        );
        // The admin console's list outranks every file, and is not read.
        assert!(matches!(
            resolve(
                json!({"availableModels": ["opus"]}),
                json!({}),
                json!({}),
                json!({"availableModels": ["sonnet"]})
            ),
            Some(Allowlist::Unknown(why)) if why.contains("/u/remote-settings.json")
        ));
        // A value that is no list of names leaves the list unknown rather than failing.
        assert_eq!(
            resolve(
                json!({}),
                json!({"availableModels": "opus"}),
                json!({}),
                json!({})
            ),
            Some(Allowlist::Unknown(
                "/p/.claude/settings.json:availableModels is not a list of model names".into()
            ))
        );
    }

    #[test]
    fn entries_match_by_family_version_prefix_and_full_id() {
        use Verdict::*;
        let blocked = || Blocked("/u/settings.json:availableModels excludes it".into());
        for (entries, model, expected) in [
            // A family permits its aliases and IDs, in any case and window.
            (&["sonnet"][..], "claude-sonnet-5-5", Permitted),
            (&["Sonnet"], "SONNET[1m]", Permitted),
            (&["sonnet"], "claude-opus-5-5", blocked()),
            (&["sonnet"], "opus", blocked()),
            // A version prefix permits the IDs that extend it by segments, not a longer number.
            (&["claude-fable-5"], "claude-fable-5-2", Permitted),
            (&["claude-fable-5"], "claude-fable-5", Permitted),
            (&["claude-fable-5-1"], "claude-fable-5-2", blocked()),
            (&["claude-opus-5"], "claude-opus-50", blocked()),
            (&["claude-opus-5-5"], "claude-opus-5-5-20260101", Permitted),
            (&["claude-opus-5-5-20260101"], "claude-opus-5-5", blocked()),
            // A specific entry turns off its family's wildcard, not another family's.
            (&["opus", "claude-opus-4-8"], "claude-opus-5-5", blocked()),
            (&["opus", "claude-sonnet-5"], "claude-opus-5-5", Permitted),
            // A narrowed alias runs on the newest version its entries permit.
            (
                &["sonnet", "claude-opus-4-6"],
                "opus",
                Narrowed(vec!["claude-opus-4-6".into()]),
            ),
            (
                &["opus", "claude-opus-5"],
                "Opus[1m]",
                Narrowed(vec!["claude-opus-5".into()]),
            ),
        ] {
            assert_eq!(
                read(entries).verdict(model),
                expected,
                "{entries:?} {model}"
            );
        }
        // An empty list blocks every named model.
        assert_eq!(
            read(&[]).verdict("opus"),
            Blocked(
                "/u/settings.json:availableModels is empty, which blocks every model a setting names"
                    .into()
            )
        );
        // An entry token-max can't match leaves what it doesn't permit outright unknown.
        let mantle = read(&["opus", "anthropic.claude-opus-5-5-v1"]);
        assert_eq!(
            mantle.verdict("opus"),
            Unknown(
                "/u/settings.json:availableModels lists an entry token-max can't match to a model"
                    .into()
            )
        );
        // An entry shaped like an ID but malformed, which could be a pasted secret, is never
        // shown as a version.
        assert_eq!(
            read(&["claude-opus-sk-ant-secret"]).verdict("opus"),
            Unknown(
                "/u/settings.json:availableModels lists an entry token-max can't match to a model"
                    .into()
            )
        );
        assert_eq!(
            read(&["claude-opus-5", "anthropic.x"]).verdict("claude-opus-5-5"),
            Permitted
        );
        // Owners of a concatenated list are named together.
        let both = Allowlist::Read {
            entries: vec!["sonnet".into()],
            owners: vec![
                "/u/settings.json:availableModels".into(),
                "/p/settings.json:availableModels".into(),
            ],
        };
        assert_eq!(
            both.verdict("opus"),
            Blocked(
                "availableModels, set in /u/settings.json and /p/settings.json, excludes it".into()
            )
        );
    }

    #[test]
    fn a_narrowed_alias_pairs_when_some_permitted_version_may() {
        // Sonnet 5.5 takes Opus 5 or later.
        assert!(!pairs("claude-sonnet-5-5", &["claude-opus-4-6".into()]));
        assert!(pairs("claude-sonnet-5-5", &["claude-opus-5".into()]));
        // Haiku 5.5 takes Opus 4.7 or later, which the prefix `claude-opus-4` reaches.
        assert!(pairs("claude-haiku-5-5", &["claude-opus-4".into()]));
        assert!(pairs(
            "claude-sonnet-5-5",
            &["claude-opus-4-6".into(), "claude-opus-5-5-20260101".into()]
        ));
    }
}
