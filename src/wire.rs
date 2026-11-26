//! The private SSH audit stream between this host and a destination's
//! `token-max doctor --ssh-scan`, `token-max sessions --ssh-scan` or
//! `token-max audit --ssh-scan`.
//!
//! The destination writes a [`Header`]. For a report it then lists the Claude profiles whose
//! billing picks their main TTL's default, in [`Outcome::Profiles`], and waits for this host's
//! [`Paid`] answers, which arrive once the person has answered; a session export or an audit
//! skips both.
//! It then starts scanning. This host answers with one [`Request`] on the destination's stdin,
//! carrying the price catalog and the [`Job`], and the destination analyzes its own scan and
//! writes one [`Outcome`]: its finished report, its priced session lines, its audit's ranked
//! findings, or a diagnostic, never the scan. Each value is one JSON line.
//!
//! A job for a walk also carries the destination's offers and leaves the stream open: this
//! host walks through them, sends the [`Answer`]s, and the destination replies to each with
//! another [`Outcome`], so its files are read and written only there. After `doctor`'s walk
//! the same stream carries `vibes`' walk, from the same scan, until [`Answer::Done`].

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::audit::{Audit, Filter};
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
            include_str!("audit.rs"),
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
    /// `token-max audit`'s findings since `since` for the sessions `filter` selects, each
    /// ranking cut to `limit`. The scan starts earlier, at [`crate::audit::scanned`], so the
    /// first request in the window has its predecessor.
    #[serde(rename_all = "camelCase")]
    Audit {
        filter: Filter,
        limit: usize,
        since: Millis,
    },
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
    #[serde(rename_all = "camelCase")]
    Audit {
        /// The window the destination scanned, which must be the one requested.
        since: Option<Millis>,
        /// Its findings, each list ranked and cut to the job's limit, each pointer without a host.
        audit: Box<Audit>,
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

/// The most of a stream this host reads for one step, in bytes, and what that step reads.
///
/// A destination may be hostile, so each value this host reads from it, and the whole
/// stream, has one. Only a header and a newline have a size the protocol fixes; everything
/// else a destination sends grows with what it holds, so those limits are chosen far above
/// what a heavy host sends: on one with ten gigabytes of transcripts, a report is 170 KB and
/// every session it ever ran is 12 MB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    pub bytes: u64,
    /// How the step reads after "more than N", such as "for its report".
    pub what: &'static str,
}

impl Limit {
    /// A build's name is a version and a fingerprint, and a version has no length limit, so
    /// this allows a long one: another build's header must still be read to name it.
    pub const HEADER: Self = Self::new(1 << 10, "for its header");
    /// Paths of the profiles it found: 40 of the longest a path can be, escaped, or
    /// thousands of typical ones.
    pub const PROFILES: Self = Self::new(1 << 20, "for its profiles");
    pub const REPORT: Self = Self::new(16 << 20, "for its report");
    pub const ADVICE: Self = Self::new(16 << 20, "for its advice");
    /// Every session in the window, which over a long history is the largest value.
    pub const SESSIONS: Self = Self::new(256 << 20, "for its sessions");
    /// An audit's ranked lists, each cut to the limit this host asked for: well under 1 MB
    /// at the default of 10 entries, so this leaves room for a limit in the hundreds.
    pub const AUDIT: Self = Self::new(64 << 20, "for its audit");
    /// The changed lines of the files a walk would write, with their context.
    pub const PREVIEW: Self = Self::new(16 << 20, "for its preview");
    pub const WROTE: Self = Self::new(1 << 20, "for the files it wrote");
    /// The newline that ends the last value; nothing else may follow it.
    pub const END: Self = Self::new(1, "after its result");
    /// No limit, for a destination reading its coordinator, which is its own person's host.
    pub const NONE: Self = Self::new(u64::MAX, "");

    const fn new(bytes: u64, what: &'static str) -> Self {
        Self { bytes, what }
    }

    /// The sum of `steps`, for the whole stream of an audit that reads each once.
    pub const fn stream(steps: &[Self]) -> Self {
        let mut bytes = 0;
        let mut at = 0;
        while at < steps.len() {
            bytes += steps[at].bytes;
            at += 1;
        }
        Self::new(bytes, "in all")
    }
}

impl std::fmt::Display for Limit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let size = match self.bytes {
            1 => "1 byte".to_string(),
            b if b >= 1 << 20 && b % (1 << 20) == 0 => format!("{} MiB", b >> 20),
            b if b >= 1 << 10 && b % (1 << 10) == 0 => format!("{} KiB", b >> 10),
            b => format!("{b} bytes"),
        };
        write!(f, "{size} {}", self.what)
    }
}

/// Why a stream's value could not be read.
#[derive(Debug)]
pub enum Error {
    /// The sender passed this limit, for the value or the whole stream.
    Exceeded(Limit),
    Json(serde_json::Error),
}

impl Error {
    /// The stream ended before the value began.
    pub fn is_eof(&self) -> bool {
        matches!(self, Self::Json(error) if error.is_eof())
    }

    /// Reading the stream failed, not its contents.
    pub fn is_io(&self) -> bool {
        matches!(self, Self::Json(error) if error.is_io())
    }
}

/// What a stream may still deliver, shared with the reader the deserializer owns.
#[derive(Debug)]
struct Left {
    value: AtomicU64,
    stream: AtomicU64,
    exceeded: AtomicBool,
}

/// A reader that fails once it would pass what is [`Left`].
struct Budget<R> {
    reader: R,
    left: Arc<Left>,
}

impl<R: Read> Read for Budget<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let value = self.left.value.load(Ordering::Relaxed);
        let stream = self.left.stream.load(Ordering::Relaxed);
        let allowed = value.min(stream).min(buf.len() as u64) as usize;
        if allowed == 0 && !buf.is_empty() {
            // At the limit, only the end of the stream may follow.
            return match self.reader.read(&mut [0])? {
                0 => Ok(0),
                _ => {
                    self.left.exceeded.store(true, Ordering::Relaxed);
                    Err(std::io::Error::other("over the stream's limit"))
                }
            };
        }
        let read = self.reader.read(&mut buf[..allowed])?;
        self.left
            .value
            .store(value - read as u64, Ordering::Relaxed);
        self.left
            .stream
            .store(stream - read as u64, Ordering::Relaxed);
        Ok(read)
    }
}

/// Reads one stream's values as they arrive, without holding its serialized bytes, and
/// without reading more than each step's [`Limit`] or the stream's.
pub struct Stream<R: Read> {
    json: serde_json::Deserializer<serde_json::de::IoRead<Budget<R>>>,
    left: Arc<Left>,
    total: Limit,
}

impl<R: Read> Stream<R> {
    /// Reads values from `reader`, at most `total` in all.
    pub fn new(reader: R, total: Limit) -> Self {
        let left = Arc::new(Left {
            value: AtomicU64::new(0),
            stream: AtomicU64::new(total.bytes),
            exceeded: AtomicBool::new(false),
        });
        let budget = Budget {
            reader,
            left: left.clone(),
        };
        Self {
            json: serde_json::Deserializer::from_reader(budget),
            left,
            total,
        }
    }

    /// The next value, read within `limit`, where more may follow.
    pub fn value<T: for<'de> Deserialize<'de>>(&mut self, limit: Limit) -> Result<T, Error> {
        self.left.value.store(limit.bytes, Ordering::Relaxed);
        T::deserialize(&mut self.json).map_err(|error| self.error(error, limit))
    }

    /// Check that nothing but the last value's newline follows.
    pub fn end(&mut self) -> Result<(), Error> {
        self.left.value.store(Limit::END.bytes, Ordering::Relaxed);
        self.json
            .end()
            .map_err(|error| self.error(error, Limit::END))
    }

    fn error(&self, error: serde_json::Error, limit: Limit) -> Error {
        if !self.left.exceeded.load(Ordering::Relaxed) {
            return Error::Json(error);
        }
        match self.left.stream.load(Ordering::Relaxed) {
            0 => Error::Exceeded(self.total),
            _ => Error::Exceeded(limit),
        }
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
        let decoded = Stream::new(&bytes[..], Limit::NONE)
            .value::<Request>(Limit::NONE)
            .unwrap();
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
        let outcome: Outcome = Stream::new(&bytes[..], Limit::NONE)
            .value(Limit::NONE)
            .unwrap();
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
            let outcome: Outcome = stream.value(Limit::REPORT)?;
            stream.end().map(|()| outcome)
        };
        let header = |stream: &mut Stream<&[u8]>| stream.value::<Header>(Limit::HEADER).unwrap();
        let mut stream = Stream::new(&bytes[..], Limit::NONE);
        assert_eq!(header(&mut stream).token_max_scan, build());
        assert!(matches!(
            outcome(&mut stream).unwrap(),
            Outcome::Report { listing, change: true, .. }
                if listing.text() == crate::report::tests::listing().text()
        ));
        // Truncated, or followed by anything.
        let mut stream = Stream::new(&bytes[..bytes.len() - 3], Limit::NONE);
        header(&mut stream);
        assert!(outcome(&mut stream).is_err());
        let trailing = [&bytes[..], b"{}"].concat();
        let mut stream = Stream::new(&trailing[..], Limit::NONE);
        header(&mut stream);
        assert!(outcome(&mut stream).is_err());
    }

    #[test]
    fn each_value_and_the_stream_stop_at_their_limit() {
        let mut bytes = vec![];
        write(&mut bytes, &Answer::Write).unwrap();
        write(&mut bytes, &Answer::Done).unwrap();
        // `"write"`, then a newline and `"done"`.
        let (first, second) = (7, 7);
        let limit = |bytes| Limit {
            bytes,
            what: "for it",
        };
        let read = |total, a, b| {
            let mut stream = Stream::new(&bytes[..], limit(total));
            let first = stream.value::<Answer>(limit(a));
            let second = first.is_ok().then(|| stream.value::<Answer>(limit(b)));
            (first, second.map(|r| r.and_then(|_| stream.end())))
        };
        assert!(matches!(
            read(15, first, second),
            (Ok(Answer::Write), Some(Ok(())))
        ));
        assert!(matches!(
            read(15, first - 1, second),
            (Err(Error::Exceeded(Limit { bytes: 6, .. })), None)
        ));
        assert!(matches!(
            read(15, first, second - 1),
            (Ok(_), Some(Err(Error::Exceeded(Limit { bytes: 6, .. }))))
        ));
        // The stream's limit, with every value within its own.
        let total = Stream::new(&bytes[..], limit(13)).total;
        assert!(matches!(
            read(13, first, second),
            (Ok(_), Some(Err(Error::Exceeded(t)))) if t == total
        ));
        // Anything after the last newline passes the end's limit.
        let trailing = [&bytes[..], b" "].concat();
        let mut stream = Stream::new(&trailing[..], Limit::NONE);
        stream.value::<Answer>(Limit::NONE).unwrap();
        stream.value::<Answer>(Limit::NONE).unwrap();
        assert!(matches!(stream.end(), Err(Error::Exceeded(Limit::END))));
        assert_eq!(Limit::SESSIONS.to_string(), "256 MiB for its sessions");
        assert_eq!(Limit::END.to_string(), "1 byte after its result");
    }
}
