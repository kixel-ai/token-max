//! Helpers that keep transcript content out of identifiers, errors and terminal output.

use std::sync::OnceLock;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::model::{Id, Location, Millis};

/// An error whose message is safe for terminal and JSON diagnostics.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct ScanError {
    pub message: String,
    pub location: Option<Location>,
}

impl ScanError {
    /// An error without a source location.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            location: None,
        }
    }
    /// An error at a transcript location.
    pub fn at(message: impl Into<String>, location: Location) -> Self {
        Self {
            message: message.into(),
            location: Some(location),
        }
    }
}

/// Accept objects without retaining surrounding content; anything else is empty.
pub fn object(value: &Value) -> &Map<String, Value> {
    static EMPTY: OnceLock<Map<String, Value>> = OnceLock::new();
    match value {
        Value::Object(map) => map,
        _ => EMPTY.get_or_init(Map::new),
    }
}

/// Look up a key, treating anything but an object as empty.
pub fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    object(value).get(key).unwrap_or(&Value::Null)
}

/// Retain only metadata identifiers, never free-form strings.
pub fn label(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    let valid = (1..=160).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:/@<>+-".contains(&b));
    valid.then(|| text.to_string())
}

/// A Codex originator label, admitting spaced client names such as `T3 Code` and
/// `Codex Desktop`.
pub fn originator(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    let valid = text.len() <= 64 && text.trim() == text;
    label(&Value::from(text.replace(' ', "_")))
        .filter(|_| valid)
        .map(|_| text.to_string())
}

/// [`identity`] held as its digest.
pub fn digest(value: &str) -> Id {
    Id::Digest(Sha256::digest(value.as_bytes()).into())
}

/// Opaque identifiers prevent accidentally exposing content put in an ID field.
pub fn identity(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((byte & 15) as u32, 16).unwrap());
    }
    out
}

const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Preserve absent counts as unknown and reject corrupt counts.
pub fn count(value: Option<&Value>) -> Result<Option<i64>, ScanError> {
    let invalid = || ScanError::new("invalid token count or duration");
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            if let Some(v) = n.as_u64() {
                return if v as f64 <= MAX_SAFE_INTEGER {
                    Ok(Some(v as i64))
                } else {
                    Err(invalid())
                };
            }
            match n.as_f64() {
                Some(f) if f >= 0.0 && f.fract() == 0.0 && f <= MAX_SAFE_INTEGER => {
                    Ok(Some(f as i64))
                }
                _ => Err(invalid()),
            }
        }
        Some(_) => Err(invalid()),
    }
}

/// Recognize recorded instants without date-only timezone guesses.
pub fn timestamp(value: &Value) -> Option<Millis> {
    parse_instant(value.as_str()?)
}

/// Parse an ISO date-time like JavaScript's `Date.parse`, local time when no offset is given.
pub fn parse_instant(text: &str) -> Option<Millis> {
    let b = text.as_bytes();
    let shape = b.len() > 10
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
        && b[10] == b'T';
    if !shape {
        return None;
    }
    if let Ok(ts) = text.parse::<jiff::Timestamp>() {
        return Some(ts.as_millisecond());
    }
    let civil = text.parse::<jiff::civil::DateTime>().ok()?;
    let zoned = civil.to_zoned(jiff::tz::TimeZone::system()).ok()?;
    Some(zoned.timestamp().as_millisecond())
}

/// Format an instant like JavaScript's `Date.prototype.toISOString`.
pub fn iso(millis: Millis) -> String {
    let ts = jiff::Timestamp::from_millisecond(millis).expect("instant in range");
    ts.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Read a required opaque ID without including its value in errors.
pub fn id(value: &Value) -> Result<String, ScanError> {
    match value.as_str() {
        Some(text) if !text.is_empty() => Ok(identity(text)),
        _ => Err(ScanError::new("missing request or session identity")),
    }
}

/// Node's `path.isAbsolute`: on Windows a rooted path such as `/project` counts, as does `C:\\project`.
pub fn is_absolute(path: &str) -> bool {
    if cfg!(windows) {
        let b = path.as_bytes();
        matches!(b.first(), Some(b'/' | b'\\'))
            || (b.len() > 2
                && b[0].is_ascii_alphabetic()
                && b[1] == b':'
                && matches!(b[2], b'/' | b'\\'))
    } else {
        path.starts_with('/')
    }
}

/// Print file paths without allowing terminal control sequences.
pub fn display(value: &str) -> String {
    terminal_text(&serde_json::to_string(value).expect("strings serialize"))
}

/// Escape terminal controls and direction overrides without dropping metadata.
pub fn terminal_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        let code = c as u32;
        let unsafe_char = code <= 0x1f
            || (0x7f..=0x9f).contains(&code)
            || code == 0x061c
            || code == 0x200e
            || code == 0x200f
            || (0x2028..=0x202e).contains(&code)
            || (0x2066..=0x2069).contains(&code);
        if unsafe_char {
            out.push_str(&format!("\\u{code:04x}"));
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_hold_digests_compactly_and_print_as_recorded() {
        let hex = identity("event");
        assert_eq!(Id::from(hex.as_str()), digest("event"));
        for text in [
            hex.as_str(),
            "modeled:claude:abc:0",
            &hex.to_uppercase(),
            &hex[1..],
        ] {
            let id = Id::from(text);
            assert_eq!(id.to_string(), text);
            assert_eq!(serde_json::to_value(&id).unwrap(), json!(text));
            assert_eq!(serde_json::from_value::<Id>(json!(text)).unwrap(), id);
        }
        assert!(matches!(Id::from(hex.to_uppercase()), Id::Text(_)));
    }

    #[test]
    fn labels_reject_free_text() {
        assert_eq!(
            label(&json!("claude-opus-5-5")),
            Some("claude-opus-5-5".into())
        );
        assert_eq!(label(&json!("has space")), None);
        assert_eq!(originator(&json!("T3 Code")), Some("T3 Code".into()));
        assert_eq!(originator(&json!(" padded")), None);
        assert_eq!(label(&json!("")), None);
        assert_eq!(label(&json!("x".repeat(161))), None);
        assert_eq!(label(&json!(5)), None);
    }

    #[test]
    fn counts_reject_corrupt_values() {
        assert_eq!(count(None).unwrap(), None);
        assert_eq!(count(Some(&json!(null))).unwrap(), None);
        assert_eq!(count(Some(&json!(5))).unwrap(), Some(5));
        assert_eq!(count(Some(&json!(5.0))).unwrap(), Some(5));
        assert!(count(Some(&json!(-1))).is_err());
        assert!(count(Some(&json!(1.5))).is_err());
        assert!(count(Some(&json!("5"))).is_err());
        assert!(count(Some(&json!(9_007_199_254_740_992u64))).is_err());
    }

    #[test]
    fn timestamps_need_a_time() {
        assert_eq!(
            timestamp(&json!("2026-11-01T00:00:00.000Z")),
            Some(1_793_491_200_000)
        );
        assert_eq!(timestamp(&json!("2026-11-01")), None);
        assert_eq!(timestamp(&json!("garbage")), None);
        assert_eq!(iso(1_793_491_200_000), "2026-11-01T00:00:00.000Z");
    }

    #[test]
    fn terminal_controls_are_escaped() {
        assert_eq!(
            terminal_text("a\u{1b}[31mb\u{202e}"),
            "a\\u001b[31mb\\u202e"
        );
        assert_eq!(display("a\nb"), "\"a\\\\nb\"".replace("\\\\", "\\"));
        assert_eq!(identity("x").len(), 64);
    }
}
