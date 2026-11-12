//! The private SSH audit stream: a remote `token-max --ssh-scan` sends its scan as JSON.
//!
//! A stream is a [`Header`], then one [`Outcome`], each a JSON value. Strings, identifier
//! lists, exposure views, result totals and message digests shared in memory are sent once and
//! then named by index, so the stream stays about as small as the scan it carries. The JSON
//! report serializes the same types without sharing.

use crate::collections::HashMap;
use std::cell::RefCell;
use std::fmt;
use std::io::{Read, Write};
use std::sync::Arc;

use serde::de::value::MapAccessDeserializer;
use serde::de::{self, DeserializeOwned, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{Exposure, ExposureView, Ids, Location, ResultTotals, Scan, Text};

/// This build's identity: the package version and a fingerprint of the types a stream carries.
///
/// Unpublished builds share a version, so the fingerprint keeps two different builds of the
/// same version from misreading each other's scans.
pub fn build() -> String {
    let schema = Sha256::digest([include_str!("model.rs"), include_str!("wire.rs")].concat());
    let hex: String = schema[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("{}+{hex}", env!("CARGO_PKG_VERSION"))
}

/// The first value of every stream, written before the scan starts.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    /// The sender's [`build`]; a coordinator reads streams only from its own build.
    pub token_max_scan: String,
    /// The sender's home directory, which its report abbreviates to `~`.
    pub home: Option<String>,
}

/// How the remote scan ended: its metadata, or a diagnostic as safe as a local one.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    Scan(Box<Scan>),
    Error {
        message: String,
        location: Option<Location>,
    },
}

/// Write one JSON value followed by a newline, sharing values within it.
pub fn write(writer: &mut impl Write, value: &impl Serialize) -> std::io::Result<()> {
    ENCODER.set(Some(Encoder::default()));
    let result = serde_json::to_writer(&mut *writer, value);
    ENCODER.set(None);
    result.map_err(std::io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Reads one stream as it arrives, without holding its serialized bytes.
pub struct Stream<R: Read> {
    json: serde_json::Deserializer<serde_json::de::IoRead<R>>,
}

impl<R: Read> Stream<R> {
    pub fn new(reader: R) -> Self {
        Self {
            json: serde_json::Deserializer::from_reader(reader),
        }
    }

    /// The header, which arrives before the remote starts scanning.
    pub fn header(&mut self) -> Result<Header, serde_json::Error> {
        Header::deserialize(&mut self.json)
    }

    /// The outcome, which must end the stream.
    pub fn outcome(&mut self) -> Result<Outcome, serde_json::Error> {
        let outcome = decode(&mut self.json);
        outcome.and_then(|outcome| self.json.end().map(|()| outcome))
    }
}

fn decode<'de, T: DeserializeOwned>(
    json: impl Deserializer<'de, Error = serde_json::Error>,
) -> Result<T, serde_json::Error> {
    DECODER.set(Some(Decoder::default()));
    let value = T::deserialize(json);
    DECODER.set(None);
    value
}

// ---------------------------------------------------------------------------
// Shared values.

#[derive(Clone, Copy)]
enum Table {
    Text,
    Ids,
    View,
    Results,
}

/// Each table maps a shared value's address to its index, assigned once it has been written.
#[derive(Default)]
struct Encoder {
    tables: [HashMap<usize, u64>; 4],
}

/// Values in the order the encoder indexed them.
#[derive(Default)]
struct Decoder {
    text: Vec<Text>,
    ids: Vec<Ids>,
    views: Vec<Arc<ExposureView>>,
    results: Vec<Arc<ResultTotals>>,
}

thread_local! {
    static ENCODER: RefCell<Option<Encoder>> = const { RefCell::new(None) };
    static DECODER: RefCell<Option<Decoder>> = const { RefCell::new(None) };
}

fn encoding() -> bool {
    ENCODER.with_borrow(Option::is_some)
}

/// Serialize a shared value in full the first time a stream carries it and by index after.
///
/// An index is assigned only after the value's own shared parts, in the same order a decoder
/// finishes reading them. Outside a stream this is `value` unchanged.
fn shared<S: Serializer>(
    table: Table,
    address: usize,
    serializer: S,
    value: impl FnOnce(S) -> Result<S::Ok, S::Error>,
) -> Result<S::Ok, S::Error> {
    match sent(table, address) {
        None => value(serializer),
        Some(Some(index)) => serializer.serialize_u64(index),
        Some(None) => {
            let ok = value(serializer)?;
            index(table, address);
            Ok(ok)
        }
    }
}

/// The index of a value this stream already sent, or None outside a stream.
fn sent(table: Table, address: usize) -> Option<Option<u64>> {
    ENCODER.with_borrow(|e| {
        e.as_ref()
            .map(|e| e.tables[table as usize].get(&address).copied())
    })
}

/// Assign the next index to a value just sent.
fn index(table: Table, address: usize) {
    ENCODER.with_borrow_mut(|e| {
        let table = &mut e.as_mut().expect("encoding").tables[table as usize];
        let index = table.len() as u64;
        table.insert(address, index);
    });
}

/// A value decoded earlier in this stream.
fn earlier<T: Clone, E: de::Error>(
    index: u64,
    pick: impl FnOnce(&Decoder) -> &Vec<T>,
) -> Result<T, E> {
    DECODER.with_borrow(|d| {
        let d = d
            .as_ref()
            .ok_or_else(|| E::custom("shared value outside a stream"))?;
        usize::try_from(index)
            .ok()
            .and_then(|i| pick(d).get(i).cloned())
            .ok_or_else(|| E::custom("reference to an unsent shared value"))
    })
}

/// Index a newly decoded value.
fn remember<T: Clone, E: de::Error>(
    value: T,
    pick: impl FnOnce(&mut Decoder) -> &mut Vec<T>,
) -> Result<T, E> {
    DECODER.with_borrow_mut(|d| {
        let d = d
            .as_mut()
            .ok_or_else(|| E::custom("shared value outside a stream"))?;
        pick(d).push(value.clone());
        Ok(value)
    })
}

/// A shared string, sent once per stream.
struct SharedText<'a>(&'a Text);

impl Serialize for SharedText<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let address = Arc::as_ptr(self.0) as *const u8 as usize;
        shared(Table::Text, address, serializer, |s| {
            s.serialize_str(self.0)
        })
    }
}

/// `#[serde(with)]` for a shared string, such as a transcript path.
pub mod text {
    use super::*;

    pub fn serialize<S: Serializer>(text: &Text, serializer: S) -> Result<S::Ok, S::Error> {
        SharedText(text).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Text, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Text;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string or an earlier string's index")
            }
            fn visit_u64<E: de::Error>(self, index: u64) -> Result<Text, E> {
                earlier(index, |d| &d.text)
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Text, E> {
                remember(Text::from(value), |d| &mut d.text)
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// `#[serde(with)]` for an optional shared string.
pub mod opt_text {
    use super::*;

    pub fn serialize<S: Serializer>(text: &Option<Text>, serializer: S) -> Result<S::Ok, S::Error> {
        text.as_ref().map(SharedText).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Text>, D::Error> {
        Option::<Element>::deserialize(deserializer).map(|e| e.map(|e| e.0))
    }
}

/// `#[serde(with)]` for a list of shared strings.
pub mod texts {
    use super::*;

    pub fn serialize<S: Serializer>(texts: &[Text], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(texts.iter().map(SharedText))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Text>, D::Error> {
        Vec::<Element>::deserialize(deserializer).map(|e| e.into_iter().map(|e| e.0).collect())
    }
}

/// A shared string read as a list element or optional value.
struct Element(Text);

impl<'de> Deserialize<'de> for Element {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        text::deserialize(d).map(Element)
    }
}

/// `#[serde(with)]` for a shared identifier list.
pub mod ids {
    use super::*;

    pub fn serialize<S: Serializer>(ids: &Ids, serializer: S) -> Result<S::Ok, S::Error> {
        ids.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Ids, D::Error> {
        Ids::deserialize(deserializer)
    }
}

/// A shared exposure view.
struct SharedView<'a>(&'a Arc<ExposureView>);

impl Serialize for SharedView<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let address = Arc::as_ptr(self.0) as usize;
        shared(Table::View, address, serializer, |s| self.0.serialize(s))
    }
}

fn view<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Arc<ExposureView>, D::Error> {
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Arc<ExposureView>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an exposure view or an earlier view's index")
        }
        fn visit_u64<E: de::Error>(self, index: u64) -> Result<Self::Value, E> {
            earlier(index, |d| &d.views)
        }
        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
            let view = ExposureView::deserialize(MapAccessDeserializer::new(map))?;
            remember(Arc::new(view), |d| &mut d.views)
        }
    }
    deserializer.deserialize_any(V)
}

/// Tool-output totals shared while the conversation has no new results.
struct SharedResults(Arc<ResultTotals>);

impl Serialize for SharedResults {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let address = Arc::as_ptr(&self.0) as usize;
        shared(Table::Results, address, serializer, |s| self.0.serialize(s))
    }
}

impl<'de> Deserialize<'de> for SharedResults {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Arc<ResultTotals>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("tool-output totals or earlier totals' index")
            }
            fn visit_u64<E: de::Error>(self, index: u64) -> Result<Self::Value, E> {
                earlier(index, |d| &d.results)
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                let results = ResultTotals::deserialize(MapAccessDeserializer::new(map))?;
                remember(Arc::new(results), |d| &mut d.results)
            }
        }
        deserializer.deserialize_any(V).map(SharedResults)
    }
}

impl Serialize for Exposure {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if encoding() {
            let mut s = serializer.serialize_struct("Exposure", 3)?;
            s.serialize_field("request", &SharedText(&self.request))?;
            s.serialize_field("view", &SharedView(&self.view))?;
            s.serialize_field("results", &self.results.clone().map(SharedResults))?;
            return s.end();
        }
        #[derive(Serialize)]
        struct Flat<'a> {
            request: &'a str,
            #[serde(flatten)]
            view: &'a ExposureView,
        }
        Flat {
            request: &self.request,
            view: &self.view,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Exposure {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(with = "text")]
            request: Text,
            #[serde(deserialize_with = "view")]
            view: Arc<ExposureView>,
            results: Option<SharedResults>,
        }
        let Wire {
            request,
            view,
            results,
        } = Wire::deserialize(deserializer)?;
        Ok(Exposure {
            request,
            view,
            results: results.map(|r| r.0),
        })
    }
}

impl Serialize for Ids {
    /// A flat list in the report. In a stream, an index once sent, and otherwise every unsent
    /// list in its chain, oldest first, each as its base's index, the length it keeps from
    /// that base and its own tail. A chain can be thousands of requests long, so it is sent
    /// as one array rather than nested.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let index = match sent(Table::Ids, self.address()) {
            None => return serializer.collect_seq(self.iter()),
            Some(index) => index,
        };
        if let Some(index) = index {
            return serializer.serialize_u64(index);
        }
        let mut chain = vec![self];
        while let Some((base, _)) = chain[chain.len() - 1].base() {
            if sent(Table::Ids, base.address()) != Some(None) {
                break;
            }
            chain.push(base);
        }
        chain.reverse();
        serializer.collect_seq(chain.into_iter().map(Node))
    }
}

/// One list of an unsent chain, whose base was sent before it.
struct Node<'a>(&'a Ids);

impl Serialize for Node<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        struct Tail<'a>(&'a [Text]);
        impl Serialize for Tail<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_seq(self.0.iter().map(SharedText))
            }
        }
        let (base, keep) = match self.0.base() {
            Some((base, keep)) => {
                let index = sent(Table::Ids, base.address()).flatten();
                (Some(index.expect("bases are sent first")), keep)
            }
            None => (None, 0),
        };
        let mut node = serializer.serialize_struct("Ids", 3)?;
        node.serialize_field("base", &base)?;
        node.serialize_field("keep", &keep)?;
        node.serialize_field("tail", &Tail(self.0.tail()))?;
        let ok = node.end()?;
        index(Table::Ids, self.0.address());
        Ok(ok)
    }
}

impl<'de> Deserialize<'de> for Ids {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Node {
            base: Option<u64>,
            keep: usize,
            tail: Vec<Element>,
        }
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Ids;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a digest chain or an earlier list's index")
            }
            fn visit_u64<E: de::Error>(self, index: u64) -> Result<Ids, E> {
                earlier(index, |d| &d.ids)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Ids, A::Error> {
                let mut last = None;
                while let Some(Node { base, keep, tail }) = seq.next_element()? {
                    let tail = tail.into_iter().map(|e| e.0).collect();
                    let base: Option<Ids> =
                        base.map(|index| earlier(index, |d| &d.ids)).transpose()?;
                    let digests = match &base {
                        Some(base) if keep > 0 && keep <= base.len() => {
                            Ids::new(Some((base, keep)), tail)
                        }
                        None if keep == 0 => Ids::new(None, tail),
                        _ => return Err(de::Error::custom("invalid digest prefix length")),
                    };
                    last = Some(remember(digests, |d| &mut d.ids)?);
                }
                last.ok_or_else(|| de::Error::custom("empty digest chain"))
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Linkage, References, TextCoverage};
    use crate::reader::scan;
    use crate::test_support::Workspace;

    fn ids(names: &[&str]) -> Ids {
        names.iter().map(|n| Text::from(*n)).collect()
    }

    fn view(items: Ids) -> Arc<ExposureView> {
        Arc::new(ExposureView {
            components: ids(&["tool", "system"]),
            candidates: ids(&[]),
            removed: ids(&[]),
            inherited: ids(&[]),
            deferred: ids(&[]),
            linkage: Linkage::Wire,
            coverage: TextCoverage::CompleteText,
            references: Some(References {
                base: None,
                keep: Some(0),
                tail: items.tail().to_vec().into(),
                items: Some(items),
            }),
            complete_characters: Some(10),
            prefix: 1,
            wire_model: None,
            reasons: [].into(),
        })
    }

    /// A scan of the synthetic transcripts with shared views and a digest chain added.
    fn sample() -> Scan {
        let w = Workspace::new();
        w.fixture(w.claude.join("projects/project/main.jsonl"), "claude.jsonl");
        w.fixture(w.codex.join("sessions/rollout.jsonl"), "codex.jsonl");
        let mut scan = scan(&w.scan_options()).unwrap();
        let first = Ids::from(vec!["a".into(), "b".into()]);
        let second = Ids::new(Some((&first, 1)), vec!["c".into()].into());
        let shared = view(second);
        let results = Arc::new(ResultTotals {
            epoch: 17,
            tools: vec![("Bash".into(), 42, false), ("mcp__docs".into(), 0, true)],
        });
        scan.exposure.requests = vec![
            Exposure {
                request: "r1".into(),
                view: shared.clone(),
                results: Some(results.clone()),
            },
            Exposure {
                request: "r2".into(),
                view: shared,
                results: Some(results),
            },
            Exposure {
                request: "r3".into(),
                view: view(first),
                results: None,
            },
        ];
        scan
    }

    fn encode(scan: Scan) -> Vec<u8> {
        let mut bytes = vec![];
        let header = Header {
            token_max_scan: build(),
            home: Some("/home/user".into()),
        };
        write(&mut bytes, &header).unwrap();
        write(&mut bytes, &Outcome::Scan(Box::new(scan))).unwrap();
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Scan, serde_json::Error> {
        let mut stream = Stream::new(bytes);
        assert_eq!(stream.header()?.token_max_scan, build());
        match stream.outcome()? {
            Outcome::Scan(scan) => Ok(*scan),
            Outcome::Error { .. } => panic!("expected a scan"),
        }
    }

    #[test]
    fn scans_round_trip_with_shared_values_sent_once() {
        let scan = sample();
        assert!(!scan.requests.is_empty());
        let report = serde_json::to_string(&scan).unwrap();
        let bytes = encode(scan.clone());
        let text = String::from_utf8(bytes.clone()).unwrap();
        // The report form is unchanged, and the stream carries no transcript text.
        assert_eq!(serde_json::to_string(&scan).unwrap(), report);
        assert!(!text.contains("SYNTHETIC_PRIVATE"));
        assert!(!report.contains("\"epoch\""));
        assert_eq!(text.matches("\"epoch\":17").count(), 1);
        // Native session IDs, which only the local session export prints, stay on the host.
        assert!(!text.contains("session-a") && !scan.sessions.is_empty());
        let decoded = decode(&bytes).unwrap();
        assert!(decoded.sessions.is_empty());
        let scan = Scan {
            sessions: Default::default(),
            ..scan
        };
        assert_eq!(decoded, scan);
        let views = &decoded.exposure.requests;
        assert!(Arc::ptr_eq(&views[0].view, &views[1].view));
        assert!(Arc::ptr_eq(
            views[0].results.as_ref().unwrap(),
            views[1].results.as_ref().unwrap()
        ));
        let items = |n: usize| {
            views[n]
                .view
                .references
                .as_ref()
                .unwrap()
                .items
                .clone()
                .unwrap()
        };
        assert_eq!(items(0).base().unwrap().0.address(), items(2).address());
        // Locations share their path, which the report repeats and the stream sends once.
        let file = &scan.requests[0].locations[0].file;
        let quoted = serde_json::to_string(&**file).unwrap();
        assert!(text.matches(&quoted).count() < report.matches(&quoted).count());
    }

    #[test]
    fn long_unsent_digest_chains_are_not_nested() {
        let mut scan = sample();
        scan.sessions = Default::default();
        let mut items = Ids::from(vec!["0".into()]);
        for n in 1..1000 {
            items = Ids::new(Some((&items, n)), vec![n.to_string().into()].into());
        }
        scan.exposure.requests.push(Exposure {
            request: "late".into(),
            view: view(items),
            results: None,
        });
        assert_eq!(decode(&encode(scan.clone())).unwrap(), scan);
    }

    #[test]
    fn corrupt_streams_fail() {
        let bytes = encode(sample());
        // Truncated, followed by anything, or naming a value it never sent.
        assert!(decode(&bytes[..bytes.len() - 2]).is_err());
        assert!(decode(&[&bytes[..], b"{}"].concat()).is_err());
        let text = String::from_utf8(bytes).unwrap();
        let unsent = text.replacen("\"view\":0", "\"view\":99", 1);
        assert_ne!(unsent, text);
        assert!(decode(unsent.as_bytes()).is_err());
        let unsent = text.replacen("\"results\":0", "\"results\":99", 1);
        assert_ne!(unsent, text);
        assert!(decode(unsent.as_bytes()).is_err());
        // A digest list cannot keep more than its base holds.
        let chain = text.replacen("\"keep\":1,", "\"keep\":5,", 1);
        assert_ne!(chain, text);
        assert!(decode(chain.as_bytes()).is_err());
    }
}
