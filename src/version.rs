//! The one comparison every Claude Code version gate shares.

/// Whether `version` is plain `major.minor.patch` digits and at least v2.`minor`.`patch`.
/// Anything else, such as a missing, malformed or overflowing version, is not.
pub(crate) fn claude_at_least(version: Option<&str>, minor: u64, patch: u64) -> bool {
    let Some(version) = version else {
        return false;
    };
    let mut parts = version.split('.').map(|part| {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse::<u64>().ok()
    });
    let (Some(Some(major)), Some(Some(mine)), Some(Some(micro)), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    (major, mine, micro) >= (2, minor, patch)
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
