//! Plain `major.minor.patch` versions, as Claude Code's and Codex's gates and token-max's releases compare them.

use std::fmt;

use crate::model::{Harness, Profile, Request};

/// A version's numbers, when it is plain `major.minor.patch` digits.
pub(crate) fn plain(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.').map(|part| {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse::<u64>().ok()
    });
    let (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    Some((major, minor, patch))
}

/// The version each of `members` ran on its latest request in `requests`, for those with
/// any: what its next session most likely runs, whatever the request was for.
pub(crate) fn latest<'a>(
    members: &[&'a Profile],
    requests: impl IntoIterator<Item = &'a Request>,
) -> Vec<(&'a Profile, Option<&'a str>)> {
    latest_where(members, requests, |_, _| true)
}

/// What [`latest`] returns, counting only each member's own interactive install: the one an
/// upgrade reaches. An Agent SDK app, remote session or MCP client on the same settings file
/// pins its own Claude Code, so it never names an upgrade.
pub(crate) fn installed<'a>(
    members: &[&'a Profile],
    requests: impl IntoIterator<Item = &'a Request>,
) -> Vec<(&'a Profile, Option<&'a str>)> {
    latest_where(members, requests, interactive)
}

/// Whether an upgrade of `profile`'s own install reaches the client that sent `request`.
fn interactive(profile: &Profile, request: &Request) -> bool {
    // T3 runs its own binary under each instance's profile, and Codex profiles count every request.
    profile.harness != Harness::Claude
        || profile.t3.is_some()
        || matches!(
            request.client.as_deref(),
            Some("cli" | "claude-vscode" | "ssh-remote")
        )
}

fn latest_where<'a>(
    members: &[&'a Profile],
    requests: impl IntoIterator<Item = &'a Request>,
    keep: impl Fn(&Profile, &Request) -> bool,
) -> Vec<(&'a Profile, Option<&'a str>)> {
    let mut last: Vec<Option<&Request>> = vec![None; members.len()];
    for r in requests {
        for (i, p) in members.iter().enumerate() {
            if r.profiles.contains(&p.id) && keep(p, r) && last[i].is_none_or(|l| r.time >= l.time)
            {
                last[i] = Some(r);
            }
        }
    }
    members
        .iter()
        .zip(last)
        .filter_map(|(&p, r)| Some((p, r?.version.as_deref())))
        .collect()
}

/// The first version of a harness that a check needs, such as Claude Code 2.1.242.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Needs {
    pub harness: Harness,
    pub minor: u64,
    pub patch: u64,
}

impl Needs {
    /// Claude Code 2.`minor`.`patch`.
    pub const fn claude(minor: u64, patch: u64) -> Self {
        Needs {
            harness: Harness::Claude,
            minor,
            patch,
        }
    }

    /// Codex 0.`minor`.`patch`.
    pub const fn codex(minor: u64, patch: u64) -> Self {
        Needs {
            harness: Harness::Codex,
            minor,
            patch,
        }
    }

    fn major(self) -> u64 {
        match self.harness {
            Harness::Claude => 2,
            Harness::Codex => 0,
            Harness::OpenCode => 2,
            Harness::Antigravity => unreachable!("no check needs an Antigravity version"),
        }
    }

    /// Whether `version` is a plain version at least this one; a missing or malformed one is not.
    pub fn met(self, version: Option<&str>) -> bool {
        version
            .and_then(plain)
            .is_some_and(|v| v >= (self.major(), self.minor, self.patch))
    }

    /// Why a setting waits on this version, from the first of `latest`, each profile's latest
    /// session, that ran before it; None when every one meets it. An unrecorded or unreadable
    /// version counts as too old, since nothing shows the setting applies.
    pub fn unmet<'a>(
        self,
        latest: impl IntoIterator<Item = (&'a Profile, Option<&'a str>)>,
    ) -> Option<Upgrade> {
        let old: Vec<(&Profile, Option<&str>)> =
            latest.into_iter().filter(|(_, v)| !self.met(*v)).collect();
        let &(_, ran) = old.first()?;
        // T3 runs its own binary path, which a native install's update may not reach, so a
        // command is named only when it reaches every install that is too old.
        let commands: Vec<Option<&'static str>> = old
            .iter()
            .map(|(p, _)| p.t3.is_none().then_some(p.updater).flatten())
            .collect();
        let command = commands[0].filter(|c| commands.iter().all(|o| *o == Some(*c)));
        // Native profiles that share a settings file share one install; each T3 instance
        // may run its own, and two T3 states can reuse an instance ID.
        let installs: std::collections::BTreeSet<Option<(&str, &str)>> = old
            .iter()
            .map(|(p, _)| {
                p.t3.as_ref()
                    .map(|t| (t.state.as_str(), t.instance.as_str()))
            })
            .collect();
        Some(Upgrade {
            needs: self,
            ran: ran.map(str::to_string),
            command,
            several: installs.len() > 1,
        })
    }

    /// Advice to upgrade, as [`Needs::unmet`] words it, when every version too old for this one
    /// is a recorded release: an unrecorded or unreadable one may already meet it, so it never
    /// advises an upgrade or names a command.
    pub fn upgrade<'a>(
        self,
        latest: impl IntoIterator<Item = (&'a Profile, Option<&'a str>)>,
    ) -> Option<Upgrade> {
        let latest: Vec<(&Profile, Option<&str>)> = latest.into_iter().collect();
        if latest.iter().any(|(_, v)| v.and_then(plain).is_none()) {
            return None;
        }
        self.unmet(latest)
    }

    /// Advice to upgrade the interactive installs of `members`, when they are the only clients
    /// too old for this version. None when every one meets it, or when another client on a
    /// member's settings file, which pins its own Claude Code, is also too old, since upgrading
    /// the installs alone would still leave the setting held back.
    pub fn advice<'a, R>(self, members: &[&'a Profile], requests: R) -> Option<Upgrade>
    where
        R: IntoIterator<Item = &'a Request> + Clone,
    {
        let pinned = latest_where(members, requests.clone(), |p, r| !interactive(p, r));
        if pinned.iter().any(|(_, v)| !self.met(*v)) {
            return None;
        }
        self.upgrade(installed(members, requests))
    }
}

impl fmt::Display for Needs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}.{}.{}",
            self.harness.product(),
            self.major(),
            self.minor,
            self.patch
        )
    }
}

/// A setting the next session would read that its harness is too old for, and how to
/// upgrade it. Every line and card a version holds back words it through this.
#[derive(Clone, Debug, PartialEq)]
pub struct Upgrade {
    pub needs: Needs,
    /// The version the latest session recorded; None when it recorded none.
    pub ran: Option<String>,
    /// The command that upgrades every install that is too old, when one does.
    pub command: Option<&'static str>,
    /// Whether more than one install is too old, such as native Claude Code and a T3
    /// instance that share a settings file.
    pub several: bool,
}

impl Upgrade {
    /// Why `what` waits, such as "applying it needs Claude Code 2.1.242 or later, but its
    /// latest session ran 2.1.200".
    pub fn why(&self, what: &str) -> String {
        let ran = match self.ran.as_deref() {
            None => "an unrecorded version",
            // Only a plain version is shown, so a malformed one never reaches the terminal.
            Some(v) if plain(v).is_none() => "a version token-max cannot read",
            Some(v) => v,
        };
        format!(
            "{what} needs {} or later, but its latest session ran {ran}",
            self.needs
        )
    }

    /// The step, such as "upgrade Claude Code with `claude update`".
    pub fn step(&self) -> String {
        let product = self.needs.harness.product();
        let what = if self.several {
            format!("every {product} install that reads it")
        } else {
            product.to_string()
        };
        match self.command {
            Some(command) => format!("upgrade {what} with `{command}`"),
            None => format!("upgrade {what}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Needs, Upgrade};

    #[test]
    fn only_plain_three_part_digit_versions_compare() {
        let needs = Needs::claude(1, 314);
        assert!(needs.met(Some("2.1.314")));
        assert!(needs.met(Some("3.0.0")));
        assert!(Needs::claude(1, 999).met(Some("2.2.0")));
        assert!(!needs.met(Some("2.1.313")));
        assert!(!Needs::claude(1, 0).met(Some("1.9.999")));
        // Codex's versions start at 0.
        assert!(Needs::codex(156, 0).met(Some("0.156.0")));
        assert!(!Needs::codex(156, 0).met(Some("0.155.9")));
        for malformed in [
            "2.1",
            "2.1.3.4",
            "2.1.314-beta",
            " 2.1.314",
            "2.1.0x200",
            "2.1.-0",
            "2.1.",
            "2..1",
            "2.1.+5",
            "2.1.1e3",
            "",
            "99999999999999999999.0.0",
        ] {
            assert!(!Needs::claude(0, 0).met(Some(malformed)), "{malformed:?}");
        }
        assert!(!Needs::claude(0, 0).met(None));
    }

    #[test]
    fn each_profile_is_judged_by_its_latest_request_of_any_kind() {
        use crate::test_support::{make_profile, make_request};
        let a = make_profile("a", "/a");
        let b = make_profile("b", "/b");
        let quiet = make_profile("quiet", "/q");
        let request = |profile: &str, time, version: &str| {
            let mut r = make_request();
            r.time = time;
            r.profiles = vec![profile.into()].into();
            r.version = Some(version.into());
            r
        };
        // Out of order, as a subagent's older request may follow a newer main one.
        let requests = [
            request("a", 20, "2.1.300"),
            request("a", 10, "2.1.100"),
            request("b", 5, "2.1.200"),
        ];
        let versions = super::latest(&[&a, &b, &quiet], &requests);
        let versions: Vec<(&str, Option<&str>)> =
            versions.iter().map(|(p, v)| (p.id.as_str(), *v)).collect();
        assert_eq!(versions, [("a", Some("2.1.300")), ("b", Some("2.1.200"))]);
    }

    #[test]
    fn only_a_native_claude_profiles_interactive_requests_name_its_install() {
        use crate::model::Harness;
        use crate::test_support::{make_profile, make_request, make_t3_profile};
        let native = make_profile("native", "/n");
        let t3 = make_t3_profile("t3", "/t");
        let mut codex = make_profile("codex", "/c");
        codex.harness = Harness::Codex;
        let request = |profile: &str, time, client: &str, version: &str| {
            let mut r = make_request();
            r.time = time;
            r.profiles = vec![profile.into()].into();
            r.client = Some(client.into());
            r.version = Some(version.into());
            r
        };
        let requests = [
            request("native", 10, "ssh-remote", "2.1.300"),
            request("native", 20, "sdk-ts", "2.1.100"),
            request("t3", 20, "sdk-ts", "2.1.200"),
            request("codex", 20, "codex_exec", "0.150.0"),
        ];
        let versions = super::installed(&[&native, &t3, &codex], &requests);
        let versions: Vec<(&str, Option<&str>)> =
            versions.iter().map(|(p, v)| (p.id.as_str(), *v)).collect();
        assert_eq!(
            versions,
            [
                ("native", Some("2.1.300")),
                ("t3", Some("2.1.200")),
                ("codex", Some("0.150.0"))
            ]
        );
    }

    #[test]
    fn an_unowned_t3_line_advises_no_upgrade() {
        use crate::model::{Action, Bucket, Ttl};
        use crate::test_support::{make_t3_profile, make_ttl_check};
        let mut t3 = make_t3_profile("t3", "/c");
        t3.t3.as_mut().unwrap().enabled = false;
        let mut check = make_ttl_check("t3", Bucket::Main, Action::Advisory, Ttl::OneHour);
        check.base.upgrade = Needs::claude(1, 242).upgrade([(&t3, Some("2.1.100"))]);
        assert!(check.base.upgrade.is_some());
        crate::t3::constrain(&mut check.base, &[t3]);
        assert_eq!(check.base.upgrade, None);
    }

    #[test]
    fn upgrades_name_the_first_old_profile_and_its_command_only_when_known() {
        use crate::test_support::{make_profile, make_t3_profile};
        let mut native = make_profile("native", "/c");
        native.updater = Some("claude update");
        let mut t3 = make_t3_profile("t3", "/c");
        t3.updater = Some("claude update");
        let plain = make_profile("plain", "/p");
        let needs = Needs::claude(1, 242);

        assert_eq!(needs.upgrade([(&native, Some("2.1.242"))]), None);
        assert_eq!(needs.upgrade([]), None);
        let old = needs
            .upgrade([(&plain, Some("2.1.300")), (&native, Some("2.1.200"))])
            .unwrap();
        assert_eq!(
            old,
            Upgrade {
                needs,
                ran: Some("2.1.200".into()),
                command: Some("claude update"),
                several: false,
            }
        );
        assert_eq!(
            old.why("applying it"),
            "applying it needs Claude Code 2.1.242 or later, but its latest session ran 2.1.200"
        );
        assert_eq!(old.step(), "upgrade Claude Code with `claude update`");
        // Native and T3 installs both too old: no one command reaches both.
        let both = needs
            .upgrade([(&native, Some("2.1.200")), (&t3, Some("2.1.100"))])
            .unwrap();
        assert_eq!(
            (both.ran.as_deref(), both.step().as_str()),
            (
                Some("2.1.200"),
                "upgrade every Claude Code install that reads it"
            )
        );
        // Two T3 states may each run their own binary under the same instance ID.
        let mut prod = make_t3_profile("prod", "/c");
        let mut dev = make_t3_profile("dev", "/c");
        prod.t3.as_mut().unwrap().instance = "claudeAgent".into();
        dev.t3.as_mut().unwrap().instance = "claudeAgent".into();
        dev.t3.as_mut().unwrap().state = "/dev".into();
        let states = needs
            .upgrade([(&prod, Some("2.1.200")), (&dev, Some("2.1.100"))])
            .unwrap();
        assert_eq!(
            states.step(),
            "upgrade every Claude Code install that reads it"
        );
        // A T3 instance runs its own binary, and an unknown install names no command.
        for profile in [&t3, &plain] {
            let u = needs.upgrade([(profile, Some("2.1.100"))]).unwrap();
            assert_eq!(u.step(), "upgrade Claude Code");
        }
        // An unrecorded or unreadable version holds the setting back, but may already meet
        // it, so no gate advises an upgrade, even beside a recorded old one.
        for ran in [None, Some("2.1.400-beta")] {
            for latest in [
                vec![(&native, ran)],
                vec![(&native, Some("2.1.100")), (&plain, ran)],
            ] {
                assert!(needs.unmet(latest.clone()).is_some(), "{ran:?}");
                assert_eq!(needs.upgrade(latest), None, "{ran:?}");
            }
        }
        assert_eq!(
            needs.unmet([(&native, None)]).unwrap().why("this"),
            "this needs Claude Code 2.1.242 or later, but its latest session ran an unrecorded version"
        );
        let codex = Needs::codex(156, 0)
            .unmet([(&plain, Some("0.150.0-alpha"))])
            .unwrap();
        assert_eq!(
            codex.why("this"),
            "this needs Codex 0.156.0 or later, but its latest session ran a version token-max cannot read"
        );
    }
}
