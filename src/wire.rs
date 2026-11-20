//! The private SSH audit stream between this host and a destination's
//! `token-max doctor --ssh-scan`.
//!
//! The destination writes a [`Header`] and starts scanning. This host answers with one
//! [`Request`] on the destination's stdin, carrying the price catalog and the [`Job`], and the
//! destination analyzes its own scan and writes one [`Outcome`]: its finished report, its
//! priced session lines, or a diagnostic, never the scan. Each value is one JSON line.
//!
//! A report for a walk also carries the destination's offers and leaves the stream open: this
//! host walks through them, sends the [`Answer`]s, and the destination replies to each with
//! another [`Outcome`], so its files are read and written only there.

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::doctor::{Event, Offers, Written};
use crate::model::{Location, Millis};
use crate::prices::Catalog;
use crate::sessions::Session;
use crate::totals::Totals;

/// This build's identity: the package version and a fingerprint of the types a stream carries,
/// the price catalog, report and session lines included.
///
/// Unpublished builds share a version, so the fingerprint keeps two different builds of the
/// same version from misreading each other's streams.
pub fn build() -> String {
    let schema = Sha256::digest(
        [
            include_str!("wire.rs"),
            include_str!("prices.rs"),
            include_str!("model.rs"),
            include_str!("sessions.rs"),
            include_str!("repository.rs"),
            include_str!("totals.rs"),
            // Its `Total` is a field of the totals a report carries, and its offers and
            // answers cross for a walk, with `mcp.rs`'s entries and `report.rs`'s choices.
            include_str!("doctor.rs"),
            include_str!("mcp.rs"),
            include_str!("report.rs"),
        ]
        .concat(),
    );
    format!(
        "{}+{}",
        env!("CARGO_PKG_VERSION"),
        crate::safe::hex(&schema[..6])
    )
}

/// The first value of every stream, written before the scan starts.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    /// The sender's [`build`]; this host reads streams only from its own build.
    pub token_max_scan: String,
}

/// What a destination analyzes its scan with, so every host is priced from one catalog.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub catalog: Catalog,
    pub job: Job,
}

/// What the destination makes of its scan.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Job {
    /// `token-max doctor`'s report.
    #[serde(rename_all = "camelCase")]
    Report {
        max_cost_per_use_usd: f64,
        format: Format,
        /// Whether this host walks through the report's changes, which keeps the stream open.
        walk: bool,
    },
    /// `token-max sessions`' lines.
    Sessions,
}

/// How a host's report is printed for this host's terminal, under the host's name when the
/// run covers several.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Format {
    pub width: Option<usize>,
    pub color: bool,
    pub unicode: bool,
    pub details: bool,
    pub host: Option<String>,
}

/// How the destination's audit ended: what its job made, or a diagnostic as safe as a local one.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    #[serde(rename_all = "camelCase")]
    Report {
        /// The window the destination scanned, which must be the one requested.
        since: Option<Millis>,
        /// The report's text.
        text: String,
        /// Whether the report recommends a change, which sets the exit code.
        change: bool,
        /// What the report adds to a total across hosts.
        totals: Totals,
        /// The changes the destination offers, when this host walks through them.
        offers: Option<Box<Offers>>,
    },
    /// The destination's answer to [`Answer::Review`].
    #[serde(rename_all = "camelCase")]
    Reviewed {
        /// The report again, each change marked as the walk answered it.
        report: String,
        /// Each file's text as the accepted changes would change it, for the confirm.
        preview: String,
    },
    /// The destination's answer to [`Answer::Write`]: the files it wrote.
    Wrote { written: Vec<Written> },
    #[serde(rename_all = "camelCase")]
    Sessions {
        /// The window the destination scanned, which must be the one requested.
        since: Option<Millis>,
        /// Its sessions, ordered by first request, each without a host.
        sessions: Vec<Session>,
    },
    Error {
        message: String,
        location: Option<Location>,
    },
}

/// What this host sends a destination after its report for a walk.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Answer {
    /// How the person answered the destination's offers, in order.
    Review { events: Vec<Event> },
    /// The person confirmed: write the reviewed changes.
    Write,
}

/// Write one JSON value followed by a newline.
pub fn write(writer: &mut impl Write, value: &impl Serialize) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value).map_err(std::io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Reads one stream's values as they arrive, without holding its serialized bytes.
pub struct Stream<R: Read> {
    json: serde_json::Deserializer<serde_json::de::IoRead<R>>,
}

impl<R: Read> Stream<R> {
    pub fn new(reader: R) -> Self {
        Self {
            json: serde_json::Deserializer::from_reader(reader),
        }
    }

    /// The header, which arrives before the destination starts scanning.
    pub fn header(&mut self) -> Result<Header, serde_json::Error> {
        Header::deserialize(&mut self.json)
    }

    /// The request, which the destination reads while it scans.
    pub fn request(&mut self) -> Result<Request, serde_json::Error> {
        Request::deserialize(&mut self.json)
    }

    /// The next value, where more may follow.
    pub fn value<T: for<'de> Deserialize<'de>>(&mut self) -> Result<T, serde_json::Error> {
        T::deserialize(&mut self.json)
    }

    /// Check that nothing but whitespace follows.
    pub fn end(&mut self) -> Result<(), serde_json::Error> {
        self.json.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::prices;

    fn request() -> Request {
        Request {
            catalog: prices(),
            job: Job::Report {
                max_cost_per_use_usd: 0.5,
                format: Format {
                    width: Some(80),
                    color: true,
                    unicode: false,
                    details: true,
                    host: Some("devbox".into()),
                },
                walk: false,
            },
        }
    }

    #[test]
    fn requests_carry_the_whole_catalog() {
        let mut bytes = vec![];
        write(&mut bytes, &request()).unwrap();
        let decoded = Stream::new(&bytes[..]).request().unwrap();
        assert_eq!(decoded.catalog, prices());
        assert!(matches!(
            decoded.job,
            Job::Report {
                max_cost_per_use_usd: 0.5,
                format: Format { host: Some(host), width: Some(80), .. },
                walk: false,
            } if host == "devbox"
        ));
    }

    #[test]
    fn outcomes_must_end_the_stream() {
        let mut bytes = vec![];
        write(
            &mut bytes,
            &Header {
                token_max_scan: build(),
            },
        )
        .unwrap();
        write(
            &mut bytes,
            &Outcome::Report {
                since: None,
                text: "report\n".into(),
                change: true,
                totals: Totals::default(),
                offers: None,
            },
        )
        .unwrap();
        let outcome = |stream: &mut Stream<&[u8]>| {
            let outcome: Outcome = stream.value()?;
            stream.end().map(|()| outcome)
        };
        let mut stream = Stream::new(&bytes[..]);
        assert_eq!(stream.header().unwrap().token_max_scan, build());
        assert!(matches!(
            outcome(&mut stream).unwrap(),
            Outcome::Report { text, change: true, .. } if text == "report\n"
        ));
        // Truncated, or followed by anything.
        let mut stream = Stream::new(&bytes[..bytes.len() - 3]);
        stream.header().unwrap();
        assert!(outcome(&mut stream).is_err());
        let trailing = [&bytes[..], b"{}"].concat();
        let mut stream = Stream::new(&trailing[..]);
        stream.header().unwrap();
        assert!(outcome(&mut stream).is_err());
    }
}
