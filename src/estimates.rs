//! Comparison-only input estimates for unlinked API attempts, from adjacent measured requests.

use crate::collections::{HashMap, HashSet, IndexMap};

use crate::model::{
    BillingPart, EstimateEvidence, EventKind, Harness, Millis, PartKind, Request, RequestEvidence,
    Scan, Text, Usage, UsageEstimate,
};

fn same_input(a: &BillingPart, b: &BillingPart) -> bool {
    let key = |p: &BillingPart| {
        (
            p.kind,
            p.model.clone(),
            p.tier.clone(),
            p.speed.clone(),
            p.geo.clone(),
            p.usage.input,
            p.usage.read,
            p.usage.write,
            p.usage.write5m,
            p.usage.write1h,
        )
    };
    a.usage.input.is_some() && a.usage.read.is_some() && a.usage.write.is_some() && key(a) == key(b)
}

/// Whether a recorded string is present and nonempty.
fn present(value: &Option<Text>) -> bool {
    value.as_deref().is_some_and(|s| !s.is_empty())
}

fn profile_key(r: &Request) -> String {
    let mut profiles: Vec<&str> = r.profiles.iter().map(String::as_str).collect();
    profiles.sort_unstable();
    profiles.join(":")
}

/// Estimate isolated attempts from matching or same-cache-lifetime growing input brackets.
pub fn estimate_usage(scan: &Scan) -> IndexMap<Text, UsageEstimate> {
    let mut result = IndexMap::default();
    let mut boundaries: HashMap<&str, Vec<Millis>> = HashMap::default();
    for event in scan.cache_context.boundaries.iter().chain(&scan.events) {
        if event.harness != Harness::Claude || event.kind != EventKind::Compaction {
            continue;
        }
        for origin in &event.origins {
            boundaries
                .entry(&*origin.session)
                .or_default()
                .push(event.time);
        }
    }
    for times in boundaries.values_mut() {
        times.sort_unstable();
    }
    let mut sessions: IndexMap<&str, Vec<&Request>> = IndexMap::default();
    for r in scan.cache_context.requests.iter().chain(&scan.requests) {
        if r.harness == Harness::Claude {
            sessions.entry(&*r.session).or_default().push(r);
        }
    }
    for group in sessions.values_mut() {
        group.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| a.id.cmp(&b.id)));
        for index in 0..group.len() {
            let r = group[index];
            let Some(attempt) = &r.attempt else {
                continue;
            };
            if r.evidence != RequestEvidence::Unknown {
                continue;
            }
            let mut e = UsageEstimate {
                request: r.id.clone(),
                evidence: EstimateEvidence::Unknown,
                basis: vec![],
                part: None,
                max_output_tokens: attempt.max_output_tokens,
                reason: "no matching adjacent recorded input evidence",
                assumptions: vec![],
            };
            if let (Some(before), Some(after)) = (
                index.checked_sub(1).map(|i| group[i]),
                group.get(index + 1).copied(),
            ) {
                let times = boundaries.get(&*r.session).map_or(&[][..], Vec::as_slice);
                estimate(&mut e, r, before, after, times);
            }
            result.insert(r.id.clone(), e);
        }
    }
    result
}

/// Fill in one attempt's estimate from the measured requests immediately around it.
fn estimate(
    e: &mut UsageEstimate,
    r: &Request,
    before: &Request,
    after: &Request,
    times: &[Millis],
) {
    e.basis = vec![before.id.clone(), after.id.clone()];
    let (Some(p), Some(next), Some(pending)) =
        (before.parts.first(), after.parts.first(), r.parts.first())
    else {
        e.reason = "adjacent requests do not supply matching measured single-part input evidence";
        return;
    };
    if before.parts.len() != 1
        || after.parts.len() != 1
        || r.parts.len() != 1
        || before.evidence != RequestEvidence::Measured
        || after.evidence != RequestEvidence::Measured
        || pending.usage.input.is_some()
        || pending.usage.read.is_some()
        || pending.usage.write.is_some()
    {
        e.reason = "adjacent requests do not supply matching measured single-part input evidence";
        return;
    }
    let files: HashSet<&str> = r.locations.iter().map(|l| &*l.file).collect();
    let common_file = before
        .locations
        .iter()
        .any(|l| files.contains(&*l.file) && after.locations.iter().any(|n| n.file == l.file));
    let profiles = profile_key(r);
    if !common_file
        || !present(&r.reference)
        || !present(&r.source)
        || !present(&r.model)
        || !present(&r.provider)
        || before.end_time >= r.time
        || after.time <= r.time
        || [before, after].iter().any(|n| {
            n.source != r.source
                || n.model != r.model
                || n.provider != r.provider
                || n.agent != r.agent
                || n.version != r.version
                || n.client != r.client
                || profile_key(n) != profiles
        })
    {
        e.reason = "no matching source/model/provider/version/client/profile provenance and nonoverlapping file-local brackets";
        return;
    }
    let contradicts = |a: &Option<Text>, b: &Option<Text>| a.is_some() && a != b;
    if p.kind == PartKind::Unknown
        || p.kind != pending.kind
        || p.model != r.model
        || p.kind != next.kind
        || p.model != next.model
        || p.tier != next.tier
        || p.speed != next.speed
        || p.geo != next.geo
        || contradicts(&pending.tier, &p.tier)
        || contradicts(&pending.speed, &p.speed)
        || contradicts(&pending.geo, &p.geo)
    {
        e.reason = "brackets lack matching recorded input/cache/pricing metadata or contradict the request";
        return;
    }
    let exact = same_input(p, next);
    let complete = [p, next].iter().all(|part| {
        part.usage.input.is_some() && part.usage.read.is_some() && part.usage.write.is_some()
    });
    let previous_prefix = p.usage.read.unwrap_or(0) + p.usage.write.unwrap_or(0);
    let input_before_new_writes = match (next.usage.input, next.usage.write) {
        (Some(input), Some(write)) => Some(input - write),
        _ => None,
    };
    // Five minutes is the shortest supported cache lifetime, not a retry timeout.
    let growing = complete
        && after.time - before.time < 5 * 60_000
        && next
            .usage
            .read
            .is_some_and(|read| read > 0 && read >= previous_prefix)
        && input_before_new_writes >= p.usage.input;
    if !exact && !growing {
        e.reason = "no matching geometry or growing-context warm-read evidence within the five-minute cache lifetime";
        return;
    }
    let first_after = times.partition_point(|&t| t <= before.time);
    if times.get(first_after).is_some_and(|&t| t <= after.time) {
        e.reason = "matching brackets are interrupted by a compaction boundary";
        return;
    }
    let source = if exact { p } else { next };
    let usage = if exact {
        Usage {
            output: None,
            reasoning: None,
            ..source.usage
        }
    } else {
        Usage {
            output: None,
            reasoning: None,
            input: input_before_new_writes,
            read: Some(previous_prefix),
            write: next.usage.read.map(|read| read - previous_prefix),
            // Replay assigns the chosen TTL, not an invented observed split.
            write5m: None,
            write1h: None,
            ..source.usage
        }
    };
    e.evidence = EstimateEvidence::Estimated;
    e.part = Some(BillingPart {
        usage,
        ..source.clone()
    });
    e.reason = if exact {
        "matching adjacent input geometry and pricing metadata in the same conversation/file"
    } else {
        "growing-context downstream cache-read evidence within one five-minute cache lifetime, with matching pricing/provenance"
    };
    e.assumptions = vec![
        if exact {
            "one billed API attempt with the same input/cache geometry and model/tier/speed/geography as both recorded brackets; this is not measured execution or prefix identity"
        } else {
            "one billed attempt with the preceding cached prefix as warm reads and any additional next-request reads attributed as its writes; next-request new writes are excluded from its input, and next-request ordinary input is assumed unchanged"
        }
        .into(),
        "retry/content identity, cache survival and attribution of extra warm reads to this attempt rather than another cache warmer are assumptions, not measured billing".into(),
        "actual usage and historical spend stay unknown; output is not imputed and cancels only under fixed behavior across TTL scenarios".into(),
        "no-billed-attempt and cold-cache sensitivities are conditional scenarios, not guaranteed bounds on hidden work or context changes".into(),
    ];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Attempt, CacheContext, ExposureScan, ScanCoverage};
    use crate::test_support::{ClaudeUsage, claude_usage, make_request};

    fn scan(requests: Vec<Request>) -> Scan {
        Scan {
            snapshot_started_at: String::new(),
            snapshot_finished_at: String::new(),
            since: None,
            roots: vec![],
            profiles: vec![],
            snapshots: vec![],
            requests,
            events: vec![],
            exposure: ExposureScan::default(),
            cache_context: CacheContext::default(),
            coverage: ScanCoverage::default(),
            t3: vec![],
        }
    }

    /// An unlinked attempt between two measured requests, mirroring the replay tests.
    fn bracketed(attempt_time: Millis, after_time: Millis, after: Usage) -> Vec<Request> {
        let first = Request {
            id: "before".into(),
            ..make_request()
        };
        let attempt = Request {
            id: "attempt".into(),
            evidence: RequestEvidence::Unknown,
            time: attempt_time,
            end_time: attempt_time,
            reference: Some("attempt-ref".into()),
            attempt: Some(Attempt {
                max_output_tokens: Some(100),
            }),
            server_tools: None,
            parts: vec![BillingPart {
                usage: Usage::default(),
                tier: None,
                speed: None,
                geo: None,
                ..first.parts[0].clone()
            }],
            ..make_request()
        };
        let mut next = Request {
            id: "after".into(),
            time: after_time,
            end_time: after_time,
            ..make_request()
        };
        next.parts[0].usage = after;
        vec![first, attempt, next]
    }

    fn usage(read: i64, write: i64) -> Usage {
        claude_usage(ClaudeUsage {
            read,
            write,
            ..ClaudeUsage::default()
        })
    }

    #[test]
    fn matching_brackets_copy_their_geometry_without_imputing_output() {
        let requests = bracketed(600000, 1200000, claude_usage(ClaudeUsage::default()));
        let estimates = estimate_usage(&scan(requests));
        let e = &estimates["attempt"];
        assert_eq!(e.evidence, EstimateEvidence::Estimated);
        assert_eq!(e.basis, [Text::from("before"), Text::from("after")]);
        assert_eq!(e.max_output_tokens, Some(100));
        let u = e.part.as_ref().unwrap().usage;
        assert_eq!(
            (u.input, u.output, u.write5m),
            (Some(1000), None, Some(1000))
        );
    }

    #[test]
    fn retry_estimates_attribute_additional_downstream_reads_never_the_following_requests_own_writes()
     {
        let estimates = estimate_usage(&scan(bracketed(300, 3300, usage(1100, 100))));
        let u = estimates["attempt"].part.as_ref().unwrap().usage;
        assert_eq!(
            (u.input, u.read, u.write),
            (Some(1100), Some(1000), Some(100))
        );
        assert_eq!((u.write5m, u.write1h), (None, None));
        // Reads that do not cover the preceding prefix cannot come from this attempt.
        let invalidated = estimate_usage(&scan(bracketed(300, 3300, usage(900, 300))));
        assert_eq!(invalidated["attempt"].evidence, EstimateEvidence::Unknown);
        // The bracket must fit within the shortest cache lifetime.
        let expired = estimate_usage(&scan(bracketed(300, 300000, usage(1100, 100))));
        assert_eq!(expired["attempt"].evidence, EstimateEvidence::Unknown);
    }
}
