//! Plain `major.minor.patch` versions, as Claude Code's and Codex's gates and token-max's releases compare them.

use std::fmt;

use crate::model::{Harness, Profile};

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
        }
    }

    /// Whether `version` is a plain version at least this one; a missing or malformed one is not.
    pub fn met(self, version: Option<&str>) -> bool {
        version
            .and_then(plain)
            .is_some_and(|v| v >= (self.major(), self.minor, self.patch))
    }

    /// Advice to upgrade, from the first of `latest`, each profile's latest session, that ran
    /// before this version; None when every one meets it.
    pub fn upgrade<'a>(
        self,
        latest: impl IntoIterator<Item = (&'a Profile, Option<&'a str>)>,
    ) -> Option<Upgrade> {
        let (profile, ran) = latest.into_iter().find(|(_, v)| !self.met(*v))?;
        Some(Upgrade {
            needs: self,
            ran: ran.map(str::to_string),
            // T3 runs its own binary path, which a native install's update may not reach.
            command: profile.t3.is_none().then_some(profile.updater).flatten(),
        })
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
    /// The command that upgrades the harness, when its install method is known.
    pub command: Option<&'static str>,
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
        match self.command {
            Some(command) => format!("upgrade {product} with `{command}`"),
            None => format!("upgrade {product}"),
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
            }
        );
        assert_eq!(
            old.why("applying it"),
            "applying it needs Claude Code 2.1.242 or later, but its latest session ran 2.1.200"
        );
        assert_eq!(old.step(), "upgrade Claude Code with `claude update`");
        // A T3 instance runs its own binary, and an unknown install names no command.
        for profile in [&t3, &plain] {
            let u = needs.upgrade([(profile, None)]).unwrap();
            assert_eq!(u.step(), "upgrade Claude Code");
            assert_eq!(
                u.why("this"),
                "this needs Claude Code 2.1.242 or later, but its latest session ran an unrecorded version"
            );
        }
        let codex = Needs::codex(156, 0)
            .upgrade([(&plain, Some("0.150.0-alpha"))])
            .unwrap();
        assert_eq!(
            codex.why("this"),
            "this needs Codex 0.156.0 or later, but its latest session ran a version token-max cannot read"
        );
    }
}
