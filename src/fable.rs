//! Each Claude profile's Fable spend per week, for comparing with a plan's documented Fable cap.

use crate::model::{FableUse, FableWeek, Harness, Millis, Scan};
use crate::prices::{Catalog, Sum, price_request};

const WEEK: Millis = 7 * 24 * 60 * 60 * 1000;

/// Whether a recorded model ID is a Fable model, such as `claude-fable-5-2[1m]`.
fn is_fable(model: &str) -> bool {
    model.contains("fable")
}

/// Fable spend for every Claude profile that signs in to Anthropic rather than a cloud
/// provider, in 7-day spans counted back from `now` to the window's start.
///
/// A plan's weekly reset time is not recorded, so spans end at the report instead.
pub fn fable_use(scan: &Scan, catalog: &Catalog, now: Millis) -> Vec<FableUse> {
    let Some(start) = scan
        .since
        .or_else(|| scan.requests.iter().map(|r| r.time).min())
    else {
        return vec![];
    };
    let mut spans = vec![];
    let mut end = now;
    while end > start {
        spans.push((start.max(end - WEEK), end));
        end -= WEEK;
    }
    scan.profiles
        .iter()
        // Bedrock, Vertex and Foundry bill per token; no plan limit applies there.
        .filter(|p| p.harness == Harness::Claude && p.provider.is_none())
        .map(|p| {
            let mut sums: Vec<(u64, Sum)> = spans.iter().map(|_| (0, Sum::new())).collect();
            let mut unrecorded = 0;
            for r in scan
                .requests
                .iter()
                .filter(|r| r.harness == Harness::Claude && r.profiles.contains(&p.id))
            {
                match r.model.as_deref() {
                    None => unrecorded += 1,
                    Some(m) if is_fable(m) => {
                        // Spans are most recent first and end exclusive, except the latest.
                        let i = spans
                            .iter()
                            .position(|&(s, e)| r.time >= s && (r.time < e || e == now));
                        if let Some(i) = i {
                            sums[i].0 += 1;
                            sums[i].1.add(&price_request(catalog, r));
                        }
                    }
                    Some(_) => {}
                }
            }
            let weeks: Vec<FableWeek> = spans
                .iter()
                .zip(sums)
                .map(|(&(start, end), (requests, sum))| FableWeek {
                    start,
                    end,
                    requests,
                    cost: sum.cost(),
                })
                .collect();
            FableUse {
                profile: p.id.clone(),
                requests: weeks.iter().map(|w| w.requests).sum(),
                unrecorded,
                weeks,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CacheContext, ExposureScan, ScanCoverage};
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
        let uses = fable_use(&scan(requests, Some(3 * DAY)), &catalog, now);
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
    }

    #[test]
    fn no_requests_and_no_bound_leave_nothing_to_show() {
        assert_eq!(fable_use(&scan(vec![], None), &prices(), DAY), vec![]);
    }
}
