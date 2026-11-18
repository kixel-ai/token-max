//! Which profiles each report line covers. A setting gets one line for each file that owns
//! it, covering every profile that file controls, so native Claude Code and a T3 instance
//! that share a home share one line for that home's `settings.json`.

use std::path::Path;

use crate::collections::HashMap;
use crate::model::{Bucket, Profile};

/// The profiles one line covers; every member's setting has the same owner.
#[derive(Clone, Debug)]
pub struct Group<'a> {
    /// The first member, whose controls the line reports.
    pub lead: &'a Profile,
    /// Every member's id, the lead's first.
    pub ids: Vec<String>,
    /// Every member, in the order of `ids`.
    pub members: Vec<&'a Profile>,
}

impl<'a> Group<'a> {
    /// A line of one profile.
    #[cfg(test)]
    pub fn one(profile: &'a Profile) -> Self {
        Group {
            lead: profile,
            ids: vec![profile.id.clone()],
            members: vec![profile],
        }
    }
    /// Whether a record made under any of these profiles belongs to the line.
    pub fn covers(&self, profiles: &[String]) -> bool {
        profiles.iter().any(|p| self.ids.contains(p))
    }
    /// The member that recorded a record made under `profiles`, else the lead. Members share
    /// the owning file, but each holds only its own requests' directories and overrides.
    pub fn recorder(&self, profiles: &[String]) -> &'a Profile {
        profiles
            .iter()
            .find_map(|p| self.members.iter().find(|m| &m.id == p))
            .copied()
            .unwrap_or(self.lead)
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
            Some(&i) => {
                result[i].ids.push(p.id.clone());
                result[i].members.push(p);
            }
            None => {
                index.insert(key, result.len());
                result.push(Group {
                    lead: p,
                    ids: vec![p.id.clone()],
                    members: vec![p],
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
        Some(_) => format!("{:?}:{}", profile.harness, canonical(owner)),
        None => format!("profile:{}", profile.id),
    }
}

/// An owner with each line's settings file resolved, so two names for one file, such as a
/// symlink, are one owner, in a composite owner's later lines as well as its first.
pub fn canonical(owner: &str) -> String {
    owner
        .split('\n')
        .map(|line| match file_prefix(line) {
            Some(file) => {
                let real = crate::discovery::realpath(file).unwrap_or_else(|_| file.to_string());
                format!("{real}{}", &line[file.len()..])
            }
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The rooted settings file an owner names in `{file}:{key}`.
fn file_prefix(owner: &str) -> Option<&str> {
    let end = [".json:", ".toml:"]
        .iter()
        .filter_map(|ext| owner.find(ext).map(|i| i + ext.len() - 1))
        .min()?;
    let file = &owner[..end];
    Path::new(file).has_root().then_some(file)
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
    file_prefix(owner).filter(|_| resolved)
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

/// The owner and value of a profile's cache TTL for one bucket, whether an unset value
/// is the billing default a change can pin rather than caching turned off, whether its
/// projects set it differently, and, for the main conversation, how the profile was answered
/// to pay. A profile whose TTL another owner sets, that caching is off for, whose projects
/// differ or whose default differs replays or compares differently.
pub fn ttl_owner(profile: &Profile, bucket: Bucket) -> String {
    let ttl = &profile.ttl.get(bucket).one();
    let pinnable = ttl.value.is_none() && ttl.reason == crate::discovery::TTL_BILLING_UNKNOWN;
    let billing = (bucket == Bucket::Main)
        .then_some(profile.billing)
        .flatten();
    format!(
        "{}\n{:?}\n{pinnable}\n{}\n{billing:?}",
        ttl.owner, ttl.value, ttl.varies
    )
}

/// The owners of the compaction windows, then of both cache TTLs the window replay
/// applies, which a line's profiles must all share.
pub fn compaction_owner(profile: &Profile) -> String {
    format!(
        "{}\n{}\n{}",
        window_owner(profile),
        ttl_owner(profile, Bucket::Main),
        ttl_owner(profile, Bucket::Subagent)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{make_profile, make_t3_profile as t3};

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
    fn replays_share_a_line_only_when_their_ttls_match() {
        let native = make_profile("native", "/home/.claude");
        // Caching off for the instance leaves the file's owner but no TTL to replay.
        let mut off = t3("off", "/home/.claude");
        off.ttl.main.value = None;
        // An environment TTL another owner sets changes the window replay, not its owner.
        let mut env = t3("env", "/home/.claude");
        env.ttl.subagent.owner = "T3 instance env: CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL".into();
        let profiles = [native, off, env];
        assert_eq!(
            ids(&groups(&profiles, |p| ttl_owner(p, Bucket::Main))),
            [vec!["native", "env"], vec!["off"]]
        );
        assert_eq!(
            ids(&groups(&profiles, compaction_owner)),
            [["native"], ["off"], ["env"]]
        );
        // An unset default whose billing is unknown can be pinned; caching off cannot.
        let [mut native, off, _] = profiles;
        native.ttl.main.value = None;
        native.ttl.main.reason = crate::discovery::TTL_BILLING_UNKNOWN.into();
        assert_eq!(
            ids(&groups(&[native.clone(), off.clone()], |p| ttl_owner(
                p,
                Bucket::Main
            ))),
            [["native"], ["off"]]
        );
        // Projects that set it differently leave no one value, unlike caching off.
        native.ttl.main.value = Some(crate::model::Ttl::OneHour);
        native.ttl.main.varies = true;
        native.ttl.main.reason = crate::discovery::TTL_PROJECTS_DIFFER.into();
        assert_eq!(
            ids(&groups(&[native, off], |p| ttl_owner(p, Bucket::Main))),
            [["native"], ["off"]]
        );
        // Roots answered to pay differently compare with different main defaults.
        let plan = crate::model::Profile {
            billing: Some(crate::model::Billing::Plan),
            ..make_profile("plan", "/home/.claude")
        };
        let api = crate::model::Profile {
            billing: Some(crate::model::Billing::Api),
            ..make_profile("api", "/home/.claude")
        };
        let profiles = [plan, api];
        assert_eq!(
            ids(&groups(&profiles, |p| ttl_owner(p, Bucket::Main))),
            [["plan"], ["api"]]
        );
        assert_eq!(
            ids(&groups(&profiles, |p| ttl_owner(p, Bucket::Subagent))),
            [vec!["plan", "api"]]
        );
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
        // A composite owner resolves every file it names, not just its first.
        assert_eq!(ids(&groups([&a, &b], compaction_owner)), [vec!["a", "b"]]);
        assert_eq!(
            groups([&a, &b], compaction_owner)[0]
                .recorder(&["b".into()])
                .id,
            "b"
        );
        assert_ne!(
            settings_key(&a),
            settings_key(&make_profile(
                "c",
                &w.home.join(".claude-other").to_string_lossy()
            ))
        );
    }
}
