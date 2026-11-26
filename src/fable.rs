//! Each Claude profile's Fable spend per week, for comparing with a plan's documented Fable cap.

use crate::model::{FableUse, FableWeek, Harness, Millis, PartKind, Request, Scan, Usd};
use crate::prices::{
    Catalog, Sample, Sum, add_costs, floored, price_part, request_evidence, server_cost,
    unknown_cost,
};

const WEEK: Millis = 7 * 24 * 60 * 60 * 1000;

/// Whether a recorded model ID is a Fable model, such as `claude-fable-5-2[1m]`.
fn is_fable(model: &str) -> bool {
    model.contains("fable")
}

/// Whether a request made an advisor call that recorded no model, which may have run on Fable.
fn unrecorded_advisor(r: &Request) -> bool {
    r.parts
        .iter()
        .any(|p| p.kind == PartKind::Advisor && p.model.is_none())
}

/// What a request's Fable parts cost, or `None` when no part ran on Fable.
///
/// An advisor iteration records its own model, so a request can mix Fable with another model;
/// only the Fable parts count toward the cap. Server-tool fees go with the request's own model,
/// and are unknown when it recorded none.
fn fable_cost(catalog: &Catalog, r: &Request) -> Option<crate::model::Cost> {
    let fable = |m: Option<&str>| m.is_some_and(is_fable);
    let mut parts: Vec<_> = r
        .parts
        .iter()
        .filter(|p| fable(p.model.as_deref()))
        .map(|p| price_part(catalog, r, p))
        .collect();
    if parts.is_empty() {
        return None;
    }
    // Claude Code pairs a Fable main model only with a Fable advisor, so beside Fable parts a
    // call that recorded no model likely ran on Fable too, at rates not recorded.
    if unrecorded_advisor(r) {
        parts.push(unknown_cost(
            "an advisor call recorded no model, and may have run on Fable",
        ));
    }
    let server = match r.model.as_deref() {
        Some(m) if is_fable(m) => Some(server_cost(r)),
        Some(_) => None,
        None => {
            let fees = server_cost(r);
            (fees.total_usd != Some(Usd::ZERO)).then(|| {
                unknown_cost("server-tool fees of a request with no recorded model may be Fable's")
            })
        }
    };
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
    // Cloud providers bill per token; no plan limit applies there.
    let eligible: Vec<&str> = scan
        .profiles
        .iter()
        .filter(|p| p.harness == Harness::Claude && p.provider.is_none())
        .map(|p| p.id.as_str())
        .collect();
    // Profiles that share a home's settings file share its line.
    let lines = crate::rows::groups(
        scan.profiles
            .iter()
            .filter(|p| eligible.contains(&p.id.as_str())),
        |p| format!("{}:", p.settings_file),
    );
    lines
        .iter()
        .map(|line| {
            let mine = |p: &String| line.ids.contains(p);
            // Each span's requests, cost, priced sample, and the known charge of each partly
            // priced request, which its extrapolated share never falls below.
            let mut sums: Vec<(u64, Sum, Sample, Vec<f64>)> = spans
                .iter()
                .map(|_| (0, Sum::new(), Sample::default(), vec![]))
                .collect();
            let mut unrecorded = 0;
            let mut shared = 0;
            for r in scan.requests.iter().filter(|r| {
                r.harness == Harness::Claude
                    && r.time >= start
                    && r.time <= now
                    && r.profiles.iter().any(mine)
            }) {
                let Some(cost) = fable_cost(catalog, r) else {
                    if r.model.is_none() || unrecorded_advisor(r) {
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
                    sums[i].2.add_cost(&cost);
                    if cost.total_usd.is_none() {
                        sums[i].3.push(cost.known_usd.to_f64());
                    }
                }
                if r.profiles
                    .iter()
                    .any(|p| !mine(p) && eligible.contains(&p.as_str()))
                {
                    shared += 1;
                }
            }
            let mut all = Sample::default();
            let mut floors = vec![];
            for (_, _, sample, known) in &sums {
                all += *sample;
                floors.extend(known);
            }
            let mut weeks: Vec<FableWeek> = spans
                .iter()
                .zip(sums)
                .map(
                    |(&(start, end), (requests, sum, sample, known))| FableWeek {
                        start,
                        end,
                        requests,
                        cost: sum.cost(),
                        estimate: floored(sample.estimate(requests), requests, &known),
                    },
                )
                .collect();
            // Weeks before the first Fable request say nothing, and a long history has many.
            while weeks.last().is_some_and(|w| w.requests == 0) {
                weeks.pop();
            }
            let requests = weeks.iter().map(|w| w.requests).sum();
            FableUse {
                profiles: line.ids.clone(),
                requests,
                unrecorded,
                shared,
                weeks,
                estimate: floored(all.estimate(requests), requests, &floors),
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
            defaults: false,
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
            (u.profiles[0].as_str(), u.requests, u.unrecorded),
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
            .map(|u| (u.profiles[0].as_str(), u.requests, u.shared))
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
        let uses = fable_use(
            &scan(vec![advised.clone(), reverse.clone()], None),
            &catalog,
            None,
            now,
        );
        assert_eq!((uses[0].requests, uses[0].unrecorded), (2, 0));
        let fable_part = price_part(&catalog, &fable, &fable.parts[0])
            .total_usd
            .unwrap();
        let fees = server_cost(&fable).total_usd.unwrap();
        let total = uses[0].weeks.last().unwrap().cost.total_usd.unwrap();
        // Two Fable parts and one request's fees; neither Opus part.
        assert_eq!(total, fable_part + fable_part + fees);
        // An advisor call that recorded no model may be Fable's under an Opus main model, and
        // is Fable's, unpriced, under a Fable one.
        let unrecorded = |mut r: crate::model::Request| {
            r.parts.last_mut().unwrap().model = None;
            r
        };
        let uses = fable_use(
            &scan(vec![unrecorded(advised), unrecorded(reverse)], None),
            &catalog,
            None,
            now,
        );
        assert_eq!((uses[0].requests, uses[0].unrecorded), (1, 1));
        let week = uses[0].weeks.last().unwrap();
        assert_eq!(week.cost.total_usd, None);
        assert_eq!(week.cost.known_usd, fable_part + fees);
        // Beside small requests priced in full, its estimate never falls below its known
        // charge.
        let mut large = fable.clone();
        large.parts[0].usage.input = Some(10_000_000);
        let mut advisor = large.parts[0].clone();
        advisor.kind = crate::model::PartKind::Advisor;
        advisor.model = None;
        large.parts.push(advisor);
        let known = fable_cost(&catalog, &large).unwrap().known_usd;
        let mut small = fable.clone();
        small.parts[0].usage.input = Some(1);
        let small_cost = fable_cost(&catalog, &small).unwrap().total_usd.unwrap();
        let uses = fable_use(
            &scan(vec![small.clone(), small, large], None),
            &catalog,
            None,
            now,
        );
        let floor = (known + small_cost + small_cost).to_f64();
        for estimate in [uses[0].estimate, uses[0].weeks.last().unwrap().estimate] {
            assert!(
                estimate.unwrap().usd >= floor - 1e-9,
                "{estimate:?} {floor}"
            );
        }
    }

    #[test]
    fn server_fees_without_a_recorded_model_stay_unknown() {
        let catalog = prices();
        let mut r = request(DAY, Some("claude-fable-5-2"), "profile");
        r.model = None;
        r.server_tools = Some(crate::model::ServerTools::default());
        assert!(fable_cost(&catalog, &r).unwrap().total_usd.is_some());
        r.server_tools = None;
        let cost = fable_cost(&catalog, &r).unwrap();
        assert_eq!(cost.total_usd, None);
        assert!(
            cost.unknown.iter().any(|u| u.contains("no recorded model")),
            "{:?}",
            cost.unknown
        );
    }

    #[test]
    fn no_requests_and_no_bound_leave_nothing_to_show() {
        assert_eq!(fable_use(&scan(vec![], None), &prices(), None, DAY), vec![]);
    }
}
