//! Fingerprints the source files that define what an `--ssh` stream carries, so the binary
//! holds a digest of them instead of their text.

use std::fmt::Write;

use sha2::{Digest, Sha256};

/// Each file whose types or values cross an `--ssh` stream, in hashing order.
const SOURCES: &[&str] = &[
    "wire.rs",
    "prices.rs",
    "model.rs",
    "sessions.rs",
    "audit.rs",
    "repository.rs",
    "totals.rs",
    // Its `Credit` is a field of the totals a report carries.
    "defaults.rs",
    // Its `Total` is a field of the totals a report carries, and its offers and answers cross
    // for a walk, with `mcp.rs`'s entries and `report.rs`'s choices.
    "doctor.rs",
    "mcp.rs",
    "report.rs",
    // Its `ScanError` is the stream's error outcome.
    "safe.rs",
    // Its items are what `vibes` carries.
    "vibes.rs",
];

fn main() {
    let mut hash = Sha256::new();
    for name in SOURCES {
        let path = format!("src/{name}");
        println!("cargo:rerun-if-changed={path}");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        // A Windows checkout may convert line endings, which must not change the fingerprint a
        // coordinator compares with another platform's build of the same commit.
        hash.update(text.replace("\r\n", "\n"));
    }
    let mut schema = String::new();
    for byte in &hash.finalize()[..6] {
        write!(schema, "{byte:02x}").expect("writing to a String");
    }
    println!("cargo:rustc-env=TOKEN_MAX_WIRE_SCHEMA={schema}");
}
