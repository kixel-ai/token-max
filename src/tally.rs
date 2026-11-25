//! Count the requests a scan has read so far, for a progress line: kept beside the scan's
//! requests and brought up to date as they change, so the line counts without a pass over
//! everything read.
//!
//! The counts follow the report header's rules on what the read loop knows, so they near the
//! header's as reading ends; the analysis then replaces them with the header's own.

use std::collections::{BTreeMap, VecDeque};

use crate::collections::{HashMap, HashSet, IndexMap};
use crate::model::{Estimate, Harness, Millis, Profile, Profiles, Request, Subagent, Text, Usd};
use crate::prices::{Catalog, Sample, price_request};
use crate::totals::{Counts, Spent};

/// What one request adds to the counts.
struct Counted {
    tokens: u64,
    session: Text,
    /// The profiles that share its history, sorted, as the header groups its spend.
    group: Profiles,
    /// What it costs at API prices, once prices load.
    cost: Option<Cost>,
}

/// A request's price: whole, or only what is known of it.
#[derive(Clone, Copy)]
enum Cost {
    Priced(Usd),
    Known(Usd),
}

/// The requests of one group of profiles, whose spend is extrapolated over its unpriced ones.
#[derive(Default)]
struct Group {
    requests: u64,
    priced: Sample,
    /// What the unpriced requests are known to cost, in all and how many cost each amount.
    known: Usd,
    floors: BTreeMap<Usd, u64>,
}

impl Group {
    /// What the group costs, as the header estimates it: each unpriced request at the priced
    /// mean, or what it is known to cost when that is more; None when none was priced.
    fn estimate(&self) -> Option<Estimate> {
        let e = self.priced.estimate(self.requests)?;
        let mean = e.usd / self.requests as f64;
        let raise: f64 = self
            .floors
            .iter()
            .rev()
            .map(|(k, &n)| (k.to_f64(), n))
            .take_while(|&(k, _)| k > mean)
            .map(|(k, n)| (k - mean) * n as f64)
            .sum();
        Some(Estimate {
            usd: e.usd + raise,
            ..e
        })
    }
}

/// The running counts over a scan's requests, by their position in its list, and over its
/// legacy Codex records, which become requests once reading ends unless a modern record of
/// the same total supersedes them.
#[derive(Default)]
pub(crate) struct Tally {
    /// What each request adds, None for one outside the window.
    counted: Vec<Option<Counted>>,
    /// Requests changed since the counts were last brought up to date, each once.
    changed: Vec<usize>,
    stale: Vec<bool>,
    /// What each legacy record adds, by its position in the scan's list of them; None for
    /// one not counted.
    legacy: Vec<Option<Counted>>,
    /// The legacy records in the window by request id, in the order read, less those found
    /// superseded, which stay so. Copies of one id become one request, so only one is
    /// counted: the first no modern record supersedes.
    copies: HashMap<Text, VecDeque<usize>>,
    /// The copy counted for each id.
    claimed: HashMap<Text, usize>,
    /// The counted legacy records by the total a modern record would supersede them with.
    totals: HashMap<String, Vec<usize>>,
    /// Totals modern records recorded since the counts were last brought up to date.
    superseded: Vec<String>,
    requests: u64,
    tokens: u64,
    /// Counted requests per session, whose roots are counted again when a session comes or
    /// goes or a subagent's lineage is recorded.
    sessions: HashMap<Text, u64>,
    sessions_changed: bool,
    subagents_seen: usize,
    roots: u64,
    groups: IndexMap<Profiles, Group>,
    /// Each history's profiles, sorted.
    sorted: HashMap<Profiles, Profiles>,
    /// Whether the counts hold prices.
    priced: bool,
}

/// What a tally reads its requests with.
pub(crate) struct Basis<'a> {
    pub since: Option<Millis>,
    /// The totals modern Codex records have recorded, which supersede legacy records.
    pub modern: &'a HashSet<String>,
    pub subagents: &'a IndexMap<Text, Subagent>,
    pub profiles: &'a [Profile],
    /// Public prices, once loaded.
    pub catalog: Option<&'a Catalog>,
}

impl Tally {
    /// Note that the request at `index` was added or changed.
    pub(crate) fn touch(&mut self, index: usize) {
        if index >= self.stale.len() {
            self.stale.resize(index + 1, false);
            self.counted.resize_with(index + 1, || None);
        }
        if !self.stale[index] {
            self.stale[index] = true;
            self.changed.push(index);
        }
    }

    /// Note that a modern record recorded `total`, which supersedes the legacy records of it.
    pub(crate) fn supersede(&mut self, total: &str) {
        if self.totals.contains_key(total) {
            self.superseded.push(total.to_string());
        }
    }

    /// Take back every legacy record, once the scan has made requests of those it keeps.
    pub(crate) fn forget_legacy(&mut self) {
        for counted in std::mem::take(&mut self.legacy).into_iter().flatten() {
            self.apply(&counted, false);
        }
        self.copies.clear();
        self.claimed.clear();
        self.totals.clear();
        self.superseded.clear();
    }

    /// Bring the counts up to date with `requests` and the `legacy` records, each with the
    /// total that would supersede it, and return them. Once prices load, every request read
    /// so far is priced, then each one as it changes.
    pub(crate) fn count(
        &mut self,
        requests: &mut [Request],
        legacy: &mut [(Request, String)],
        basis: &Basis<'_>,
    ) -> Counts {
        if !self.priced && basis.catalog.is_some() {
            self.priced = true;
            for index in 0..requests.len() {
                self.touch(index);
            }
            for index in self.claimed.values().copied().collect::<Vec<_>>() {
                if let Some(old) = self.legacy[index].take() {
                    self.apply(&old, false);
                }
                let counted = self.measure(&mut legacy[index].0, basis);
                self.apply(&counted, true);
                self.legacy[index] = Some(counted);
            }
        }
        self.count_legacy(legacy, basis);
        for index in std::mem::take(&mut self.changed) {
            self.stale[index] = false;
            if let Some(old) = self.counted[index].take() {
                self.apply(&old, false);
            }
            let r = &mut requests[index];
            if basis.since.is_some_and(|since| r.time < since) {
                continue;
            }
            let counted = self.measure(r, basis);
            self.apply(&counted, true);
            self.counted[index] = Some(counted);
        }
        if self.sessions_changed || self.subagents_seen != basis.subagents.len() {
            let roots: HashSet<&Text> = self
                .sessions
                .keys()
                .map(|session| root(session, basis.subagents))
                .collect();
            self.roots = roots.len() as u64;
            self.sessions_changed = false;
            self.subagents_seen = basis.subagents.len();
        }
        Counts {
            requests: self.requests,
            tokens: self.tokens,
            sessions: self.roots,
            spend: self.priced.then(|| {
                Spent::estimated(
                    self.groups
                        .values()
                        .map(|g| (g.estimate(), g.known.to_f64())),
                )
            }),
        }
    }

    /// Count the legacy records read since the last count, and recount the ids whose counted
    /// copy a modern record has superseded since.
    fn count_legacy(&mut self, legacy: &mut [(Request, String)], basis: &Basis<'_>) {
        let mut ids: Vec<Text> = vec![];
        let read = self.legacy.len();
        for (index, (r, _)) in legacy.iter().enumerate().skip(read) {
            self.legacy.push(None);
            if basis.since.is_some_and(|since| r.time < since) {
                continue;
            }
            self.copies
                .entry(r.id.clone())
                .or_default()
                .push_back(index);
            if !self.claimed.contains_key(&r.id) {
                ids.push(r.id.clone());
            }
        }
        for total in std::mem::take(&mut self.superseded) {
            for index in self.totals.remove(&total).unwrap_or_default() {
                if let Some(old) = self.legacy[index].take() {
                    self.apply(&old, false);
                }
                let id = legacy[index].0.id.clone();
                self.claimed.remove(&id);
                ids.push(id);
            }
        }
        for id in ids {
            if self.claimed.contains_key(&id) {
                continue;
            }
            let Some(copies) = self.copies.get_mut(&id) else {
                continue;
            };
            while copies
                .front()
                .is_some_and(|&index| basis.modern.contains(&legacy[index].1))
            {
                copies.pop_front();
            }
            let Some(&index) = copies.front() else {
                continue;
            };
            let counted = self.measure(&mut legacy[index].0, basis);
            self.apply(&counted, true);
            self.legacy[index] = Some(counted);
            self.claimed.insert(id, index);
            self.totals
                .entry(legacy[index].1.clone())
                .or_default()
                .push(index);
        }
    }

    /// What `r` adds to the counts.
    fn measure(&mut self, r: &mut Request, basis: &Basis<'_>) -> Counted {
        let group = self
            .sorted
            .entry(r.profiles.clone())
            .or_insert_with(|| {
                let mut sorted = r.profiles.to_vec();
                // As the header's groups sort them, by UTF-16 code units.
                sorted.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
                sorted.into()
            })
            .clone();
        let cost = basis.catalog.map(|catalog| {
            let cost = match provider(r, basis.profiles) {
                // Priced as the scan will fill it in once reading ends, then put back.
                Some(provider) => {
                    let mut meta = (*r.meta).clone();
                    meta.provider = Some(provider);
                    let read = std::mem::replace(&mut r.meta, meta.into());
                    let cost = price_request(catalog, r);
                    r.meta = read;
                    cost
                }
                None => price_request(catalog, r),
            };
            match cost.total_usd {
                Some(usd) => Cost::Priced(usd),
                None => Cost::Known(cost.known_usd),
            }
        });
        Counted {
            tokens: crate::totals::tokens(r),
            session: r.session.clone(),
            group,
            cost,
        }
    }

    /// Add `counted` to the counts, or take it back out.
    fn apply(&mut self, counted: &Counted, add: bool) {
        let group = self.groups.entry(counted.group.clone()).or_default();
        if add {
            self.requests += 1;
            self.tokens += counted.tokens;
            let n = self.sessions.entry(counted.session.clone()).or_default();
            *n += 1;
            self.sessions_changed |= *n == 1;
            group.requests += 1;
            match counted.cost {
                Some(Cost::Priced(usd)) => group.priced.add(usd),
                Some(Cost::Known(usd)) => {
                    group.known += usd;
                    *group.floors.entry(usd).or_default() += 1;
                }
                None => {}
            }
            return;
        }
        self.requests -= 1;
        self.tokens -= counted.tokens;
        if let Some(n) = self.sessions.get_mut(&counted.session) {
            *n -= 1;
            if *n == 0 {
                self.sessions.remove(&counted.session);
                self.sessions_changed = true;
            }
        }
        group.requests -= 1;
        match counted.cost {
            Some(Cost::Priced(usd)) => group.priced.remove(usd),
            Some(Cost::Known(usd)) => {
                group.known -= usd;
                if let Some(n) = group.floors.get_mut(&usd) {
                    *n -= 1;
                    if *n == 0 {
                        group.floors.remove(&usd);
                    }
                }
            }
            None => {}
        }
        if group.requests == 0 {
            self.groups.swap_remove(&counted.group);
        }
    }
}

/// The provider the scan fills in for `r` once reading ends, when its record named none:
/// the one its profiles agree on, or Anthropic's public API for a Claude history whose
/// profiles name none.
fn provider(r: &Request, profiles: &[Profile]) -> Option<Text> {
    if r.provider.is_some() {
        return None;
    }
    let sharing: Vec<&Profile> = profiles
        .iter()
        .filter(|p| r.profiles.contains(&p.id))
        .collect();
    let first = sharing.first();
    if let Some(first) = first
        && sharing.iter().all(|p| p.provider == first.provider)
        && let Some(provider) = &first.provider
    {
        return Some(provider.as_str().into());
    }
    (r.harness == Harness::Claude && sharing.iter().all(|p| p.provider.is_none()))
        .then(|| "anthropic".into())
}

/// The session `session` counts in: the one that spawned it, when it is a subagent's.
fn root<'a>(session: &'a Text, subagents: &'a IndexMap<Text, Subagent>) -> &'a Text {
    let mut root = session;
    // Bounded so a cycle in recorded lineage cannot loop.
    for _ in 0..subagents.len() {
        match subagents.get(root).and_then(|s| s.parent.as_ref()) {
            Some(parent) => root = parent,
            None => break,
        }
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{make_request, prices};

    /// A request that changes is counted once, as it is now: a later chunk's usage replaces
    /// its earlier price, and an earlier copy can move it out of the window.
    #[test]
    fn a_changed_request_counts_as_it_is_now() {
        let catalog = prices();
        let subagents = IndexMap::default();
        let modern = HashSet::default();
        let basis = |catalog| Basis {
            since: Some(10),
            modern: &modern,
            subagents: &subagents,
            profiles: &[],
            catalog,
        };
        let mut requests = vec![Request {
            time: 20,
            ..make_request()
        }];
        let mut tally = Tally::default();
        tally.touch(0);
        let unpriced = tally.count(&mut requests, &mut [], &basis(None));
        assert_eq!((unpriced.requests, unpriced.sessions), (1, 1));
        assert_eq!(unpriced.spend, None);
        // Prices arriving price what was read without a new touch.
        let once = tally.count(&mut requests, &mut [], &basis(Some(&catalog)));
        let usd = |c: Counts| c.spend.unwrap().usd;
        assert!(usd(once) > 0.0);
        requests[0].parts[0].usage.output = Some(requests[0].parts[0].usage.output.unwrap() * 2);
        tally.touch(0);
        let twice = tally.count(&mut requests, &mut [], &basis(Some(&catalog)));
        assert_eq!(twice.requests, 1);
        assert!(twice.tokens > once.tokens && usd(twice) > usd(once));
        requests[0].time = 5;
        tally.touch(0);
        let gone = tally.count(&mut requests, &mut [], &basis(Some(&catalog)));
        assert_eq!((gone.requests, gone.tokens, gone.sessions), (0, 0, 0));
        assert_eq!(usd(gone), 0.0);
        assert!(tally.groups.is_empty() && tally.sessions.is_empty());
    }

    /// A legacy record counts once across copies, until a modern record of its total arrives,
    /// whether that was read before it or after.
    #[test]
    fn a_legacy_record_counts_until_a_modern_one_supersedes_it() {
        let mut modern = HashSet::default();
        let subagents = IndexMap::default();
        let legacy = |id: &str, total: &str| {
            let r = Request {
                id: id.into(),
                ..make_request()
            };
            (r, total.to_string())
        };
        let mut records = vec![legacy("a", "one"), legacy("a", "one")];
        let mut tally = Tally::default();
        let count = |tally: &mut Tally, records: &mut [(Request, String)], modern: &_| {
            let basis = Basis {
                since: None,
                modern,
                subagents: &subagents,
                profiles: &[],
                catalog: None,
            };
            tally.count(&mut [], records, &basis).requests
        };
        assert_eq!(count(&mut tally, &mut records, &modern), 1);
        modern.insert("one".to_string());
        tally.supersede("one");
        assert_eq!(count(&mut tally, &mut records, &modern), 0);
        modern.insert("two".to_string());
        records.push(legacy("b", "two"));
        records.push(legacy("c", "three"));
        assert_eq!(count(&mut tally, &mut records, &modern), 1);
        // A fork's copy of a counted id, under a total of its own, counts once the parent's
        // is superseded.
        records.push(legacy("c", "fork"));
        assert_eq!(count(&mut tally, &mut records, &modern), 1);
        modern.insert("three".to_string());
        tally.supersede("three");
        assert_eq!(count(&mut tally, &mut records, &modern), 1);
        modern.insert("fork".to_string());
        tally.supersede("fork");
        assert_eq!(count(&mut tally, &mut records, &modern), 0);
    }

    /// A subagent counts in the session at the root of its lineage, however much of that
    /// lineage had been read when its requests were.
    #[test]
    fn a_subagent_counts_in_its_root_session_once_its_lineage_is_read() {
        let modern = HashSet::default();
        let mut subagents = IndexMap::default();
        let session = |id: &str| Request {
            id: id.into(),
            meta: std::sync::Arc::new(crate::model::RequestMeta {
                session: id.into(),
                ..(*make_request().meta).clone()
            }),
            ..make_request()
        };
        let mut requests = vec![session("root"), session("grandchild")];
        let mut tally = Tally::default();
        tally.touch(0);
        tally.touch(1);
        // Later counts change no request, only the lineage.
        let mut count = |tally: &mut Tally, subagents: &IndexMap<Text, Subagent>| {
            let basis = Basis {
                since: None,
                modern: &modern,
                subagents,
                profiles: &[],
                catalog: None,
            };
            tally.count(&mut requests, &mut [], &basis).sessions
        };
        assert_eq!(count(&mut tally, &subagents), 2);
        let spawned = |session: &str, parent: &str| Subagent {
            harness: Harness::Codex,
            session: session.into(),
            parent: Some(parent.into()),
            kind: "default".into(),
            built_in: None,
            fork: false,
            native: false,
            model: None,
            effort: None,
        };
        subagents.insert("grandchild".into(), spawned("grandchild", "child"));
        assert_eq!(count(&mut tally, &subagents), 2);
        subagents.insert("child".into(), spawned("child", "root"));
        assert_eq!(count(&mut tally, &subagents), 1);
    }

    /// An unpriced request known to cost more than its group's priced mean costs at least
    /// what is known, as the header estimates it.
    #[test]
    fn a_known_charge_above_the_mean_raises_the_estimate() {
        let mut group = Group {
            requests: 2,
            known: Usd::from_f64(10.0),
            ..Group::default()
        };
        group.priced.add(Usd::from_f64(1.0));
        group.floors.insert(Usd::from_f64(10.0), 1);
        assert_eq!(group.estimate().unwrap().usd, 11.0);
        group.floors = BTreeMap::from([(Usd::from_f64(0.5), 1)]);
        assert_eq!(group.estimate().unwrap().usd, 2.0);
    }

    #[test]
    fn a_history_without_a_recorded_provider_is_priced_as_the_scan_fills_it() {
        let mut r = make_request();
        r.provider = None;
        let profile = |provider: Option<&str>| Profile {
            provider: provider.map(str::to_string),
            ..crate::test_support::make_profile("profile", "/synthetic")
        };
        assert_eq!(provider(&r, &[]).as_deref(), Some("anthropic"));
        assert_eq!(
            provider(&r, &[profile(Some("bedrock"))]).as_deref(),
            Some("bedrock")
        );
        r.harness = Harness::Codex;
        assert_eq!(provider(&r, &[]), None);
    }
}
