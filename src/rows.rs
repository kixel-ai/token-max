//! Which profiles each report line covers. A setting gets one line for each file that owns
//! it, covering every profile that file controls, so native Claude Code and a T3 instance
//! that share a home share one line for that home's `settings.json`.

use std::path::Path;

use crate::collections::HashMap;
use crate::model::Profile;

/// The profiles one line covers; every member's setting has the same owner.
#[derive(Clone, Debug)]
pub struct Group<'a> {
    /// The first member, whose controls the line reports.
    pub lead: &'a Profile,
    /// Every member's id, the lead's first.
    pub ids: Vec<String>,
}

impl<'a> Group<'a> {
    /// A line of one profile.
    #[cfg(test)]
    pub fn one(profile: &'a Profile) -> Self {
        Group {
            lead: profile,
            ids: vec![profile.id.clone()],
        }
    }
    /// Whether a record made under any of these profiles belongs to the line.
    pub fn covers(&self, profiles: &[String]) -> bool {
        profiles.iter().any(|p| self.ids.contains(p))
    }
}

/// Group profiles by the owner `owner` names for each. Profiles whose owner is the same key
/// of the same settings file share a group. Any other owner, such as an environment variable
/// or a T3 thread selection, keeps a group per profile, and so does a T3 instance that is
/// disabled or whose effective settings are unresolved.
pub fn groups<'a>(
    profiles: impl IntoIterator<Item = &'a Profile>,
    owner: impl Fn(&Profile) -> String,
) -> Vec<Group<'a>> {
    let mut index: HashMap<String, usize> = HashMap::default();
    let mut result: Vec<Group<'a>> = vec![];
    for p in profiles {
        let key = key(p, &owner(p));
        match index.get(&key) {
            Some(&i) => result[i].ids.push(p.id.clone()),
            None => {
                index.insert(key, result.len());
                result.push(Group {
                    lead: p,
                    ids: vec![p.id.clone()],
                });
            }
        }
    }
    result
}

/// The line a profile's setting belongs to: its owner's file and key when a file owns it and
/// the profile's settings are resolved, or else the profile itself.
pub fn key(profile: &Profile, owner: &str) -> String {
    match owner_file(profile, owner) {
        Some(file) => {
            // Two names for one file, such as a symlink, are one line.
            let real = crate::discovery::realpath(file).unwrap_or_else(|_| file.to_string());
            format!("{:?}:{real}{}", profile.harness, &owner[file.len()..])
        }
        None => format!("profile:{}", profile.id),
    }
}

/// The settings file whose line a profile's setting belongs to, as `owner` names it in
/// `{file}:{key}`; None when the line is the profile's own: another kind of owner, such as an
/// environment variable or a T3 thread selection, or a T3 instance that is disabled or whose
/// effective settings are unresolved.
pub fn owner_file<'o>(profile: &Profile, owner: &'o str) -> Option<&'o str> {
    let resolved = profile
        .t3
        .as_ref()
        .is_none_or(|t| t.enabled && !t.settings_unresolved);
    let end = [".json:", ".toml:"]
        .iter()
        .filter_map(|ext| owner.find(ext).map(|i| i + ext.len() - 1))
        .min()?;
    let file = &owner[..end];
    (resolved && Path::new(file).has_root()).then_some(file)
}

/// The line for a profile's tool, skill and MCP switches, which live in its settings file.
pub fn settings_key(profile: &Profile) -> String {
    key(profile, &format!("{}:", profile.settings_file))
}

/// The owners of a profile's compaction windows, its default and each model's, which a
/// line's profiles must all share.
pub fn window_owner(profile: &Profile) -> String {
    let c = &profile.compaction;
    std::iter::once(c.default.owner.as_str())
        .chain(c.models.values().map(|m| m.owner.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::make_profile;

    fn t3(id: &str, root: &str) -> Profile {
        let mut p = make_profile(id, root);
        p.t3 = Some(crate::model::T3Instance {
            state: "/state".into(),
            instance: id.into(),
            enabled: true,
            idle_mode: crate::model::T3IdleMode::OnReturn,
            idle_owner: "/state/settings.json:idleCompaction".into(),
            ownership_resolved: true,
            settings_unresolved: false,
            limitations: vec![],
            inherited_window: None,
            retained_since: None,
        });
        p
    }

    fn ids(groups: &[Group<'_>]) -> Vec<Vec<String>> {
        groups.iter().map(|g| g.ids.clone()).collect()
    }

    #[test]
    fn a_file_owner_merges_and_any_other_owner_stays_per_profile() {
        let native = make_profile("native", "/home/.claude");
        let agent = t3("agent", "/home/.claude");
        let other = make_profile("other", "/home/.claude-work");
        let profiles = [native, agent, other];
        let by_file = groups(&profiles, |p| p.ttl.main.owner.clone());
        assert_eq!(ids(&by_file), [vec!["native", "agent"], vec!["other"]]);
        assert_eq!(by_file[0].lead.id, "native");
        assert!(by_file[0].covers(&["x".into(), "agent".into()]));
        assert!(!by_file[1].covers(&["agent".into()]));
        let variable = groups(&profiles, |_| "BASH_MAX_TIMEOUT_MS".into());
        assert_eq!(ids(&variable), [["native"], ["agent"], ["other"]]);
        let thread = groups(&profiles, |p| {
            if p.t3.is_some() {
                "T3 instance agent: thread model selection".into()
            } else {
                "/home/.claude/settings.json:effortLevel".into()
            }
        });
        assert_eq!(ids(&thread), [vec!["native", "other"], vec!["agent"]]);
    }

    #[test]
    fn an_unresolved_or_disabled_t3_instance_keeps_its_own_line() {
        for unresolve in [
            |t: &mut crate::model::T3Instance| t.settings_unresolved = true,
            |t: &mut crate::model::T3Instance| t.enabled = false,
        ] {
            let mut agent = t3("agent", "/home/.claude");
            unresolve(agent.t3.as_mut().unwrap());
            let profiles = [make_profile("native", "/home/.claude"), agent];
            assert_eq!(
                ids(&groups(&profiles, |p| p.ttl.main.owner.clone())),
                [["native"], ["agent"]]
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn two_names_for_one_settings_file_share_a_line() {
        let w = crate::test_support::Workspace::new();
        let (real, alias) = (w.home.join(".claude"), w.home.join(".claude-alias"));
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&alias).unwrap();
        std::fs::write(real.join("settings.json"), "{}").unwrap();
        std::os::unix::fs::symlink(real.join("settings.json"), alias.join("settings.json"))
            .unwrap();
        let a = make_profile("a", &real.to_string_lossy());
        let b = make_profile("b", &alias.to_string_lossy());
        assert_eq!(settings_key(&a), settings_key(&b));
        assert_ne!(
            settings_key(&a),
            settings_key(&make_profile("c", &w.home.join(".claude-other").to_string_lossy()))
        );
    }
}
