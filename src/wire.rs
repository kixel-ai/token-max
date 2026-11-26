//! The private SSH audit stream between this host and a destination's
//! `token-max doctor --ssh-scan` or `token-max sessions --ssh-scan`.
//!
//! The destination writes a [`Header`]. For a report it then lists the Claude profiles whose
//! billing picks their main TTL's default, in [`Outcome::Profiles`], and waits for this host's
//! [`Paid`] answers, which arrive once the person has answered; a session export skips both.
//! It then starts scanning. This host answers with one [`Request`] on the destination's stdin,
//! carrying the price catalog and the [`Job`], and the destination analyzes its own scan and
//! writes one [`Outcome`]: its finished report, its priced session lines, or a diagnostic,
//! never the scan. Each value is one JSON line.
//!
//! A job for a walk also carries the destination's offers and leaves the stream open: this
//! host walks through them, sends the [`Answer`]s, and the destination replies to each with
//! another [`Outcome`], so its files are read and written only there. After `doctor`'s walk
//! the same stream carries `vibes`' walk, from the same scan, until [`Answer::Done`].

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::collections::IndexMap;
use crate::doctor::{Event, Offers, Style, Written};
use crate::model::{Billing, Millis};
use crate::prices::Catalog;
use crate::report::{Host, Listing, ReportOptions};
use crate::safe::ScanError;
use crate::sessions::Session;
use crate::totals::Totals;
use crate::vibes::Item;

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
            // Its `Credit` is a field of the totals a report carries.
            include_str!("defaults.rs"),
            // Its `Total` is a field of the totals a report carries, and its offers and
            // answers cross for a walk, with `mcp.rs`'s entries and `report.rs`'s choices.
            include_str!("doctor.rs"),
            include_str!("mcp.rs"),
            include_str!("report.rs"),
            // Its `ScanError` is the stream's error outcome.
            include_str!("safe.rs"),
            // Its items are what `vibes` carries.
            include_str!("vibes.rs"),
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

/// How each Claude profile a destination listed pays, by its root as listed, as the person
/// answered; a profile left out was skipped or never asked about.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Paid {
    pub profiles: IndexMap<String, Billing>,
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
    /// `token-max doctor`'s report, or `token-max vibes`' advice from the same analysis.
    #[serde(rename_all = "camelCase")]
    Report {
        max_cost_per_use_usd: f64,
        format: Format,
        /// Whether this host walks through the changes, which keeps the stream open.
        walk: bool,
        /// Answer with [`Outcome::Vibes`] instead of the report.
        vibes: bool,
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

impl Format {
    /// The report's layout, naming `home` as the host's home directory when `host` is set.
    pub fn report<'a>(&'a self, home: Option<&'a str>) -> ReportOptions<'a> {
        ReportOptions {
            width: self.width,
            color: self.color,
            unicode: self.unicode,
            details: self.details,
            host: self.host.as_deref().map(|name| Host { name, home }),
        }
    }

    /// The walk's layout, with paths shown from `home`.
    pub fn style(&self, home: Option<String>) -> Style {
        Style {
            color: self.color,
            unicode: self.unicode,
            width: self.width,
            home,
            host: self.host.clone(),
        }
    }
}

/// What the destination writes after its header: what its job made, or a diagnostic as safe as
/// a local one, and for a report first the profiles it asks about.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    /// A report's Claude profiles whose billing picks their main TTL's default; the
    /// destination waits for [`Paid`] before it scans.
    #[serde(rename_all = "camelCase")]
    Profiles {
        /// The destination's home directory, which its roots are shown from as `~`.
        home: Option<String>,
        roots: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    Report {
        /// The window the destination scanned, which must be the one requested.
        since: Option<Millis>,
        /// The report's lines and what it says around them, which this host joins with
        /// every other host's into one report.
        listing: Box<Listing>,
        /// Whether the report recommends a change, which sets the exit code.
        change: bool,
        /// What the report adds to a total across hosts.
        totals: Box<Totals>,
        /// The changes the destination offers, when this host walks through them.
        offers: Option<Box<Offers>>,
        /// The advice `vibes` has there, whose count `doctor` offers to show.
        vibes: Vec<Item>,
    },
    /// `vibes`' advice: the job's result, or the destination's answer to [`Answer::Vibes`].
    #[serde(rename_all = "camelCase")]
    Vibes {
        /// The window the destination scanned, which must be the one requested.
        since: Option<Millis>,
        items: Vec<Item>,
        /// The destination's home directory, which its files are named from.
        home: Option<String>,
        /// The changes the items name, checked against its files as they are now, when this
        /// host walks through them.
        offers: Option<Box<Offers>>,
    },
    /// The destination's answer to [`Answer::Review`], once it checked the answers against
    /// its files: each file's text as the accepted changes would change it, for the confirm.
    #[serde(rename_all = "camelCase")]
    Reviewed {
        preview: String,
    },
    /// The destination's answer to [`Answer::Write`]: the files it wrote.
    Wrote {
        written: Vec<Written>,
    },
    #[serde(rename_all = "camelCase")]
    Sessions {
        /// The window the destination scanned, which must be the one requested.
        since: Option<Millis>,
        /// Its sessions, ordered by first request, each without a host.
        sessions: Vec<Session>,
    },
    Error(ScanError),
}

/// What this host sends a destination after its report for a walk.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Answer {
    /// How the person answered the destination's offers, in order.
    Review { events: Vec<Event> },
    /// The person confirmed: write the reviewed changes.
    Write,
    /// `doctor`'s walk is over: offer `vibes`' changes next, from the files as they are now.
    Vibes,
    /// Nothing more follows: end the audit, which a closed stream would cancel instead.
    Done,
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
    /// Reads values from `reader`.
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
                vibes: false,
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
                vibes: false,
            } if host == "devbox"
        ));
    }

    #[test]
    fn errors_keep_their_location_and_the_format_its_settings() {
        let mut bytes = vec![];
        let error = ScanError::at(
            "bad line",
            crate::model::Location {
                file: "a.jsonl".into(),
                line: 7,
            },
        );
        write(&mut bytes, &Outcome::Error(error.clone())).unwrap();
        let outcome: Outcome = Stream::new(&bytes[..]).value().unwrap();
        assert!(matches!(outcome, Outcome::Error(sent) if sent == error));

        let Job::Report { format, .. } = request().job else {
            unreachable!()
        };
        let style = format.style(Some("/home/dev".into()));
        assert_eq!(
            (
                style.color,
                style.unicode,
                style.width,
                style.host.as_deref()
            ),
            (true, false, Some(80), Some("devbox"))
        );
        let report = format.report(Some("/home/dev"));
        assert_eq!(
            (
                report.details,
                report.host.map(|host| (host.name, host.home))
            ),
            (true, Some(("devbox", Some("/home/dev"))))
        );
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
                listing: Box::new(crate::report::tests::listing()),
                change: true,
                totals: Box::default(),
                offers: None,
                vibes: vec![],
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
            Outcome::Report { listing, change: true, .. }
                if listing.text() == crate::report::tests::listing().text()
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
