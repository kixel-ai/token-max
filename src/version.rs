//! The one comparison every client version gate shares.

/// A client version's numbers, when it is plain `major.minor.patch` digits.
pub(crate) fn claude_version(version: &str) -> Option<(u64, u64, u64)> {
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

/// Whether `version` is a plain version at least v2.`minor`.`patch`; a missing or malformed one is not.
pub(crate) fn claude_at_least(version: Option<&str>, minor: u64, patch: u64) -> bool {
    version
        .and_then(claude_version)
        .is_some_and(|v| v >= (2, minor, patch))
}

/// Whether `version` is a plain Codex version at least v0.`minor`.`patch`; a missing or
/// malformed one is not.
pub(crate) fn codex_at_least(version: Option<&str>, minor: u64, patch: u64) -> bool {
    version
        .and_then(claude_version)
        .is_some_and(|v| v >= (0, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::claude_at_least;

    #[test]
    fn only_plain_three_part_digit_versions_compare() {
        assert!(claude_at_least(Some("2.1.314"), 1, 314));
        assert!(claude_at_least(Some("3.0.0"), 1, 314));
        assert!(claude_at_least(Some("2.2.0"), 1, 999));
        assert!(!claude_at_least(Some("2.1.313"), 1, 314));
        assert!(!claude_at_least(Some("1.9.999"), 1, 0));
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
            assert!(!claude_at_least(Some(malformed), 0, 0), "{malformed:?}");
        }
        assert!(!claude_at_least(None, 0, 0));
    }
}
