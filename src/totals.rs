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
