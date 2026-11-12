//! The private SSH audit stream between this host and a destination's `token-max --ssh-scan`.
//!
//! The destination writes a [`Header`] and starts scanning. This host answers with one
//! [`Request`] on the destination's stdin, carrying the price catalog and report format, and
//! the destination analyzes its own scan and writes one [`Outcome`]: its finished report or a
//! diagnostic, never the scan. Each value is one JSON line.

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{Location, Millis};
use crate::prices::Catalog;

/// This build's identity: the package version and a fingerprint of the types a stream carries,
/// the price catalog and report included.
///
/// Unpublished builds share a version, so the fingerprint keeps two different builds of the
/// same version from misreading each other's streams.
pub fn build() -> String {
    let schema = Sha256::digest(
        [
            include_str!("wire.rs"),
            include_str!("prices.rs"),
            include_str!("model.rs"),
        ]
        .concat(),
    );
    let hex: String = schema[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("{}+{hex}", env!("CARGO_PKG_VERSION"))
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
    pub max_cost_per_use_usd: f64,
    pub format: Format,
}

/// How a host's report is printed.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Format {
    /// Text for this host's terminal, under the host's name when the run covers several.
    #[serde(rename_all = "camelCase")]
    Text {
        width: Option<usize>,
        color: bool,
        unicode: bool,
        details: bool,
        host: Option<String>,
    },
    /// The JSON report.
    Json,
}

/// One host's report as printed.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Printed {
    Text(String),
    Json(serde_json::Value),
}

/// How the destination's audit ended: its report, or a diagnostic as safe as a local one.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    #[serde(rename_all = "camelCase")]
    Report {
        /// The window the destination scanned, which must be the one requested.
        since: Option<Millis>,
        printed: Printed,
        /// Whether the report recommends a change, which sets the exit code.
        change: bool,
    },
    Error {
        message: String,
        location: Option<Location>,
    },
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

    /// The outcome, which must end the stream.
    pub fn outcome(&mut self) -> Result<Outcome, serde_json::Error> {
        let outcome = Outcome::deserialize(&mut self.json)?;
        self.json.end().map(|()| outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::prices;

    fn request() -> Request {
        Request {
            catalog: prices(),
            max_cost_per_use_usd: 0.5,
            format: Format::Text {
                width: Some(80),
                color: true,
                unicode: false,
                details: true,
                host: Some("devbox".into()),
            },
        }
    }

    #[test]
    fn requests_carry_the_whole_catalog() {
        let mut bytes = vec![];
        write(&mut bytes, &request()).unwrap();
        let decoded = Stream::new(&bytes[..]).request().unwrap();
        assert_eq!(decoded.catalog, prices());
        assert_eq!(decoded.max_cost_per_use_usd, 0.5);
        assert!(matches!(
            decoded.format,
            Format::Text { host: Some(host), width: Some(80), .. } if host == "devbox"
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
                printed: Printed::Text("report\n".into()),
                change: true,
            },
        )
        .unwrap();
        let mut stream = Stream::new(&bytes[..]);
        assert_eq!(stream.header().unwrap().token_max_scan, build());
        assert!(matches!(
            stream.outcome().unwrap(),
            Outcome::Report { printed: Printed::Text(text), change: true, .. } if text == "report\n"
        ));
        // Truncated, or followed by anything.
        let mut stream = Stream::new(&bytes[..bytes.len() - 3]);
        stream.header().unwrap();
        assert!(stream.outcome().is_err());
        let trailing = [&bytes[..], b"{}"].concat();
        let mut stream = Stream::new(&trailing[..]);
        stream.header().unwrap();
        assert!(stream.outcome().is_err());
    }
}
