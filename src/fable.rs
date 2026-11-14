//! Each Claude profile's Fable spend per week, for comparing with a plan's documented Fable cap.

use crate::model::{FableUse, FableWeek, Harness, Millis, Request, Scan};
use crate::prices::{Catalog, Sum, add_costs, price_part, request_evidence, server_cost};

const WEEK: Millis = 7 * 24 * 60 * 60 * 1000;

/// Whether a recorded model ID is a Fable model, such as `claude-fable-5-2[1m]`.
fn is_fable(model: &str) -> bool {
    model.contains("fable")
}

/// What a request's Fable parts cost, or `None` when no part ran on Fable.
///
/// An advisor iteration records its own model, so a request can mix Fable with another model;
/// only the Fable parts count toward the cap. Server-tool fees go with the request's own model.
fn fable_cost(catalog: &Catalog, r: &Request) -> Option<crate::model::Cost> {
    let fable = |m: Option<&str>| m.is_some_and(is_fable);
    let parts: Vec<_> = r
        .parts
        .iter()
        .filter(|p| fable(p.model.as_deref()))
        .map(|p| price_part(catalog, r, p))
        .collect();
    if parts.is_empty() {
        return None;
    }
    let server = fable(r.model.as_deref()).then(|| server_cost(r));
    Some(request_evidence(r, add_costs(parts.iter().chain(&server))))
}

/// Fable spend for every Claude profile that signs in to Anthropic rather than a cloud
/// provider, in 7-day spans counted back from `now` to `since`, or else the earliest request.
///
/// A plan's weekly reset time is not recorded, so spans end at the report instead.
pub fn fable_use(
    scan: &Scan,
    catalog: &Catalog,
    since: Option<Millis>,
    now: Millis,
) -> Vec<FableUse> {
    let Some(start) = since.or_else(|| scan.requests.iter().map(|r| r.time).min()) else {
        return vec![];
    };
    // At least one span, so a request at `now` counts even when the window starts there.
    let mut spans = vec![(start.max(now - WEEK), now)];
    let mut end = now - WEEK;
    while end > start {
        spans.push((start.max(end - WEEK), end));
        end -= WEEK;
    }
    // Bedrock, Vertex and Foundry bill per token; no plan limit applies there.
    let eligible: Vec<&str> = scan
        .profiles
        .iter()
        .filter(|p| p.harness == Harness::Claude && p.provider.is_none())
        .map(|p| p.id.as_str())
        .collect();
    eligible
        .iter()
        .map(|&id| {
            let mut sums: Vec<(u64, Sum)> = spans.iter().map(|_| (0, Sum::new())).collect();
            let mut unrecorded = 0;
            let mut shared = 0;
            for r in scan.requests.iter().filter(|r| {
                r.harness == Harness::Claude
                    && r.time >= start
                    && r.time <= now
                    && r.profiles.iter().any(|p| p == id)
            }) {
                let Some(cost) = fable_cost(catalog, r) else {
                    if r.model.is_none() {
                        unrecorded += 1;
                    }
                    continue;
                };
                // Spans are most recent first and end exclusive, except the latest.
                let i = spans
                    .iter()
                    .position(|&(s, e)| r.time >= s && (r.time < e || e == now));
                if let Some(i) = i {
                    sums[i].0 += 1;
                    sums[i].1.add(&cost);
                }
                if r.profiles
                    .iter()
                    .any(|p| p != id && eligible.contains(&p.as_str()))
                {
                    shared += 1;
                }
            }
            let mut weeks: Vec<FableWeek> = spans
                .iter()
                .zip(sums)
                .map(|(&(start, end), (requests, sum))| FableWeek {
                    start,
                    end,
                    requests,
                    cost: sum.cost(),
                })
                .collect();
            // Weeks before the first Fable request say nothing, and a long history has many.
            while weeks.last().is_some_and(|w| w.requests == 0) {
                weeks.pop();
            }
            FableUse {
                profile: id.to_string(),
                requests: weeks.iter().map(|w| w.requests).sum(),
                unrecorded,
                shared,
                weeks,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CacheContext, ExposureScan, ScanCoverage};
    use crate::prices::price_request;
    use crate::test_support::{make_profile, make_request, prices};

    const DAY: Millis = 24 * 60 * 60 * 1000;

    fn scan(requests: Vec<crate::model::Request>, since: Option<Millis>) -> Scan {
        let mut cloud = make_profile("cloud", "/synthetic/cloud");
        cloud.provider = Some("amazon-bedrock".into());
        Scan {
            snapshot_started_at: String::new(),
            snapshot_finished_at: String::new(),
            since,
            roots: vec![],
            profiles: vec![make_profile("profile", "/synthetic/claude"), cloud],
            snapshots: vec![],
            requests,
            events: vec![],
            exposure: ExposureScan::default(),
            cache_context: CacheContext::default(),
            coverage: ScanCoverage::default(),
            subagents: vec![],
            t3: vec![],
            sessions: Default::default(),
        }
    }

    fn request(time: Millis, model: Option<&str>, profile: &str) -> crate::model::Request {
        let mut r = make_request();
        r.time = time;
        r.model = model.map(Into::into);
        r.profiles = vec![profile.into()].into();
        if let Some(m) = model {
            r.parts[0].model = Some(m.into());
        }
        r
    }

    #[test]
    fn spans_count_back_from_now_and_bucket_only_fable_requests() {
        let now = 20 * DAY;
        let fable = Some("claude-fable-5-2");
        let requests = vec![
            request(4 * DAY, fable, "profile"),
            // On a span's start, it belongs to the later span.
            request(6 * DAY, fable, "profile"),
            request(now, fable, "profile"),
            request(10 * DAY, Some("claude-opus-5-5"), "profile"),
            request(11 * DAY, None, "profile"),
            request(12 * DAY, fable, "cloud"),
        ];
        let catalog = prices();
        let uses = fable_use(&scan(requests.clone(), None), &catalog, Some(3 * DAY), now);
        assert_eq!(uses.len(), 1, "the cloud profile has no plan limit");
        let u = &uses[0];
        assert_eq!(
            (u.profile.as_str(), u.requests, u.unrecorded),
            ("profile", 3, 1)
        );
        let spans: Vec<(Millis, Millis, u64)> = u
            .weeks
            .iter()
            .map(|w| (w.start, w.end, w.requests))
            .collect();
        assert_eq!(
            spans,
            [
                (13 * DAY, 20 * DAY, 1),
                (6 * DAY, 13 * DAY, 1),
                (3 * DAY, 6 * DAY, 1)
            ]
        );
        // Priced at the Fable rate, which the synthetic catalog sets above Opus.
        let opus = price_request(&catalog, &request(0, Some("claude-opus-5-5"), "profile"));
        let week = u.weeks[0].cost.total_usd.expect("priced");
        assert!(week > opus.total_usd.unwrap(), "{week:?}");
        // A later analysis bound drops earlier requests, unrecorded ones included.
        let later = fable_use(
            &scan(requests, Some(3 * DAY)),
            &catalog,
            Some(12 * DAY),
            now,
        );
        let spans: Vec<(Millis, Millis, u64)> = later[0]
            .weeks
            .iter()
            .map(|w| (w.start, w.end, w.requests))
            .collect();
        // The oldest span, with no Fable request, is left out.
        assert_eq!(spans, [(13 * DAY, 20 * DAY, 1)]);
        assert_eq!((later[0].requests, later[0].unrecorded), (1, 0));
    }

    #[test]
    fn weeks_start_at_the_first_fable_request_and_skip_later_requests() {
        let now = 100 * DAY;
        let fable = Some("claude-fable-5-2");
        let requests = vec![
            // Opus long before Fable sets the window's start, but adds no weeks.
            request(DAY, Some("claude-opus-5-5"), "profile"),
            request(now - 9 * DAY, fable, "profile"),
            // After the report's time, as from a skewed clock: never counted.
            request(now + DAY, fable, "profile"),
            request(now + DAY, None, "profile"),
        ];
        let uses = fable_use(&scan(requests, None), &prices(), None, now);
        let spans: Vec<(Millis, Millis, u64)> = uses[0]
            .weeks
            .iter()
            .map(|w| (w.start, w.end, w.requests))
            .collect();
        assert_eq!(
            spans,
            [(now - 7 * DAY, now, 0), (now - 14 * DAY, now - 7 * DAY, 1)]
        );
        assert_eq!((uses[0].requests, uses[0].unrecorded), (1, 0));
    }

    #[test]
    fn requests_shared_with_another_eligible_profile_are_counted_as_shared() {
        let now = 10 * DAY;
        let fable = Some("claude-fable-5-2");
        let mut both = request(DAY, fable, "profile");
        both.profiles = vec!["profile".into(), "second".into()].into();
        let mut cloud = request(2 * DAY, fable, "profile");
        cloud.profiles = vec!["profile".into(), "cloud".into()].into();
        let mut s = scan(vec![both, cloud, request(3 * DAY, fable, "profile")], None);
        s.profiles.push(make_profile("second", "/synthetic/second"));
        let uses = fable_use(&s, &prices(), None, now);
        let counts: Vec<(&str, u64, u64)> = uses
            .iter()
            .map(|u| (u.profile.as_str(), u.requests, u.shared))
            .collect();
        // A cloud profile has no plan limit, so sharing with it leaves the owner known.
        assert_eq!(counts, [("profile", 3, 1), ("second", 1, 1)]);
    }

    #[test]
    fn a_request_at_the_report_time_counts_when_the_window_starts_there() {
        let now = 10 * DAY;
        let only = vec![request(now, Some("claude-fable-5-2"), "profile")];
        for since in [None, Some(now)] {
            let uses = fable_use(&scan(only.clone(), since), &prices(), since, now);
            let spans: Vec<(Millis, Millis, u64)> = uses[0]
                .weeks
                .iter()
                .map(|w| (w.start, w.end, w.requests))
                .collect();
            assert_eq!(spans, [(now, now, 1)], "{since:?}");
        }
    }

    #[test]
    fn only_fable_parts_count_and_are_priced() {
        let now = 10 * DAY;
        let catalog = prices();
        let opus = request(DAY, Some("claude-opus-5-5"), "profile");
        let fable = request(DAY, Some("claude-fable-5-2"), "profile");
        // An Opus request whose advisor iteration ran on Fable, and the reverse.
        let mut advised = opus.clone();
        let mut advisor = fable.parts[0].clone();
        advisor.kind = crate::model::PartKind::Advisor;
        advised.parts.push(advisor);
        let mut reverse = fable.clone();
        let mut part = opus.parts[0].clone();
        part.kind = crate::model::PartKind::Advisor;
        reverse.parts.push(part);
        let uses = fable_use(&scan(vec![advised, reverse], None), &catalog, None, now);
        assert_eq!((uses[0].requests, uses[0].unrecorded), (2, 0));
        let fable_part = price_part(&catalog, &fable, &fable.parts[0])
            .total_usd
            .unwrap();
        let fees = server_cost(&fable).total_usd.unwrap();
        let total = uses[0].weeks.last().unwrap().cost.total_usd.unwrap();
        // Two Fable parts and one request's fees; neither Opus part.
        assert_eq!(total, fable_part + fable_part + fees);
    }

    #[test]
    fn no_requests_and_no_bound_leave_nothing_to_show() {
        assert_eq!(fable_use(&scan(vec![], None), &prices(), None, DAY), vec![]);
    }
}
