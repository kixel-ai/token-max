//! What the hosts of one `doctor` run add up to: their requests, spend and offered savings.
//!
//! Each host sums its own figures beside its report, so a destination's text still never
//! carries its scan. The hosts' figures are added as they are: a history copied between hosts
//! counts on each.

use serde::{Deserialize, Serialize};

use crate::doctor::{Fix, Total};
use crate::model::{Margin, Report};
use crate::report::Spend;

/// One host's figures, or the sum of several.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub requests: u64,
    /// What the requests cost at API prices, extrapolated over unpriced ones, plus `floor`.
    pub spend: f64,
    /// How far `spend` may be off; None when nothing was extrapolated.
    pub margin: Option<Margin>,
    /// What profiles that priced none of their requests are known to cost.
    pub floor: f64,
    /// Whether some profile priced none of its requests.
    pub partial: bool,
    /// What the hosts spend every 30 days at their windows' rates, the base of a saving's share.
    pub monthly: f64,
    /// What the changes the report offers save every 30 days.
    pub savings: Total,
}

impl Totals {
    /// One host's report and the changes it offers.
    pub fn new(report: &Report, fixes: &[Fix]) -> Self {
        let spend = Spend::new(report);
        let mut savings = Total::of(fixes);
        savings.set_share(spend.monthly());
        Totals {
            requests: report.scan.coverage.requests,
            spend: spend.usd,
            margin: spend.margin,
            floor: spend.floor,
            partial: spend.partial,
            monthly: spend.monthly(),
            savings,
        }
    }

    /// Another host's figures added to these. Hosts are separate samples, so their margins
    /// combine in quadrature.
    pub fn merge(&mut self, other: &Totals) {
        self.requests += other.requests;
        self.spend += other.spend;
        self.margin = Margin::independent(self.margin, other.margin);
        self.floor += other.floor;
        self.partial |= other.partial;
        self.monthly += other.monthly;
        self.savings.merge(&other.savings);
        self.savings.set_share(self.monthly);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::spend_text;

    fn host(requests: u64, spend: f64, margin: Option<Margin>, monthly: f64) -> Totals {
        Totals {
            requests,
            spend,
            margin,
            monthly,
            ..Totals::default()
        }
    }

    #[test]
    fn hosts_sum_as_they_are_with_independent_margins() {
        let mut total = host(100, 1000.0, Some(Margin::Usd(30.0)), 600.0);
        total.merge(&host(50, 500.0, Some(Margin::Usd(40.0)), 200.0));
        // A host priced in full adds no margin of its own.
        total.merge(&host(1, 10.0, None, 5.0));
        assert_eq!(total.requests, 151);
        assert_eq!(total.spend, 1510.0);
        assert_eq!(total.monthly, 805.0);
        // 30 and 40 combine as 3-4-5.
        assert_eq!(total.margin, Some(Margin::Usd(50.0)));
        assert_eq!(
            spend_text(total.spend, total.margin, total.floor, total.partial),
            "~$1,510 ± $50"
        );
    }

    #[test]
    fn a_host_with_no_spread_leaves_the_margin_unknown_and_a_floor_stays_a_floor() {
        let mut total = host(10, 100.0, Some(Margin::Usd(5.0)), 10.0);
        total.merge(&host(10, 20.0, Some(Margin::Unmeasured), 10.0));
        assert_eq!(total.margin, Some(Margin::Unmeasured));
        let mut priced_none = host(5, 7.0, None, 1.0);
        priced_none.floor = 7.0;
        priced_none.partial = true;
        let mut total = host(10, 100.0, None, 10.0);
        total.merge(&priced_none);
        assert_eq!(
            spend_text(total.spend, total.margin, total.floor, total.partial),
            "$100 plus at least $7"
        );
    }

    #[test]
    fn a_history_on_two_hosts_counts_on_each() {
        let one = host(7, 12.5, None, 12.5);
        let mut total = one.clone();
        total.merge(&one);
        assert_eq!((total.requests, total.spend), (14, 25.0));
    }
}
