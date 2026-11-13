//! Shared cache replay of request parts under chosen TTLs, for every setting consumer.

use crate::collections::{HashSet, IndexMap};

use crate::cache::{CacheBoundaries, CacheChain, Replayed, Touch};
use crate::model::{
    Agent, BillingPart, Bucket, CacheReplayEvidence, Harness, Millis, PartKind, Request,
    RequestEvidence, Text, Ttl, UsageEstimate,
};
use crate::prices::{Catalog, Components, price_components, unknown_cost};

/// Cache policy bucket for a billed iteration; None when its attribution is unresolved.
pub fn cache_bucket(r: &Request, p: &BillingPart) -> Option<Bucket> {
    if r.agent == Agent::Subagent {
        return Some(Bucket::Subagent);
    }
    match p.kind {
        PartKind::Unknown => None,
        PartKind::Compaction | PartKind::Advisor => Some(Bucket::Subagent),
        PartKind::Message => match r.agent {
            Agent::Main => Some(Bucket::Main),
            Agent::Subagent => Some(Bucket::Subagent),
            Agent::Unknown => None,
        },
    }
}

/// The TTL replayed in each bucket, including parts whose bucket is unresolved; None when the
/// effective policy is not established.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ReplayTtls {
    pub main: Option<Ttl>,
    pub subagent: Option<Ttl>,
    pub unknown: Option<Ttl>,
}

impl ReplayTtls {
    /// The same TTL in every bucket.
    pub fn all(ttl: Option<Ttl>) -> Self {
        Self {
            main: ttl,
            subagent: ttl,
            unknown: ttl,
        }
    }

    fn get(&self, bucket: Option<Bucket>) -> Option<Ttl> {
        match bucket {
            Some(Bucket::Main) => self.main,
            Some(Bucket::Subagent) => self.subagent,
            None => self.unknown,
        }
    }
}

/// How an estimated unlinked attempt is replayed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AttemptScenario {
    /// One billed attempt with the bracketed input geometry.
    #[default]
    Point,
    /// No billed or cache-affecting attempt.
    NoBilledAttempt,
    /// One billed attempt whose whole input is a cold cache write.
    ColdCache,
}

/// Inputs shared by every replay of one set of requests.
#[derive(Clone, Copy, Debug)]
pub struct ReplayOptions<'a> {
    pub estimates: &'a IndexMap<Text, UsageEstimate>,
    pub catalog: &'a Catalog,
    pub ttls: ReplayTtls,
    /// Parts at or after this time are selected; earlier parts only inform cache state.
    pub since: Millis,
    pub cold_prefixes: bool,
    /// Further (request id, part index) pairs whose geometry is estimated.
    pub estimated_parts: Option<&'a HashSet<(Text, usize)>>,
    pub attempt_scenario: AttemptScenario,
}

/// One cache replay result; pre-window parts inform evidence without becoming billed.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayedPart<'a> {
    pub request: &'a Request,
    pub part: usize,
    pub p: BillingPart,
    pub selected: bool,
    pub components: Components,
    /// Why the cache charge is unknown though the part prices, if it is.
    pub cache_unknown: Option<&'static str>,
    pub evidence: Option<Box<CacheReplayEvidence>>,
}

/// Price one replayed part, whose cache charge is unknown for `cache_unknown`.
pub(crate) fn replayed_components(
    catalog: &Catalog,
    r: &Request,
    p: &BillingPart,
    cache_unknown: Option<&str>,
) -> Components {
    let mut components = price_components(catalog, r, p);
    if let Some(reason) = cache_unknown {
        components.cache = unknown_cost(reason);
    }
    components
}

/// Replay the same cache histories and usage estimates for every setting consumer.
///
/// Parts are replayed as the iterator is consumed, so a caller keeps only what it needs.
///
/// `boundaries` index the compaction events that reset cache histories. Callers build each
/// index once (for example per profile) and pass modeled events as another small layer,
/// because rebuilding the index from every event on each replay dominated the run time.
pub fn replay_parts<'a, 'b>(
    requests: impl IntoIterator<Item = &'a Request>,
    boundaries: impl IntoIterator<Item = &'b CacheBoundaries>,
    options: &ReplayOptions,
) -> impl Iterator<Item = ReplayedPart<'a>> {
    let mut replay = PartReplay::new(boundaries, options);
    requests
        .into_iter()
        .flat_map(move |r| replay.request(r, &[]))
}

/// A replay fed one request at a time, for callers that model each request just before it.
pub(crate) struct PartReplay<'b, 'o> {
    chain: CacheChain<'b>,
    options: ReplayOptions<'o>,
}

impl<'b, 'o> PartReplay<'b, 'o> {
    pub(crate) fn new(
        boundaries: impl IntoIterator<Item = &'b CacheBoundaries>,
        options: &ReplayOptions<'o>,
    ) -> Self {
        Self {
            chain: CacheChain::new(boundaries),
            options: *options,
        }
    }

    /// Reset `session`'s cache histories at `time`, as a compaction boundary there would.
    pub(crate) fn boundary(&mut self, session: &Text, time: Millis) {
        self.chain.boundary(session, time);
    }

    /// Forget a session that has no more requests.
    pub(crate) fn finish(&mut self, session: &str) {
        self.chain.finish(session);
    }

    /// Replay the next request's parts, with `estimated` naming further parts whose
    /// geometry is estimated.
    pub(crate) fn request<'a>(
        &mut self,
        r: &'a Request,
        estimated: &[usize],
    ) -> Vec<ReplayedPart<'a>> {
        let options = self.options;
        let mut result = Vec::new();
        let estimate = options.estimates.get(&r.id).and_then(|e| e.part.as_ref());
        if estimate.is_some() && options.attempt_scenario == AttemptScenario::NoBilledAttempt {
            return result;
        }
        let parts = estimate.map_or(r.parts.as_slice(), std::slice::from_ref);
        for (part, p) in parts.iter().enumerate() {
            let bucket = cache_bucket(r, p);
            let ttl = options.ttls.get(bucket);
            let selected = r.time >= options.since;
            let replayed = if r.harness == Harness::Claude {
                self.chain.replay(&Touch {
                    request: r,
                    p,
                    part,
                    estimated: estimate.is_some()
                        || r.evidence == RequestEvidence::Estimated
                        || estimated.contains(&part)
                        || options
                            .estimated_parts
                            .is_some_and(|set| set.contains(&(r.id.clone(), part))),
                    ttl: ttl.unwrap_or(Ttl::FiveMinutes),
                    cold: options.cold_prefixes,
                    selected,
                })
            } else {
                Some(Replayed {
                    p: p.clone(),
                    unknown: false,
                    evidence: None,
                })
            };
            let Some(mut replayed) = replayed else {
                continue;
            };
            if estimate.is_some()
                && options.attempt_scenario == AttemptScenario::ColdCache
                && r.harness == Harness::Claude
            {
                let u = &mut replayed.p.usage;
                let write = u.write.unwrap_or(0) + u.read.unwrap_or(0);
                u.write = Some(write);
                u.read = Some(0);
                u.write5m = Some(if ttl == Some(Ttl::FiveMinutes) {
                    write
                } else {
                    0
                });
                u.write1h = Some(if ttl == Some(Ttl::OneHour) { write } else { 0 });
            }
            let mut cache_unknown = None;
            if r.harness == Harness::Claude && (p.usage.read != Some(0) || p.usage.write != Some(0))
            {
                if bucket.is_none() {
                    cache_unknown = Some("cache TTL bucket is not established for this part");
                } else if ttl.is_none() {
                    cache_unknown = Some("effective cache TTL policy is not established");
                } else if replayed.unknown {
                    let evidence = replayed
                        .evidence
                        .as_ref()
                        .expect("unknown replay has evidence");
                    cache_unknown = Some(evidence.reason);
                }
            }
            let components = replayed_components(options.catalog, r, &replayed.p, cache_unknown);
            result.push(ReplayedPart {
                request: r,
                part,
                p: replayed.p,
                selected,
                components,
                cache_unknown,
                evidence: replayed.evidence,
            });
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EventKind, IdleAttribution, Location, Origin, StructuralEvent};
    use crate::test_support::{ClaudeUsage, claude_usage, make_request, prices};

    fn at(id: &str, session: &str, time: Millis, read: i64, write: i64) -> Request {
        let mut r = {
            let mut built = Request {
                id: id.into(),
                time,
                end_time: time,
                ..make_request()
            };
            built.session = session.into();
            built
        };
        r.parts[0].usage = claude_usage(ClaudeUsage {
            read,
            write,
            ..ClaudeUsage::default()
        });
        r
    }

    fn boundary(id: &str, session: &str, time: Millis) -> StructuralEvent {
        let location = Location {
            file: "synthetic".into(),
            line: 1,
        };
        StructuralEvent {
            id: id.into(),
            origins: vec![Origin {
                session: session.into(),
                location: location.clone(),
            }]
            .into(),
            time,
            kind: EventKind::Compaction,
            location,
            request: None,
            tool_id: None,
            settings: Default::default(),
            idle_attribution: IdleAttribution::Unknown,
            shell: None,
            meta: crate::model::EventMeta {
                harness: Harness::Claude,
                session: session.into(),
                profiles: vec!["profile".into()].into(),
                model: None,
                version: None,
                client: None,
                cwd: None,
                trigger: None,
                precomputed: None,
                pre_tokens: None,
                post_tokens: None,
                duration_ms: None,
                tool: None,
                t3: None,
                pricing: None,
            }
            .into(),
        }
    }

    /// Splitting boundaries across a prebuilt index and an extra layer replays like one index.
    #[test]
    fn layered_boundaries_replay_like_their_concatenation() {
        let requests = [
            at("a-0", "a", 0, 0, 1000),
            at("b-0", "b", 0, 0, 1000),
            at("a-1", "a", 600_000, 0, 1500),
            at("b-1", "b", 600_000, 0, 1500),
            at("a-2", "a", 1_200_000, 0, 1500),
            at("b-2", "b", 1_200_000, 0, 1500),
        ];
        let base = [boundary("base", "a", 300_000)];
        let extra = [boundary("extra", "b", 900_000)];
        let catalog = prices();
        let estimates = IndexMap::default();
        let options = ReplayOptions {
            estimates: &estimates,
            catalog: &catalog,
            ttls: ReplayTtls::all(Some(Ttl::OneHour)),
            since: 0,
            cold_prefixes: false,
            estimated_parts: None,
            attempt_scenario: AttemptScenario::Point,
        };
        let combined = CacheBoundaries::new(base.iter().chain(&extra));
        let (base, extra) = (CacheBoundaries::new(&base), CacheBoundaries::new(&extra));
        let replay = |boundaries: &[&CacheBoundaries]| {
            replay_parts(&requests, boundaries.iter().copied(), &options).collect::<Vec<_>>()
        };
        let layered = replay(&[&base, &extra]);
        assert_eq!(layered, replay(&[&combined]));
        assert_ne!(layered, replay(&[&base]));
        assert_ne!(layered, replay(&[]));
    }
}
