//! Shared cache replay of request parts under chosen TTLs, for every setting consumer.

use std::collections::HashSet;

use indexmap::IndexMap;

use crate::cache::{CacheChain, Replayed, Touch};
use crate::model::{
    Agent, BillingPart, Bucket, CacheReplayEvidence, Harness, Millis, PartKind, Request,
    RequestEvidence, StructuralEvent, Ttl, UsageEstimate,
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
    pub estimates: &'a IndexMap<String, UsageEstimate>,
    pub catalog: &'a Catalog,
    pub ttls: ReplayTtls,
    /// Parts at or after this time are selected; earlier parts only inform cache state.
    pub since: Millis,
    pub cold_prefixes: bool,
    /// Further (request id, part index) pairs whose geometry is estimated.
    pub estimated_parts: Option<&'a HashSet<(String, usize)>>,
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
    pub evidence: Option<CacheReplayEvidence>,
}

/// Replay the same cache histories and usage estimates for every setting consumer.
pub fn replay_parts<'a, 'e>(
    requests: impl IntoIterator<Item = &'a Request>,
    events: impl IntoIterator<Item = &'e StructuralEvent>,
    options: &ReplayOptions,
) -> Vec<ReplayedPart<'a>> {
    let mut chain = CacheChain::new(events);
    let mut result = Vec::new();
    for r in requests {
        let estimate = options.estimates.get(&r.id).and_then(|e| e.part.as_ref());
        if estimate.is_some() && options.attempt_scenario == AttemptScenario::NoBilledAttempt {
            continue;
        }
        let parts = estimate.map_or(r.parts.as_slice(), std::slice::from_ref);
        for (part, p) in parts.iter().enumerate() {
            let bucket = cache_bucket(r, p);
            let ttl = options.ttls.get(bucket);
            let selected = r.time >= options.since;
            let replayed = if r.harness == Harness::Claude {
                chain.replay(&Touch {
                    request: r,
                    p,
                    part,
                    estimated: estimate.is_some()
                        || r.evidence == RequestEvidence::Estimated
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
            let mut components = price_components(options.catalog, r, &replayed.p);
            if r.harness == Harness::Claude && (p.usage.read != Some(0) || p.usage.write != Some(0))
            {
                if bucket.is_none() {
                    components.cache =
                        unknown_cost("cache TTL bucket is not established for this part");
                } else if ttl.is_none() {
                    components.cache =
                        unknown_cost("effective cache TTL policy is not established");
                } else if replayed.unknown {
                    let evidence = replayed
                        .evidence
                        .as_ref()
                        .expect("unknown replay has evidence");
                    components.cache = unknown_cost(evidence.reason.clone());
                }
            }
            result.push(ReplayedPart {
                request: r,
                part,
                p: replayed.p,
                selected,
                components,
                evidence: replayed.evidence,
            });
        }
    }
    result
}
