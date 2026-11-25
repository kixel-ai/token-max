//! What the hosts of one `doctor` run add up to: their requests, sessions, spend, offered
//! savings, and what their settings save against the defaults.
//!
//! Each host sums its own figures beside its report, so a destination's text still never
//! carries its scan. The hosts' figures are added as they are: a history copied between hosts
//! counts on each.

use serde::{Deserialize, Serialize};

use crate::defaults::Credit;
use crate::doctor::{Fix, How, Placed, Saved, Total, place};
use crate::model::{CheckResult, Estimate, HistoricalCost, Margin, Report, Request, Scan};
use crate::report::{Spend, spend_text};

/// One host's figures, or the sum of several.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub requests: u64,
    /// The input and output tokens the requests recorded, cache reads and writes included.
    pub tokens: u64,
    /// Sessions, each subagent counted with the session that spawned it.
    pub sessions: u64,
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
    /// What the settings save every 30 days against the defaults; None under `--defaults`,
    /// where every setting is its default.
    pub credit: Option<Credit>,
}

impl Totals {
    /// One host's report, the changes it offers, and what its settings save against the
    /// defaults, None under `--defaults`.
    pub fn new(report: &Report, fixes: &[Fix], credit: Option<Credit>) -> Self {
        let spend = Spend::new(report);
        let mut savings = Total::of(fixes);
        savings.set_share(spend.monthly());
        let Counts {
            requests,
            tokens,
            sessions,
            ..
        } = Counts::of(&report.scan, None);
        Totals {
            requests,
            tokens,
            sessions,
            spend: spend.usd,
            margin: spend.margin,
            floor: spend.floor,
            partial: spend.partial,
            monthly: spend.monthly(),
            savings,
            credit,
        }
    }

    /// Another host's figures added to these. Hosts are separate samples, so their margins
    /// combine in quadrature.
    pub fn merge(&mut self, other: &Totals) {
        self.requests += other.requests;
        self.tokens += other.tokens;
        self.sessions += other.sessions;
        self.spend += other.spend;
        self.margin = Margin::independent(self.margin, other.margin);
        self.floor += other.floor;
        self.partial |= other.partial;
        self.monthly += other.monthly;
        self.savings.merge(&other.savings);
        self.savings.set_share(self.monthly);
        // Every host runs with the same `--defaults`, so either both have a credit or neither.
        if let (Some(credit), Some(other)) = (&mut self.credit, &other.credit) {
            credit.merge(other);
        }
    }

    /// What these figures count, for a progress line.
    pub fn counts(&self) -> Counts {
        Counts {
            requests: self.requests,
            tokens: self.tokens,
            sessions: self.sessions,
            spend: Some(Spent {
                usd: self.spend,
                margin: self.margin,
                floor: self.floor,
                partial: self.partial,
            }),
        }
    }

    /// The line naming what the settings save against the defaults, each percent of the
    /// spend of `whose`, such as "these hosts'"; None under `--defaults`.
    pub fn credit(&self, whose: &str) -> Option<String> {
        let credit = self.credit.as_ref()?;
        Some(credit.text(self.monthly, whose))
    }
}

/// A change a check found, and the edit the walk would make for it, if any.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    pub saved: Saved,
    /// The walk lists an edit two lines make once, so it is counted once.
    pub edit: Option<How>,
}

impl Finding {
    /// The change saving `saved`, carried out as `how`.
    pub fn new(saved: Saved, how: How) -> Self {
        let edit = matches!(how, How::Edit(_)).then_some(how);
        Finding { saved, edit }
    }
}

/// What a host's checks have found so far, for a progress line while its settings are checked.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Checked {
    /// How many checks are done, of `of`.
    pub done: usize,
    pub of: usize,
    /// The changes found so far.
    found: Vec<Finding>,
    /// What `found` saves together, summed as each check lands so a redraw only reads it.
    savings: Total,
    /// What the settings checked so far save against the defaults; None under `--defaults`.
    /// The tools, skills and MCP servers already off join once the report is formatted.
    pub credit: Option<Credit>,
}

impl Checked {
    /// Nothing checked yet of `of` checks, with a credit unless every setting is its default.
    pub fn new(of: usize, defaults: bool) -> Self {
        Checked {
            of,
            credit: (!defaults).then(Credit::default),
            ..Checked::default()
        }
    }

    /// One check's `results` in `scan`, counted as their report lines and the walk count them.
    pub fn check(&mut self, results: &[CheckResult], scan: &Scan) {
        let mut found = vec![];
        for result in results {
            let (baseline, finding) = crate::report::check_tally(result, scan);
            if let (Some(credit), Some(baseline)) = (&mut self.credit, &baseline) {
                credit.add(baseline);
            }
            found.extend(finding);
        }
        self.find(found);
    }

    /// Changes found, joining those found before.
    pub fn find(&mut self, found: impl IntoIterator<Item = Finding>) {
        self.found.extend(found);
        self.savings = self.sum();
    }

    /// What the changes found so far save together, their shares unknown until the spend is
    /// summed.
    pub fn savings(&self) -> Total {
        self.savings
    }

    /// The changes found so far counted as the walk lists them: biggest saving first, an
    /// edit two lines make once, with the first line's saving, and an edit that wants a
    /// setting another line sets at another value as a step by hand.
    fn sum(&self) -> Total {
        let mut found: Vec<&Finding> = self.found.iter().collect();
        found.sort_by(|a, b| crate::report::by_saving(a.saved.saving, b.saved.saving));
        let manual = How::Manual {
            step: String::new(),
            reason: String::new(),
        };
        let mut listed: Vec<&How> = vec![];
        let mut total = Total::default();
        for finding in found {
            let how = finding.edit.as_ref().unwrap_or(&manual);
            match place(&listed, how) {
                Placed::Same(_) => continue,
                Placed::Conflicts(_) => listed.push(&manual),
                Placed::New => listed.push(how),
            }
            total.count(&finding.saved);
        }
        total
    }
}

/// One kind of figure after another as a progress line tallies them, each with its qualifier.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tallied {
    /// What was measured, estimates included.
    pub measured: Option<String>,
    /// What holds only while a default is the other value, saving and costing apart.
    pub up_to: Vec<String>,
    /// How many differ, or change, by amounts not measured, when none was measured.
    pub unmeasured: Option<String>,
}

/// What a progress line counts: requests, their tokens and sessions as a report's header
/// counts them, and what they cost once priced.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counts {
    pub requests: u64,
    pub tokens: u64,
    pub sessions: u64,
    /// None until prices load.
    pub spend: Option<Spent>,
}

impl Counts {
    /// What a scan's selected requests count to, as its report's header counts them, with
    /// what they cost when `historical` prices them.
    pub fn of(scan: &Scan, historical: Option<&[HistoricalCost]>) -> Self {
        Counts {
            requests: scan.coverage.requests,
            tokens: scan.requests.iter().map(tokens).sum(),
            sessions: crate::report::sessions(scan),
            spend: historical.map(Spent::of),
        }
    }

    /// Another host's counts added to these; the spend only while both have one.
    pub fn merge(&mut self, other: &Counts) {
        self.requests += other.requests;
        self.tokens += other.tokens;
        self.sessions += other.sessions;
        self.spend = match (&mut self.spend, &other.spend) {
            (Some(spend), Some(other)) => {
                spend.merge(other);
                Some(*spend)
            }
            _ => None,
        };
    }
}

/// What requests cost at API prices, extrapolated over the ones that could not be priced.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spent {
    /// The total, extrapolated over unpriced requests, plus `floor`.
    pub usd: f64,
    /// How far the extrapolated part may be off; None when nothing was extrapolated.
    pub margin: Option<Margin>,
    /// What the groups that priced none of their requests are known to cost, a lower bound
    /// with nothing to extrapolate from.
    pub floor: f64,
    /// Whether some group priced none of its requests, even one with no known charge.
    pub partial: bool,
}

impl Spent {
    /// What groups of requests cost, each group a profile's history.
    pub fn of(groups: &[HistoricalCost]) -> Self {
        Self::estimated(
            groups
                .iter()
                .map(|g| (g.estimate, g.cost.known_usd.to_f64())),
        )
    }

    /// What groups of requests cost from each group's estimate, None when it priced none of
    /// its requests, and what its requests are known to cost.
    pub fn estimated(groups: impl IntoIterator<Item = (Option<Estimate>, f64)>) -> Self {
        let mut usd = 0.0;
        // Each group is a separate sample, so their errors are independent and their margins
        // add in quadrature.
        let mut variance = 0.0;
        let (mut extrapolated, mut unmeasured, mut floor, mut partial) = (false, false, 0.0, false);
        for (estimate, known) in groups {
            match estimate {
                Some(estimate) => {
                    usd += estimate.usd;
                    match estimate.margin {
                        None => {}
                        Some(Margin::Usd(m)) => {
                            extrapolated = true;
                            variance += m * m;
                        }
                        Some(Margin::Unmeasured) => unmeasured = true,
                    }
                }
                None => {
                    usd += known;
                    floor += known;
                    partial = true;
                }
            }
        }
        Spent {
            usd,
            margin: if unmeasured {
                Some(Margin::Unmeasured)
            } else {
                extrapolated.then(|| Margin::Usd(variance.sqrt()))
            },
            floor,
            partial,
        }
    }

    /// Another host's spend added to this one, as [`Totals::merge`] adds it.
    pub fn merge(&mut self, other: &Spent) {
        self.usd += other.usd;
        self.margin = Margin::independent(self.margin, other.margin);
        self.floor += other.floor;
        self.partial |= other.partial;
    }

    /// The spend as a report's header writes it, such as "~$9,392 ± $4.72".
    pub fn text(&self) -> String {
        spend_text(self.usd, self.margin, self.floor, self.partial)
    }
}

/// The input and output tokens a request recorded; a count a record left out adds none.
pub fn tokens(r: &Request) -> u64 {
    r.parts
        .iter()
        .map(|p| {
            let u = &p.usage;
            // Input counts cache reads and writes; output counts reasoning.
            u.input.unwrap_or(0).max(0) as u64 + u.output.unwrap_or(0).max(0) as u64
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::spend_text;

    /// Two lines that make one edit, such as a compaction window for two profiles sharing a
    /// settings file, count once as the walk lists them: the bigger saving's line keeps it.
    /// Each line that wants another value for the setting is left by hand and counts.
    #[test]
    fn an_edit_two_checks_find_counts_once() {
        use crate::doctor::{Edit, Op, Patch};
        let edit = |value: i64| {
            How::Edit(Patch {
                file: "/home/u/.claude/settings.json".into(),
                harness: crate::model::Harness::Claude,
                edits: vec![Edit {
                    path: vec!["autoCompactWindow".into()],
                    value: value.into(),
                    op: Op::Set,
                }],
            })
        };
        let finding = |usd, how| {
            let saved = Saved {
                saving: Some(usd),
                margin: None,
                verb: "saves",
            };
            Finding::new(saved, how)
        };
        let manual = || How::Manual {
            step: "by hand".into(),
            reason: "synthetic".into(),
        };
        let mut checked = Checked::new(2, false);
        checked.find(vec![
            finding(20.0, edit(400_000)),
            finding(50.0, edit(400_000)),
            finding(7.0, edit(300_000)),
            // Steps by hand are never the same edit.
            finding(3.0, manual()),
            finding(3.0, manual()),
        ]);
        let tallied = checked.savings().tally(true);
        assert_eq!(tallied.measured.as_deref(), Some("found $63/mo more"));
        // Two lines that want the window the bigger saving's line doesn't are each left by
        // hand, so both count, as the walk lists them, even when a later check finds the
        // bigger saving.
        let mut checked = Checked::new(2, false);
        checked.find(vec![
            finding(20.0, edit(300_000)),
            finding(10.0, edit(300_000)),
        ]);
        let tallied = checked.savings().tally(true);
        assert_eq!(tallied.measured.as_deref(), Some("found $20/mo more"));
        checked.find(vec![finding(50.0, edit(400_000))]);
        let tallied = checked.savings().tally(true);
        assert_eq!(tallied.measured.as_deref(), Some("found $80/mo more"));
    }

    fn host(requests: u64, spend: f64, margin: Option<Margin>, monthly: f64) -> Totals {
        Totals {
            requests,
            tokens: requests * 10,
            sessions: requests / 10,
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
        assert_eq!(total.tokens, 1510);
        assert_eq!(total.sessions, 15);
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
    fn credits_add_up_and_take_their_share_of_every_hosts_spend() {
        use crate::defaults::{Baseline, Saving};
        let host = |monthly, usd| Totals {
            monthly,
            credit: Some(Credit::of(&[Baseline {
                default: "5m".into(),
                saving: Saving::Saves(usd),
                margin: None,
            }])),
            ..Totals::default()
        };
        // 10% of $1,000 and 3% of $3,000 are 5% of the $4,000 together.
        let mut total = host(1000.0, 100.0);
        total.merge(&host(3000.0, 100.0));
        assert_eq!(
            total.credit("these hosts'").unwrap(),
            "Compared with defaults, your settings save about $200/mo, 5% of these hosts' spend. Overlapping settings may save less."
        );
        // Under --defaults no host has one.
        assert_eq!(Totals::default().credit("this host's"), None);
    }

    #[test]
    fn a_history_on_two_hosts_counts_on_each() {
        let one = host(7, 12.5, None, 12.5);
        let mut total = one.clone();
        total.merge(&one);
        assert_eq!((total.requests, total.sessions, total.spend), (14, 0, 25.0));
    }
}
