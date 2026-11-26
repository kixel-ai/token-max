//! Inline interactive terminal UI for `token-max doctor` and `token-max vibes`.
//!
//! Renders an inline viewport in normal scrollback (no alternate screen) with synchronized
//! updates, safe-default pre-selection, live Plan/API billing recalculation, real measured
//! ASCII proof charts, and a two-keypress path (`Enter` -> `Enter`) from findings to
//! confirmed atomic writes.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crossterm::cursor::{Hide, MoveToColumn, MoveUp, Show};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, disable_raw_mode,
    enable_raw_mode,
};

use crate::doctor::{
    Event, Fix, How, Kind, Proof, Reason, Style, Walking, home_in, section_value, target, why,
};
use crate::model::ContextKind;
use crate::report::{UP_TO, about, share_text};
use crate::safe::{ScanError, terminal_text};
use crate::terminal::{WrapOptions, cells, fit, wrap_text};
use crate::vibes::Item as VibeItem;

/// ANSI style codes used when `color` is enabled.
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const BRAND: &str = "\x1b[35m";

/// Minimum monthly saving (USD) to appear unfolded in the main checklist.
const MINOR_THRESHOLD_USD: f64 = 1.0;

/// Summary figures displayed in the Hero Savings Card.
#[derive(Clone, Debug, Default)]
pub(crate) struct Hero {
    pub requests: u64,
    pub tokens: u64,
    pub sessions: u64,
    pub spend_text: String,
    pub monthly_spend: f64,
    pub credit: Option<String>,
    pub caveats: Vec<String>,
    pub host_count: usize,
    pub vibes_pending: usize,
    /// True under `--defaults`: interactive read-only preview that never edits files.
    pub read_only: bool,
    /// Already-good settings (shown with green `✓` in the Measured scorecard).
    pub kept: Vec<KeptItem>,
    /// Count of remaining in-budget tools/skills folded into a single summary line.
    pub kept_features: usize,
}

/// One already-good setting shown with a green `✓` in the Measured checklist.
#[derive(Clone, Debug)]
pub(crate) struct KeptItem {
    pub host_label: Option<String>,
    pub name: String,
    pub value: String,
    pub saving_usd: Option<f64>,
    pub default_value: Option<String>,
    pub summary: String,
}

/// Billing mode for live recalculation of unset Claude main-agent prompt cache TTLs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BillingMode {
    /// Leave unset main TTLs as "up to $X/mo" (covers both Plan and API).
    Auto,
    /// Assume API key or usage credits (unset main TTL defaults to 5m).
    Api,
    /// Assume Claude Pro/Max included usage (unset main TTL defaults to 1h).
    Plan,
}

impl BillingMode {
    fn next(self) -> Self {
        match self {
            BillingMode::Auto => BillingMode::Api,
            BillingMode::Api => BillingMode::Plan,
            BillingMode::Plan => BillingMode::Auto,
        }
    }

    fn label(self) -> &'static str {
        match self {
            BillingMode::Auto => "Auto",
            BillingMode::Api => "API/Credits",
            BillingMode::Plan => "Plan (1h)",
        }
    }
}

/// Which tab is active in the inline checklist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tab {
    Measured,
    Vibes,
}

/// Current screen inside the inline viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    List,
    Diff,
}

/// Prepared colored diff preview for the Diff confirmation screen.
#[derive(Clone, Debug, Default)]
pub(crate) struct DiffPreview {
    pub text: String,
    pub file_count: usize,
    pub change_count: usize,
    pub backups: Vec<String>,
}

/// Outcome of writing the reviewed edits across hosts.
#[derive(Clone, Debug, Default)]
pub(crate) struct WriteResult {
    pub files_written: usize,
    pub lines: Vec<String>,
}

/// One vibes item projected for the Vibes tab.
#[derive(Clone, Debug)]
pub(crate) struct VibeRow {
    pub host: usize,
    pub host_label: Option<String>,
    pub item: VibeItem,
    /// Index into the host's vibes `Offers::offers`, when editable.
    pub offer_index: Option<usize>,
    pub checked: bool,
}

/// Host callbacks connecting the pure TUI state to local/SSH file planning and writing.
pub(crate) trait Bridge {
    /// Preview exact diffs for `events` (one `Vec<Event>` per host) on the measured changes.
    fn preview_measured(&mut self, events: &[Vec<Event>]) -> Result<DiffPreview, ScanError>;
    /// Atomically write the last previewed measured selections.
    fn write_measured(&mut self) -> Result<WriteResult, ScanError>;
    /// Load vibes entries (lazy on first switch to Vibes tab, or immediate for `vibes` command).
    fn load_vibes(&mut self) -> Result<Vec<VibeRow>, ScanError>;
    /// Preview exact diffs for the currently checked vibes `rows`.
    fn preview_vibes(&mut self, rows: &[VibeRow]) -> Result<DiffPreview, ScanError>;
    /// Atomically write the last previewed vibes selections.
    fn write_vibes(&mut self) -> Result<WriteResult, ScanError>;
}

/// What happened when the interactive TUI exited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExitKind {
    /// Completed normally (either applied changes or user quit with `q`).
    Done,
    /// Interrupted by Ctrl-C; caller should exit with code 2.
    Cancelled,
}

/// One member inside a grouped checklist item.
#[derive(Clone, Debug)]
struct MemberItem {
    host: usize,
    offer_index: usize,
    fix: Fix,
    entries_count: usize,
    denials_count: usize,
    checked: bool,
}

/// One already-turned-off MCP server cleanup entry.
#[derive(Clone, Debug)]
struct CleanupItem {
    host: usize,
    host_label: Option<String>,
    entry_index: usize,
    server: String,
    checked: bool,
}

/// One top-level row in the Measured checklist.
#[derive(Clone, Debug)]
struct Group {
    host: usize,
    host_label: Option<String>,
    members: Vec<MemberItem>,
    /// Expanded into individual member rows (`s`).
    split: bool,
    /// Minor (`<$1/mo` measured) row folded by default.
    minor: bool,
}

/// Address of a focusable row in the visible list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Group(usize),
    Member(usize, usize),
    Cleanup(usize),
    Kept(usize),
    Vibe(usize),
}

/// Complete interactive TUI state, cleanly testable without a terminal.
pub(crate) struct App {
    pub tab: Tab,
    start_vibes_only: bool,
    screen: Screen,
    hero: Hero,
    billing: BillingMode,
    has_unset_main_ttl: bool,
    groups: Vec<Group>,
    cleanups: Vec<CleanupItem>,
    by_hand: Vec<(Option<String>, String, String, String)>,
    vibes: Option<Vec<VibeRow>>,
    focus: usize,
    expanded: Option<Focus>,
    show_minor: bool,
    show_caveats: bool,
    diff: Option<DiffPreview>,
    diff_scroll: usize,
    diff_tab: Tab,
    status_error: Option<String>,
    host_count: usize,
    color: bool,
    unicode: bool,
}

impl App {
    /// Build initial interactive state from every host's offered changes and hero totals.
    pub(crate) fn new(
        hosts: &[Walking<'_>],
        hero: Hero,
        start_tab: Tab,
        color: bool,
        unicode: bool,
    ) -> Self {
        let mut groups: Vec<Group> = vec![];
        let mut cleanups: Vec<CleanupItem> = vec![];
        let mut by_hand: Vec<(Option<String>, String, String, String)> = vec![];
        let mut has_unset_main_ttl = false;

        for (host_idx, walking) in hosts.iter().enumerate() {
            let label = walking.label.map(str::to_string);
            for (offer_index, offer) in walking.offers.offers.iter().enumerate() {
                let fix = &offer.fix;
                if is_unset_main_ttl(fix) {
                    has_unset_main_ttl = true;
                }
                let member = MemberItem {
                    host: host_idx,
                    offer_index,
                    fix: fix.clone(),
                    entries_count: offer.entries.len(),
                    denials_count: offer.denials.len(),
                    checked: false,
                };
                // Costly tradeoffs (e.g. a single $87/mo skill like `dataviz`) stay ungrouped
                // as their own top-level rows with their own bar graph so huge outliers stand out.
                let keep_individual = matches!(fix.section.kind, Kind::Feature(_, Reason::Costly));
                if !keep_individual
                    && let Some(existing) = groups
                        .iter_mut()
                        .find(|g| g.host == host_idx && g.members[0].fix.joins(fix))
                {
                    existing.members.push(member);
                } else {
                    groups.push(Group {
                        host: host_idx,
                        host_label: label.clone(),
                        members: vec![member],
                        split: false,
                        minor: false,
                    });
                }
            }
            for (entry_index, (_, entry)) in walking.offers.cleanup.iter().enumerate() {
                cleanups.push(CleanupItem {
                    host: host_idx,
                    host_label: label.clone(),
                    entry_index,
                    server: entry.server.clone(),
                    checked: false,
                });
            }
            for fix in &walking.offers.by_hand {
                if let How::Manual { step, reason } = &fix.how {
                    by_hand.push((
                        label.clone(),
                        fix.name.clone(),
                        home_in(step, walking.style.home.as_deref()),
                        reason.clone(),
                    ));
                }
            }
        }

        // Rank members inside each group and groups overall by largest saving first.
        for g in &mut groups {
            g.members
                .sort_by(|a, b| match (a.fix.saving, b.fix.saving) {
                    (Some(x), Some(y)) => y.total_cmp(&x),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                });
            let usd = group_usd(g, BillingMode::Auto);
            let is_minor = usd.is_some_and(|u| u < MINOR_THRESHOLD_USD);
            g.minor = is_minor;
            // Smart default pre-selection: safe measured wins (`!suggested`, >= $1/mo) pre-checked.
            let precheck = !is_minor && usd.is_some() && g.members.iter().any(|m| !m.fix.suggested);
            for m in &mut g.members {
                m.checked = precheck && !m.fix.suggested;
            }
        }
        groups.sort_by(|a, b| {
            match (
                group_usd(a, BillingMode::Auto),
                group_usd(b, BillingMode::Auto),
            ) {
                (Some(x), Some(y)) => y.total_cmp(&x),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
        });

        Self {
            tab: start_tab,
            start_vibes_only: start_tab == Tab::Vibes,
            screen: Screen::List,
            hero,
            billing: BillingMode::Auto,
            has_unset_main_ttl,
            groups,
            cleanups,
            by_hand,
            vibes: None,
            focus: 0,
            expanded: None,
            show_minor: false,
            show_caveats: false,
            diff: None,
            diff_scroll: 0,
            diff_tab: start_tab,
            status_error: None,
            host_count: hosts.len(),
            color,
            unicode,
        }
    }

    /// Whether there are zero interactive items to prompt on in Measured mode.
    pub(crate) fn is_all_clear(&self) -> bool {
        self.tab == Tab::Measured
            && self.groups.is_empty()
            && self.cleanups.is_empty()
            && self.hero.vibes_pending == 0
    }

    /// Total monthly USD across all measured items (under current billing mode).
    pub(crate) fn total_available_usd(&self) -> f64 {
        self.groups
            .iter()
            .filter_map(|g| group_usd(g, self.billing))
            .sum()
    }

    /// Monthly USD across currently checked measured items (under current billing mode).
    pub(crate) fn selected_usd(&self) -> f64 {
        self.groups
            .iter()
            .flat_map(|g| &g.members)
            .filter(|m| m.checked)
            .filter_map(|m| effective_saving(&m.fix, self.billing))
            .sum()
    }

    /// Number of checked top-level items on the active tab.
    pub(crate) fn checked_count(&self) -> usize {
        match self.tab {
            Tab::Measured => {
                self.groups
                    .iter()
                    .filter(|g| g.members.iter().any(|m| m.checked))
                    .count()
                    + self.cleanups.iter().filter(|c| c.checked).count()
            }
            Tab::Vibes => self
                .vibes
                .as_deref()
                .map_or(0, |v| v.iter().filter(|r| r.checked).count()),
        }
    }

    /// Number of minor (`<$1/mo`) groups and their combined monthly saving.
    fn minor_summary(&self) -> (usize, f64) {
        let mut count = 0;
        let mut usd = 0.0;
        for g in &self.groups {
            if g.minor {
                count += g.members.len();
                usd += group_usd(g, self.billing).unwrap_or(0.0);
            }
        }
        (count, usd)
    }

    /// Visible focusable rows on the current tab.
    fn visible_rows(&self) -> Vec<Focus> {
        match self.tab {
            Tab::Measured => {
                let mut rows = vec![];
                for (gi, g) in self.groups.iter().enumerate() {
                    if g.minor && !self.show_minor {
                        continue;
                    }
                    rows.push(Focus::Group(gi));
                    if g.split {
                        for mi in 0..g.members.len() {
                            rows.push(Focus::Member(gi, mi));
                        }
                    }
                }
                for ci in 0..self.cleanups.len() {
                    rows.push(Focus::Cleanup(ci));
                }
                for ki in 0..self.hero.kept.len() {
                    rows.push(Focus::Kept(ki));
                }
                rows
            }
            Tab::Vibes => self
                .vibes
                .as_deref()
                .map(|v| {
                    v.iter()
                        .enumerate()
                        .filter(|(_, r)| !r.item.quiet || self.show_minor)
                        .map(|(i, _)| Focus::Vibe(i))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    fn clamp_focus(&mut self) {
        let len = self.visible_rows().len();
        if len == 0 {
            self.focus = 0;
        } else if self.focus >= len {
            self.focus = len - 1;
        }
    }

    fn current_focus(&self) -> Option<Focus> {
        self.visible_rows().get(self.focus).copied()
    }

    /// Toggle the currently focused item.
    pub(crate) fn toggle_focused(&mut self) {
        let Some(target) = self.current_focus() else {
            return;
        };
        match target {
            Focus::Group(gi) => {
                let g = &mut self.groups[gi];
                let all_checked = g.members.iter().all(|m| m.checked);
                let next = !all_checked;
                for m in &mut g.members {
                    m.checked = next;
                }
            }
            Focus::Member(gi, mi) => {
                let m = &mut self.groups[gi].members[mi];
                m.checked = !m.checked;
            }
            Focus::Cleanup(ci) => {
                self.cleanups[ci].checked = !self.cleanups[ci].checked;
            }
            Focus::Kept(_) => {}
            Focus::Vibe(vi) => {
                if let Some(vibes) = &mut self.vibes
                    && let Some(row) = vibes.get_mut(vi)
                    && row.offer_index.is_some()
                {
                    row.checked = !row.checked;
                }
            }
        }
    }

    /// Toggle all safe wins (non-suggested major items) on or off.
    pub(crate) fn toggle_all_safe(&mut self) {
        if self.tab != Tab::Measured {
            return;
        }
        let any_safe_unchecked = self.groups.iter().any(|g| {
            !g.minor
                && g.members.iter().any(|m| {
                    !m.fix.suggested
                        && effective_saving(&m.fix, self.billing).is_some_and(|u| u > 0.0)
                        && !m.checked
                })
        });
        for g in &mut self.groups {
            for m in &mut g.members {
                if any_safe_unchecked {
                    m.checked = !g.minor
                        && !m.fix.suggested
                        && effective_saving(&m.fix, self.billing).is_some_and(|u| u > 0.0);
                } else {
                    m.checked = false;
                }
            }
        }
    }

    /// Cycle live billing mode (`Auto -> API -> Plan`) and re-sync unset main TTL check states.
    pub(crate) fn cycle_billing(&mut self) {
        if !self.has_unset_main_ttl {
            return;
        }
        self.billing = self.billing.next();
        for g in &mut self.groups {
            for m in &mut g.members {
                if is_unset_main_ttl(&m.fix) {
                    let eff = effective_saving(&m.fix, self.billing).unwrap_or(0.0);
                    m.checked = eff >= MINOR_THRESHOLD_USD && !m.fix.suggested;
                }
            }
        }
    }

    /// Build `Vec<Event>` per host for the currently checked Measured items.
    pub(crate) fn measured_events(&self) -> Vec<Vec<Event>> {
        let mut events: Vec<Vec<Event>> = (0..self.host_count).map(|_| vec![]).collect();
        for g in &self.groups {
            for m in &g.members {
                events[m.host].push(Event::Choose {
                    offer: m.offer_index,
                    yes: m.checked,
                });
                if m.checked {
                    for entry in 0..m.entries_count {
                        events[m.host].push(Event::Remove {
                            offer: m.offer_index,
                            entry,
                        });
                    }
                    for name in 0..m.denials_count {
                        events[m.host].push(Event::Deny {
                            offer: m.offer_index,
                            name,
                        });
                    }
                }
            }
        }
        for c in &self.cleanups {
            if c.checked {
                events[c.host].push(Event::Cleanup {
                    entry: c.entry_index,
                });
            }
        }
        events
    }

    /// Render the current frame as terminal lines, each guaranteed `<= width` cells and
    /// total line count `<= max_rows`.
    pub(crate) fn render(&self, width: usize, max_rows: usize) -> Vec<String> {
        let w = width.clamp(48, 104);
        let mut lines = match self.screen {
            Screen::List => self.render_list(w, max_rows),
            Screen::Diff => self.render_diff(w, max_rows),
        };
        let cap = max_rows.max(8);
        if lines.len() > cap {
            lines.truncate(cap);
        }
        lines.into_iter().map(|l| trim_ansi(&l, w)).collect()
    }

    fn style(&self, code: &str, text: &str) -> String {
        if self.color && !code.is_empty() {
            format!("{code}{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    fn arrow(&self) -> &'static str {
        if self.unicode { "→" } else { "->" }
    }

    fn sep(&self) -> &'static str {
        if self.unicode { " · " } else { " | " }
    }

    fn fit_text(&self, text: &str, max_cells: usize) -> String {
        fit_plain(text, max_cells, self.unicode)
    }

    /// Render the Hero Savings Card with generous padding and clean wrapping (never cuts off mid-sentence).
    fn render_hero(&self, w: usize) -> Vec<String> {
        let box_w = w.saturating_sub(2);
        let inner = box_w.saturating_sub(6); // 2-space inner margin on both sides
        let (tl, tr, bl, br, hz, vt) = if self.unicode {
            ("╭", "╮", "╰", "╯", "─", "│")
        } else {
            ("+", "+", "+", "+", "-", "|")
        };
        let border = |s: &str| self.style(DIM, s);
        let mut out = vec![
            String::new(),
            format!(
                " {}{}{}",
                border(tl),
                border(&hz.repeat(box_w.saturating_sub(2))),
                border(tr)
            ),
        ];

        let mut body_lines: Vec<String> = vec![];
        match self.tab {
            Tab::Measured => {
                let sel = self.selected_usd();
                let avail = self.total_available_usd();
                let pct = if self.hero.monthly_spend > 0.0 && sel > 0.0 {
                    format!(" ({} of spend)", share_text(sel / self.hero.monthly_spend))
                } else {
                    String::new()
                };
                let mut pills = vec![];
                if self.hero.read_only {
                    pills.push("--defaults · read-only".to_string());
                }
                if self.has_unset_main_ttl {
                    pills.push(format!("[b] Billing: {}", self.billing.label()));
                }
                let right_pill = pills.join("  ");
                let left_headline = if self.groups.is_empty() && self.cleanups.is_empty() {
                    let ok = if self.unicode { "✓" } else { "OK" };
                    format!(
                        "{} {}",
                        self.style(&format!("{BOLD}{GREEN}"), &format!("{ok} All clear")),
                        self.style(DIM, "no changes recommended")
                    )
                } else {
                    let save_str = if sel > 0.0 {
                        if self.hero.read_only {
                            format!("Potential ~{}/mo", about(sel))
                        } else {
                            format!("Save ~{}/mo", about(sel))
                        }
                    } else {
                        "Select changes to preview".into()
                    };
                    format!(
                        "{}{}",
                        self.style(&format!("{BOLD}{GREEN}"), &save_str),
                        self.style(DIM, &pct)
                    )
                };
                if right_pill.is_empty() {
                    body_lines.push(left_headline);
                } else {
                    let right = self.style(BRAND, &right_pill);
                    if ansi_cells(&left_headline) + ansi_cells(&right) + 2 <= inner {
                        let gap = inner - ansi_cells(&left_headline) - ansi_cells(&right);
                        body_lines.push(format!("{left_headline}{}{right}", " ".repeat(gap)));
                    } else {
                        body_lines.push(left_headline);
                        body_lines.push(right);
                    }
                }

                let mut parts = vec![];
                if self.hero.sessions > 0 {
                    parts.push(format!("{} sessions", commas(self.hero.sessions)));
                }
                if self.hero.requests > 0 {
                    parts.push(format!("{} reqs", commas(self.hero.requests)));
                }
                if !self.hero.spend_text.is_empty() {
                    parts.push(format!("{} API value", self.hero.spend_text));
                }
                if self.hero.host_count > 1 {
                    parts.push(format!("{} hosts", self.hero.host_count));
                }
                if !parts.is_empty() {
                    for wrapped in wrap_text(
                        &parts.join(self.sep()),
                        WrapOptions {
                            width: Some(inner),
                            ..Default::default()
                        },
                    )
                    .lines()
                    {
                        body_lines.push(self.style(DIM, wrapped));
                    }
                }

                if avail > 0.0 {
                    let ratio = (sel / avail).clamp(0.0, 1.0);
                    let pct_found = (ratio * 100.0).round() as usize;
                    let label_left = "Queued ";
                    let label_right = format!(" {pct_found}% of ~{}/mo found", about(avail));
                    let bar_w = inner
                        .saturating_sub(cells(label_left) + cells(&label_right))
                        .clamp(10, 36);
                    let filled = ((ratio * bar_w as f64).round() as usize).min(bar_w);
                    let rest = bar_w - filled;
                    let (fch, rch) = if self.unicode {
                        ("█", "░")
                    } else {
                        ("#", "-")
                    };
                    body_lines.push(format!(
                        "{}{}{}{}",
                        self.style(DIM, label_left),
                        self.style(GREEN, &fch.repeat(filled)),
                        self.style(YELLOW, &rch.repeat(rest)),
                        self.style(DIM, &label_right),
                    ));
                }

                if let Some(credit) = &self.hero.credit {
                    let col = if avail > 0.0 { DIM } else { GREEN };
                    for wrapped in wrap_text(
                        credit,
                        WrapOptions {
                            width: Some(inner),
                            ..Default::default()
                        },
                    )
                    .lines()
                    {
                        body_lines.push(self.style(col, wrapped));
                    }
                }
            }
            Tab::Vibes => {
                let count = self.vibes.as_deref().map_or(self.hero.vibes_pending, |v| {
                    v.iter().filter(|r| !r.item.quiet).count()
                });
                body_lines.push(format!(
                    "{} {}",
                    self.style(&format!("{BOLD}{BRAND}"), "token-max vibes"),
                    self.style(
                        DIM,
                        &format!("{count} unmeasured tips{}no verdict", self.sep())
                    )
                ));
                body_lines.push(self.style(
                    DIM,
                    "Tips transcripts can't price. Unchecked by default — Space to opt in.",
                ));
            }
        }

        for line in body_lines {
            let padded = pad_ansi(&line, inner);
            out.push(format!(" {}  {padded}  {}", border(vt), border(vt)));
        }

        out.push(format!(
            " {}{}{}",
            border(bl),
            border(&hz.repeat(box_w.saturating_sub(2))),
            border(br)
        ));
        out.push(String::new());
        out
    }

    fn render_list(&self, w: usize, max_rows: usize) -> Vec<String> {
        let mut head = self.render_hero(w);

        // Optional tab strip when both Measured and Vibes are relevant.
        if !self.start_vibes_only && (self.hero.vibes_pending > 0 || self.vibes.is_some()) {
            let m_cnt = self.groups.len() + self.cleanups.len();
            let v_cnt = self.vibes.as_deref().map_or(self.hero.vibes_pending, |v| {
                v.iter().filter(|r| !r.item.quiet).count()
            });
            let m_lbl = format!(" Measured ({m_cnt}) ");
            let v_lbl = format!(" Vibes ({v_cnt}) ");
            let m_tab = if self.tab == Tab::Measured {
                self.style(&format!("{BOLD}{BRAND}"), &format!("[{m_lbl}]"))
            } else {
                self.style(DIM, &m_lbl)
            };
            let v_tab = if self.tab == Tab::Vibes {
                self.style(&format!("{BOLD}{BRAND}"), &format!("[{v_lbl}]"))
            } else {
                self.style(DIM, &v_lbl)
            };
            let hint = self.style(DIM, "  Tab/v switch");
            head.push(trim_ansi(&format!("  {m_tab}  {v_tab}{hint}"), w));
            head.push(String::new());
        }

        // Section header
        let has_changes =
            self.groups.iter().any(|g| !g.minor || self.show_minor) || !self.cleanups.is_empty();
        let hdr_right = if self.tab == Tab::Measured {
            "EST. / 30D"
        } else {
            ""
        };
        let hdr_raw = match self.tab {
            Tab::Measured if has_changes && w >= 60 => {
                let x = if self.unicode { "✗" } else { "x" };
                format!("CHANGES{}{x} fix{}! tradeoff", self.sep(), self.sep())
            }
            Tab::Measured if has_changes => "CHANGES".into(),
            Tab::Measured => format!("ALREADY GOOD{}NO CHANGES NEEDED", self.sep()),
            Tab::Vibes => format!("TIPS & PICKS{}NO VERDICT", self.sep()),
        };
        let hdr_budget = w.saturating_sub(4 + cells(hdr_right));
        let hdr_left = self.fit_text(&hdr_raw, hdr_budget);
        let gap = w
            .saturating_sub(4 + cells(&hdr_left) + cells(hdr_right))
            .max(1);
        head.push(self.style(DIM, &format!("  {hdr_left}{}{hdr_right}", " ".repeat(gap))));
        head.push(String::new());

        // Build footer first so we know exact vertical budget for scrollable list body.
        let mut foot = vec![];
        if self.tab == Tab::Measured {
            let (minor_cnt, minor_usd) = self.minor_summary();
            if minor_cnt > 0 {
                let tri = if self.show_minor {
                    if self.unicode { "▾" } else { "v" }
                } else if self.unicode {
                    "▸"
                } else {
                    ">"
                };
                let left = format!(
                    "  {tri} {minor_cnt} minor tweak{} under $1/mo (+{}/mo)",
                    if minor_cnt == 1 { "" } else { "s" },
                    about(minor_usd)
                );
                let right = if self.show_minor {
                    "[m] fold"
                } else {
                    "[m] show"
                };
                let gap = w.saturating_sub(cells(&left) + cells(right) + 2).max(1);
                foot.push(self.style(DIM, &format!("{left}{}{right}", " ".repeat(gap))));
            }
            if !self.by_hand.is_empty() {
                foot.push(self.style(
                    DIM,
                    &self.fit_text(
                        &format!(
                            "  ! {} step{} by hand (shown after writing)",
                            self.by_hand.len(),
                            if self.by_hand.len() == 1 { "" } else { "s" }
                        ),
                        w,
                    ),
                ));
            }
        } else if let Some(vibes) = &self.vibes {
            let quiet_cnt = vibes.iter().filter(|r| r.item.quiet).count();
            if quiet_cnt > 0 {
                let tri = if self.show_minor {
                    if self.unicode { "▾" } else { "v" }
                } else if self.unicode {
                    "▸"
                } else {
                    ">"
                };
                let left = format!(
                    "  {tri} {quiet_cnt} already-aligned item{}",
                    if quiet_cnt == 1 { "" } else { "s" }
                );
                let right = if self.show_minor {
                    "[m] hide"
                } else {
                    "[m] show"
                };
                let gap = w.saturating_sub(cells(&left) + cells(right) + 2).max(1);
                foot.push(self.style(DIM, &format!("{left}{}{right}", " ".repeat(gap))));
            }
        }

        if self.show_caveats && !self.hero.caveats.is_empty() {
            foot.push(String::new());
            let bullet = if self.unicode { "·" } else { "-" };
            for c in &self.hero.caveats {
                for wrapped in wrap_text(
                    &format!("{bullet} {c}"),
                    WrapOptions {
                        width: Some(w.saturating_sub(4)),
                        indent: 2,
                        continuation: Some(4),
                    },
                )
                .lines()
                {
                    foot.push(self.style(DIM, wrapped));
                }
            }
        }

        if let Some(err) = &self.status_error {
            foot.push(String::new());
            foot.push(self.style(RED, &self.fit_text(&format!("  ! {err}"), w)));
        }

        // Keybind bar with generous top separation.
        foot.push(String::new());
        let rule = if self.unicode { "─" } else { "-" };
        foot.push(self.style(DIM, &format!(" {}", rule.repeat(w.saturating_sub(2)))));
        let checked = self.checked_count();
        let primary = if self.hero.read_only {
            self.style(&format!("{BOLD}{BRAND}"), "[q] Exit preview")
        } else if checked > 0 {
            self.style(
                &format!("{BOLD}{GREEN}"),
                &format!("[Enter] Review diff ({checked})"),
            )
        } else {
            self.style(DIM, "[Enter] Done")
        };
        let mut keys = vec![primary];
        if !matches!(self.current_focus(), Some(Focus::Kept(_))) {
            keys.push(self.style(DIM, "[Space] toggle"));
        }
        if self.tab == Tab::Measured {
            keys.push(self.style(DIM, &format!("[{}] proof", self.arrow())));
            if let Some(Focus::Group(gi)) = self.current_focus()
                && self.groups[gi].members.len() > 1
            {
                keys.push(self.style(DIM, "[s] split"));
            }
            if !self.hero.caveats.is_empty() {
                keys.push(self.style(DIM, "[?] caveats"));
            }
        } else {
            keys.push(self.style(DIM, &format!("[{}] details", self.arrow())));
        }
        if !self.hero.read_only {
            keys.push(self.style(DIM, "[q] quit"));
        }
        foot.push(trim_ansi(&format!("  {}", keys.join("  ")), w));

        // Build body rows with focus tracking and vertical windowing.
        let visible = self.visible_rows();
        let max_usd = self
            .groups
            .iter()
            .filter_map(|g| group_usd(g, self.billing))
            .fold(0.0_f64, f64::max);
        let max_kept_usd = self
            .hero
            .kept
            .iter()
            .filter_map(|k| k.saving_usd)
            .fold(0.0_f64, f64::max);

        let mut body_blocks: Vec<(bool, Vec<String>)> = vec![];
        if visible.is_empty() {
            let msg = match self.tab {
                Tab::Measured => "  No changes to apply — your settings look good.",
                Tab::Vibes => "  No actionable vibes tips for these profiles.",
            };
            body_blocks.push((false, vec![self.style(DIM, msg)]));
        } else {
            let mut emitted_kept_hdr = false;
            for (idx, item) in visible.iter().enumerate() {
                let focused = idx == self.focus;
                let expanded = self.expanded == Some(*item);
                let mut lines = vec![];
                if matches!(item, Focus::Kept(_)) && !emitted_kept_hdr {
                    emitted_kept_hdr = true;
                    if has_changes {
                        lines.push(self.style(DIM, "  ALREADY GOOD"));
                        lines.push(String::new());
                    }
                }
                match *item {
                    Focus::Group(gi) => {
                        lines.extend(self.render_group_row(gi, focused, expanded, max_usd, w));
                    }
                    Focus::Member(gi, mi) => {
                        lines.extend(self.render_member_row(gi, mi, focused, expanded, max_usd, w));
                    }
                    Focus::Cleanup(ci) => {
                        lines.extend(self.render_cleanup_row(ci, focused, expanded, w));
                    }
                    Focus::Kept(ki) => {
                        lines.extend(self.render_kept_row(ki, focused, expanded, max_kept_usd, w));
                    }
                    Focus::Vibe(vi) => {
                        lines.extend(self.render_vibe_row(vi, focused, expanded, w));
                    }
                }
                // Add breathing space between top-level actionable rows; keep already-good rows tidy.
                let is_sub = matches!(item, Focus::Member(..));
                let is_kept = matches!(item, Focus::Kept(_));
                let next_is_sub = matches!(visible.get(idx + 1), Some(Focus::Member(..)));
                let next_is_kept = matches!(visible.get(idx + 1), Some(Focus::Kept(_)));
                if !is_sub && !next_is_sub && idx + 1 < visible.len() && (!is_kept || !next_is_kept)
                {
                    lines.push(String::new());
                }
                body_blocks.push((focused, lines));
            }
            if self.tab == Tab::Measured && self.hero.kept_features > 0 {
                let ok = if self.unicode { "✓" } else { "+" };
                let n = self.hero.kept_features;
                let msg = format!(
                    "        {} {n} tool{} & skill{} within budget",
                    self.style(GREEN, ok),
                    if n == 1 { "" } else { "s" },
                    if n == 1 { "" } else { "s" },
                );
                body_blocks.push((false, vec![String::new(), self.style(DIM, &msg)]));
            }
        }

        // Window body lines so head + body + foot <= max_rows.
        let budget = max_rows.saturating_sub(head.len() + foot.len()).max(4);
        let body = window_blocks(&body_blocks, budget, w, self.color, self.unicode);

        let mut out = head;
        out.extend(body);
        out.extend(foot);
        out
    }

    fn render_group_row(
        &self,
        gi: usize,
        focused: bool,
        expanded: bool,
        max_usd: f64,
        w: usize,
    ) -> Vec<String> {
        let g = &self.groups[gi];
        let all_checked = g.members.iter().all(|m| m.checked);
        let any_checked = g.members.iter().any(|m| m.checked);
        let box_str = if all_checked {
            let chk = if self.unicode { "✓" } else { "x" };
            self.style(&format!("{BOLD}{GREEN}"), &format!("[{chk}]"))
        } else if any_checked {
            let half = if self.unicode { "◐" } else { "~" };
            self.style(&format!("{BOLD}{YELLOW}"), &format!("[{half}]"))
        } else {
            self.style(DIM, "[ ]")
        };

        let rail = if focused {
            self.style(BRAND, if self.unicode { "▎" } else { ">" })
        } else {
            " ".into()
        };

        let suggested = g.members.iter().all(|m| m.fix.suggested);
        let usd = group_usd(g, self.billing);
        let dot = if usd.is_none() {
            self.style(DIM, if self.unicode { "○" } else { "o" })
        } else if suggested {
            self.style(&format!("{BOLD}{YELLOW}"), "!")
        } else {
            self.style(
                &format!("{BOLD}{RED}"),
                if self.unicode { "✗" } else { "x" },
            )
        };

        let name = if g.members.len() == 1 {
            g.members[0].fix.name.clone()
        } else {
            g.members[0].fix.section.label.clone()
        };
        let host_tag = g
            .host_label
            .as_deref()
            .map(|h| format!("[{h}] "))
            .unwrap_or_default();
        // Never show settings file paths inside the value summary!
        let value = section_value(
            g.members
                .iter()
                .map(|m| (&m.fix.section, "", m.fix.value.as_str())),
            self.arrow(),
        );

        let usd_text = match usd {
            Some(u) if u <= 0.0 => "$0/mo".into(),
            Some(u) => {
                let prefix = if g.members.iter().all(|m| is_unset_main_ttl(&m.fix))
                    && self.billing == BillingMode::Auto
                {
                    if self.unicode { "≤" } else { "<=" }
                } else {
                    "+"
                };
                format!("{prefix}{}/mo", about(u))
            }
            None => "unmeasured".into(),
        };

        let bar_w = if w >= 72 { 10 } else { 0 };
        let bar_str = if bar_w > 0 {
            let ratio = match (usd, max_usd > 0.0) {
                (Some(u), true) if u > 0.0 => (u / max_usd).clamp(0.0, 1.0),
                _ => 0.0,
            };
            let filled = if ratio > 0.0 {
                ((ratio * bar_w as f64).round() as usize).clamp(1, bar_w)
            } else {
                0
            };
            let (fch, rch) = if self.unicode {
                ("█", "░")
            } else {
                ("#", ".")
            };
            let f_col = if any_checked { GREEN } else { DIM };
            format!(
                "{}{}  ",
                self.style(f_col, &fch.repeat(filled)),
                self.style(DIM, &rch.repeat(bar_w - filled))
            )
        } else {
            String::new()
        };

        let right_plain_len = if bar_w > 0 { bar_w + 2 } else { 0 } + cells(&usd_text).max(9);
        let left_budget = w.saturating_sub(11 + right_plain_len);
        let full_title = format!("{host_tag}{name}");
        let name_fit = self.fit_text(&full_title, left_budget);
        let val_budget = left_budget.saturating_sub(cells(&name_fit) + 2);
        let val_str = if val_budget >= 6 && !value.is_empty() {
            format!("  {}", self.style(DIM, &self.fit_text(&value, val_budget)))
        } else {
            String::new()
        };

        let name_styled = if focused || any_checked {
            self.style(BOLD, &name_fit)
        } else {
            name_fit.clone()
        };
        let usd_padded = format!("{usd_text:>9}");
        let usd_styled = if any_checked && usd.is_some_and(|u| u > 0.0) {
            self.style(&format!("{BOLD}{GREEN}"), &usd_padded)
        } else {
            self.style(DIM, &usd_padded)
        };

        let left_part = format!(" {rail} {box_str} {dot} {name_styled}{val_str}");
        let right_part = format!("{bar_str}{usd_styled}");
        let gap = w
            .saturating_sub(ansi_cells(&left_part) + ansi_cells(&right_part) + 2)
            .max(1);
        let mut rows = vec![trim_ansi(
            &format!("{left_part}{}{right_part}", " ".repeat(gap)),
            w,
        )];

        // Crisp 1-line subtitle on the focused row (with cost-per-use callout for tradeoffs!).
        if focused {
            let mut tags = vec![];
            if g.members.len() == 1
                && let Some(Proof::CostPerUse {
                    cost_per_use_usd,
                    uses,
                    ..
                }) = g.members[0].fix.proof
            {
                tags.push(format!(
                    "used {}x ({}/use)",
                    commas(uses),
                    about(cost_per_use_usd)
                ));
            } else if suggested {
                tags.push("tradeoff".to_string());
            } else if usd.is_some_and(|u| u > 0.0) {
                tags.push("safe win".to_string());
            }
            if g.members.len() > 1 {
                tags.push(format!(
                    "{} items{}[s] {}",
                    g.members.len(),
                    self.sep(),
                    if g.split { "collapse" } else { "split" }
                ));
            }
            if let Some(u) = usd {
                let total_avail = self.total_available_usd();
                if total_avail > 0.0 && u > 0.0 {
                    let share = (u / total_avail * 100.0).round() as usize;
                    if share >= 5 {
                        tags.push(format!("{share}% of savings"));
                    }
                }
            }
            tags.push(format!(
                "[{}] {}",
                self.arrow(),
                if expanded { "hide proof" } else { "proof" }
            ));
            rows.push(self.style(
                DIM,
                &self.fit_text(&format!("        {}", tags.join(self.sep())), w),
            ));
        }

        if expanded {
            rows.push(String::new());
            rows.extend(self.render_group_proof(g, w));
        }
        rows
    }

    fn render_member_row(
        &self,
        gi: usize,
        mi: usize,
        focused: bool,
        expanded: bool,
        max_usd: f64,
        w: usize,
    ) -> Vec<String> {
        let m = &self.groups[gi].members[mi];
        let rail = if focused {
            self.style(BRAND, if self.unicode { "▎" } else { ">" })
        } else {
            " ".into()
        };
        let box_str = if m.checked {
            let chk = if self.unicode { "✓" } else { "x" };
            self.style(GREEN, &format!("[{chk}]"))
        } else {
            self.style(DIM, "[ ]")
        };
        let usd = effective_saving(&m.fix, self.billing);
        let usd_text = usd.map_or_else(|| "unmeasured".into(), |u| format!("+{}/mo", about(u)));
        let usd_styled = if m.checked && usd.is_some_and(|u| u > 0.0) {
            self.style(GREEN, &format!("{usd_text:>9}"))
        } else {
            self.style(DIM, &format!("{usd_text:>9}"))
        };
        let left_budget = w.saturating_sub(24);
        let name_fit = self.fit_text(&m.fix.name, left_budget);
        let name_styled = if focused {
            self.style(BOLD, &name_fit)
        } else {
            name_fit
        };
        let left = format!(" {rail}    {box_str} {name_styled}");
        let gap = w
            .saturating_sub(ansi_cells(&left) + ansi_cells(&usd_styled) + 2)
            .max(1);
        let mut rows = vec![trim_ansi(
            &format!("{left}{}{usd_styled}", " ".repeat(gap)),
            w,
        )];
        if expanded {
            let _ = max_usd;
            rows.extend(self.render_fix_proof(&m.fix, w));
        }
        rows
    }

    fn render_cleanup_row(
        &self,
        ci: usize,
        focused: bool,
        expanded: bool,
        w: usize,
    ) -> Vec<String> {
        let c = &self.cleanups[ci];
        let rail = if focused {
            self.style(BRAND, if self.unicode { "▎" } else { ">" })
        } else {
            " ".into()
        };
        let box_str = if c.checked {
            let chk = if self.unicode { "✓" } else { "x" };
            self.style(GREEN, &format!("[{chk}]"))
        } else {
            self.style(DIM, "[ ]")
        };
        let host = c
            .host_label
            .as_deref()
            .map(|h| format!("[{h}] "))
            .unwrap_or_default();
        let label = self.fit_text(
            &format!("{host}Remove disabled MCP {}", c.server),
            w.saturating_sub(22),
        );
        let right = self.style(DIM, "cleanup");
        let left = format!(
            " {rail} {box_str} {} {}",
            self.style(DIM, if self.unicode { "○" } else { "o" }),
            if focused {
                self.style(BOLD, &label)
            } else {
                label
            }
        );
        let gap = w
            .saturating_sub(ansi_cells(&left) + ansi_cells(&right) + 2)
            .max(1);
        let mut rows = vec![trim_ansi(&format!("{left}{}{right}", " ".repeat(gap)), w)];
        if expanded {
            let pipe = self.style(BRAND, if self.unicode { "│" } else { "|" });
            rows.push(trim_ansi(
                &format!(
                    "      {pipe} {}",
                    self.style(
                        DIM,
                        "Already turned off — removes the leftover config entry."
                    )
                ),
                w,
            ));
        }
        rows
    }

    fn render_kept_row(
        &self,
        ki: usize,
        focused: bool,
        expanded: bool,
        max_kept_usd: f64,
        w: usize,
    ) -> Vec<String> {
        let k = &self.hero.kept[ki];
        let rail = if focused {
            self.style(BRAND, if self.unicode { "▎" } else { ">" })
        } else {
            " ".into()
        };
        let check = self.style(
            &format!("{BOLD}{GREEN}"),
            if self.unicode { "✓" } else { "+" },
        );
        let host_tag = k
            .host_label
            .as_deref()
            .map(|h| format!("[{h}] "))
            .unwrap_or_default();

        let usd_text = match k.saving_usd {
            Some(u) if u > 0.0 => format!("+{}/mo", about(u)),
            _ => "good".into(),
        };
        let bar_w = if w >= 72 && max_kept_usd > 0.0 { 10 } else { 0 };
        let bar_str = if bar_w > 0 {
            if let Some(u) = k.saving_usd.filter(|&u| u > 0.0) {
                let ratio = (u / max_kept_usd).clamp(0.0, 1.0);
                let filled = ((ratio * bar_w as f64).round() as usize).clamp(1, bar_w);
                let (fch, rch) = if self.unicode {
                    ("█", "░")
                } else {
                    ("#", ".")
                };
                format!(
                    "{}{}  ",
                    self.style(GREEN, &fch.repeat(filled)),
                    self.style(DIM, &rch.repeat(bar_w - filled))
                )
            } else {
                " ".repeat(bar_w + 2)
            }
        } else {
            String::new()
        };

        let right_plain_len = if bar_w > 0 { bar_w + 2 } else { 0 } + cells(&usd_text).max(9);
        let left_budget = w.saturating_sub(11 + right_plain_len);
        let full_title = format!("{host_tag}{}", k.name);
        let name_fit = self.fit_text(&full_title, left_budget);
        let val_budget = left_budget.saturating_sub(cells(&name_fit) + 2);
        let val_str = if val_budget >= 2 && !k.value.is_empty() {
            format!(
                "  {}",
                self.style(DIM, &self.fit_text(&k.value, val_budget))
            )
        } else {
            String::new()
        };

        let name_styled = if focused {
            self.style(BOLD, &name_fit)
        } else {
            name_fit
        };
        let usd_padded = format!("{usd_text:>9}");
        let usd_styled = if k.saving_usd.is_some_and(|u| u > 0.0) {
            self.style(GREEN, &usd_padded)
        } else {
            self.style(DIM, &usd_padded)
        };

        // Align green `✓` directly under the `✗`/`!` status mark column (` {rail}     {check} `).
        let left_part = format!(" {rail}     {check} {name_styled}{val_str}");
        let right_part = format!("{bar_str}{usd_styled}");
        let gap = w
            .saturating_sub(ansi_cells(&left_part) + ansi_cells(&right_part) + 2)
            .max(1);
        let mut rows = vec![trim_ansi(
            &format!("{left_part}{}{right_part}", " ".repeat(gap)),
            w,
        )];

        if focused {
            let mut tags = vec!["already good".to_string()];
            if let (Some(def), Some(u)) = (&k.default_value, k.saving_usd) {
                tags.push(format!("saving ~{}/mo vs {def} default", about(u)));
            }
            tags.push(format!(
                "[{}] {}",
                self.arrow(),
                if expanded { "hide" } else { "why" }
            ));
            rows.push(self.style(
                DIM,
                &self.fit_text(&format!("        {}", tags.join(self.sep())), w),
            ));
        }

        if expanded && !k.summary.is_empty() {
            let pipe = self.style(GREEN, if self.unicode { "│" } else { "|" });
            let wrap_w = w.saturating_sub(11).max(20);
            for line in wrap_text(
                &k.summary,
                WrapOptions {
                    width: Some(wrap_w),
                    ..Default::default()
                },
            )
            .lines()
            {
                rows.push(trim_ansi(&format!("      {pipe} {line}"), w));
            }
        }
        rows
    }

    fn render_vibe_row(&self, vi: usize, focused: bool, expanded: bool, w: usize) -> Vec<String> {
        let Some(vibes) = &self.vibes else {
            return vec![];
        };
        let v = &vibes[vi];
        let rail = if focused {
            self.style(BRAND, if self.unicode { "▎" } else { ">" })
        } else {
            " ".into()
        };
        let box_str = if v.offer_index.is_none() {
            self.style(DIM, if self.unicode { " · " } else { " - " })
        } else if v.checked {
            let chk = if self.unicode { "✓" } else { "x" };
            self.style(&format!("{BOLD}{GREEN}"), &format!("[{chk}]"))
        } else {
            self.style(DIM, "[ ]")
        };
        let host = v
            .host_label
            .as_deref()
            .map(|h| format!("[{h}] "))
            .unwrap_or_default();
        let val = if v.item.value.is_empty() {
            String::new()
        } else {
            format!("  {}", self.style(DIM, &v.item.value))
        };
        let source = concise_vibe_source(&v.item.sources);
        let tag = self.style(DIM, source);
        let name = self.fit_text(&format!("{host}{}", v.item.name), w.saturating_sub(24));
        let name_styled = if focused {
            self.style(BOLD, &name)
        } else {
            name
        };
        let left = format!(
            " {rail} {box_str} {} {name_styled}{val}",
            self.style(BRAND, if self.unicode { "○" } else { "o" })
        );
        let gap = w
            .saturating_sub(ansi_cells(&left) + ansi_cells(&tag) + 2)
            .max(1);
        let mut rows = vec![trim_ansi(&format!("{left}{}{tag}", " ".repeat(gap)), w)];

        if expanded || focused {
            let pipe = self.style(BRAND, if self.unicode { "│" } else { "|" });
            let wrap_w = w.saturating_sub(11).max(20);
            if expanded {
                rows.push(String::new());
                let simple = concise_vibe_copy(&v.item);
                for line in wrap_text(
                    &simple,
                    WrapOptions {
                        width: Some(wrap_w),
                        ..Default::default()
                    },
                )
                .lines()
                {
                    rows.push(trim_ansi(&format!("      {pipe} {line}"), w));
                }
            } else {
                rows.push(self.style(
                    DIM,
                    &self.fit_text(
                        &format!("        Pick: {source}{}[{}] why", self.sep(), self.arrow()),
                        w,
                    ),
                ));
            }
        }
        rows
    }

    /// Render the expandable inline proof drawer for a group (lists every member cleanly with bars, no cutoffs, no file paths).
    fn render_group_proof(&self, g: &Group, w: usize) -> Vec<String> {
        if g.members.len() == 1 {
            return self.render_fix_proof(&g.members[0].fix, w);
        }
        let pipe = self.style(BRAND, if self.unicode { "│" } else { "|" });
        let wrap_w = w.saturating_sub(11).max(20);
        let mut out = vec![];

        let summary = concise_fix_copy(&g.members[0].fix, true);
        if !summary.is_empty() {
            for line in wrap_text(
                &summary,
                WrapOptions {
                    width: Some(wrap_w),
                    ..Default::default()
                },
            )
            .lines()
            {
                out.push(trim_ansi(&format!("      {pipe} {line}"), w));
            }
        }

        // If members are CacheTtl with Proof::CacheTtl, show aggregated real 5m vs 1h cost chart!
        let mut five = 0.0;
        let mut one = 0.0;
        let mut reqs = 0u64;
        let mut ttl_count = 0usize;
        for m in &g.members {
            if let Some(Proof::CacheTtl {
                five_m_usd,
                one_h_usd,
                requests,
            }) = &m.fix.proof
            {
                five += five_m_usd;
                one += one_h_usd;
                reqs += requests;
                ttl_count += 1;
            }
        }
        if ttl_count > 0 && (five > 0.0 || one > 0.0) {
            out.push(trim_ansi(&format!("      {pipe}"), w));
            out.extend(self.render_ttl_chart(five, one, reqs, wrap_w, &pipe, w));
        } else {
            // Render EVERY member on its own clean row with a proportional bar + exact saving — never cut off names!
            out.push(trim_ansi(&format!("      {pipe}"), w));
            let top_member_usd = g
                .members
                .iter()
                .filter_map(|m| effective_saving(&m.fix, self.billing))
                .fold(0.0_f64, f64::max);
            let max_name = g
                .members
                .iter()
                .map(|m| cells(&m.fix.section.member))
                .max()
                .unwrap_or(10)
                .min(wrap_w.saturating_sub(24).max(12));
            let bar_w = if wrap_w >= max_name + 22 { 10 } else { 0 };
            let (fch, rch) = if self.unicode {
                ("█", "░")
            } else {
                ("#", ".")
            };

            for m in &g.members {
                let usd = effective_saving(&m.fix, self.billing);
                let name = &m.fix.section.member;
                let pad = " ".repeat(max_name.saturating_sub(cells(name)));
                let bar = if bar_w > 0 && top_member_usd > 0.0 {
                    let ratio = usd.unwrap_or(0.0) / top_member_usd;
                    let filled = if ratio > 0.0 {
                        ((ratio * bar_w as f64).round() as usize).clamp(1, bar_w)
                    } else {
                        0
                    };
                    format!(
                        "  {}{}",
                        self.style(GREEN, &fch.repeat(filled)),
                        self.style(DIM, &rch.repeat(bar_w - filled))
                    )
                } else {
                    String::new()
                };
                let amt = usd.map_or_else(|| "unmeasured".into(), |u| format!("+{}/mo", about(u)));
                let amt_styled = self.style(GREEN, &format!("{amt:>9}"));
                out.push(trim_ansi(
                    &format!("      {pipe} {name}{pad}{bar}  {amt_styled}"),
                    w,
                ));
            }
        }
        out
    }

    /// Render the expandable inline proof drawer for a single `Fix` (no file paths, short human prose).
    fn render_fix_proof(&self, fix: &Fix, w: usize) -> Vec<String> {
        let pipe = self.style(BRAND, if self.unicode { "│" } else { "|" });
        let wrap_w = w.saturating_sub(11).max(20);
        let mut out = vec![];

        let summary = concise_fix_copy(fix, false);
        if !summary.is_empty() {
            for line in wrap_text(
                &summary,
                WrapOptions {
                    width: Some(wrap_w),
                    ..Default::default()
                },
            )
            .lines()
            {
                out.push(trim_ansi(&format!("      {pipe} {line}"), w));
            }
        }

        if let Some(proof) = &fix.proof {
            if !out.is_empty() {
                out.push(trim_ansi(&format!("      {pipe}"), w));
            }
            match *proof {
                Proof::CacheTtl {
                    five_m_usd,
                    one_h_usd,
                    requests,
                } => {
                    out.extend(
                        self.render_ttl_chart(five_m_usd, one_h_usd, requests, wrap_w, &pipe, w),
                    );
                }
                Proof::CostPerUse {
                    cost_per_use_usd,
                    threshold_usd,
                    uses,
                    requests,
                } => {
                    out.extend(self.render_cpu_chart(
                        cost_per_use_usd,
                        threshold_usd,
                        uses,
                        requests,
                        wrap_w,
                        &pipe,
                        w,
                    ));
                }
                Proof::Unused { requests, tokens } => {
                    let tok = tokens
                        .map(|t| format!(" (~{} tok/req)", commas(t.max(0) as u64)))
                        .unwrap_or_default();
                    let line = format!("0 uses across {} requests{tok}", commas(requests));
                    out.push(trim_ansi(
                        &format!("      {pipe} {}", self.style(DIM, &line)),
                        w,
                    ));
                }
            }
        }
        out
    }

    fn render_ttl_chart(
        &self,
        five_m_usd: f64,
        one_h_usd: f64,
        requests: u64,
        wrap_w: usize,
        pipe: &str,
        w: usize,
    ) -> Vec<String> {
        let max_v = five_m_usd.max(one_h_usd).max(0.01);
        let bar_w = wrap_w.saturating_sub(28).clamp(8, 22);
        let (fch, rch) = if self.unicode {
            ("█", "░")
        } else {
            ("#", ".")
        };
        let make_bar = |v: f64, col: &str| {
            let n = ((v / max_v) * bar_w as f64).round() as usize;
            let n = n.clamp(usize::from(v > 0.0), bar_w);
            format!(
                "{}{}",
                self.style(col, &fch.repeat(n)),
                self.style(DIM, &rch.repeat(bar_w - n))
            )
        };
        let one_wins = one_h_usd <= five_m_usd;
        let col5 = if one_wins { RED } else { GREEN };
        let col1 = if one_wins { GREEN } else { RED };
        let diff = (five_m_usd - one_h_usd).abs();
        let hdr = format!(
            "Replayed cache cost on {} reqs (per 30d):",
            commas(requests)
        );
        let r5 = format!(
            "5m TTL  {}  {}/mo{}",
            make_bar(five_m_usd, col5),
            about(five_m_usd),
            if !one_wins {
                format!("  (-{}/mo)", about(diff))
            } else {
                String::new()
            }
        );
        let r1 = format!(
            "1h TTL  {}  {}/mo{}",
            make_bar(one_h_usd, col1),
            about(one_h_usd),
            if one_wins {
                format!("  (-{}/mo)", about(diff))
            } else {
                String::new()
            }
        );
        vec![
            trim_ansi(&format!("      {pipe} {}", self.style(DIM, &hdr)), w),
            trim_ansi(&format!("      {pipe}   {r5}"), w),
            trim_ansi(&format!("      {pipe}   {r1}"), w),
        ]
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "pure formatting helper for cost-per-use chart"
    )]
    fn render_cpu_chart(
        &self,
        cost_per_use: f64,
        threshold: f64,
        uses: u64,
        requests: u64,
        wrap_w: usize,
        pipe: &str,
        w: usize,
    ) -> Vec<String> {
        let max_v = cost_per_use.max(threshold).max(0.01);
        let bar_w = wrap_w.saturating_sub(30).clamp(8, 20);
        let (fch, rch) = if self.unicode {
            ("█", "░")
        } else {
            ("#", ".")
        };
        let make_bar = |v: f64, col: &str| {
            let n = ((v / max_v) * bar_w as f64).round() as usize;
            let n = n.clamp(usize::from(v > 0.0), bar_w);
            format!(
                "{}{}",
                self.style(col, &fch.repeat(n)),
                self.style(DIM, &rch.repeat(bar_w - n))
            )
        };
        let ratio = if threshold > 0.0 {
            format!("  ({:.0}x budget)", cost_per_use / threshold)
        } else {
            String::new()
        };
        let hdr = format!(
            "Cost vs usage ({} use{} across {} reqs):",
            commas(uses),
            if uses == 1 { "" } else { "s" },
            commas(requests)
        );
        let rb = format!(
            "Budget  {}  {}/use",
            make_bar(threshold, GREEN),
            about(threshold)
        );
        let ra = format!(
            "Actual  {}  {}/use{ratio}",
            make_bar(cost_per_use, YELLOW),
            about(cost_per_use)
        );
        vec![
            trim_ansi(&format!("      {pipe} {}", self.style(DIM, &hdr)), w),
            trim_ansi(&format!("      {pipe}   {rb}"), w),
            trim_ansi(&format!("      {pipe}   {ra}"), w),
        ]
    }

    fn render_diff(&self, w: usize, max_rows: usize) -> Vec<String> {
        let Some(diff) = &self.diff else {
            return vec![];
        };
        let rule = if self.unicode { "─" } else { "-" };
        let mut head = vec![
            String::new(),
            trim_ansi(
                &format!(
                    "  {} {}",
                    self.style(&format!("{BOLD}{BRAND}"), "Review exact edits"),
                    self.style(
                        DIM,
                        &format!(
                            "{}{} change{} across {} file{}",
                            self.sep(),
                            diff.change_count,
                            if diff.change_count == 1 { "" } else { "s" },
                            diff.file_count,
                            if diff.file_count == 1 { "" } else { "s" }
                        )
                    )
                ),
                w,
            ),
            self.style(DIM, &format!(" {}", rule.repeat(w.saturating_sub(2)))),
            String::new(),
        ];

        let mut foot = vec![];
        for b in &diff.backups {
            foot.push(self.style(DIM, &self.fit_text(&format!("  {b}"), w)));
        }
        if let Some(err) = &self.status_error {
            foot.push(self.style(RED, &self.fit_text(&format!("  ! {err}"), w)));
        }
        foot.push(String::new());
        foot.push(self.style(DIM, &format!(" {}", rule.repeat(w.saturating_sub(2)))));
        foot.push(trim_ansi(
            &format!(
                "  {}  {}  {}",
                self.style(&format!("{BOLD}{GREEN}"), "[Enter] Write settings"),
                self.style(DIM, "[Esc] Back"),
                self.style(DIM, "[q] Cancel"),
            ),
            w,
        ));

        let raw_lines: Vec<String> = diff.text.lines().map(|l| trim_ansi(l, w)).collect();
        let budget = max_rows.saturating_sub(head.len() + foot.len()).max(4);
        let max_scroll = raw_lines.len().saturating_sub(budget);
        let scroll = self.diff_scroll.min(max_scroll);
        let slice = &raw_lines[scroll..(scroll + budget).min(raw_lines.len())];
        head.extend(slice.iter().cloned());
        if max_scroll > 0 {
            let arrows = if self.unicode { "↑/↓" } else { "j/k" };
            head.push(self.style(
                DIM,
                &format!(
                    "  ({arrows} scroll{}lines {}-{} of {})",
                    self.sep(),
                    scroll + 1,
                    scroll + slice.len(),
                    raw_lines.len()
                ),
            ));
        }
        head.extend(foot);
        head
    }
}

/// Short, plain-English 1-sentence explanation for a measured fix in the interactive TUI.
fn concise_fix_copy(fix: &Fix, grouped: bool) -> String {
    let name_lc = fix.name.to_ascii_lowercase();
    match fix.section.kind {
        Kind::CacheTtl => {
            if target(&fix.value) == "1h" {
                "Your pauses between turns often pass 5m, forcing full cache rewrites. 1h costs less overall.".into()
            } else {
                "Your turns are fast enough that 1h's write fee costs more than 5m.".into()
            }
        }
        Kind::Feature(ContextKind::Skill, Reason::Unused | Reason::Unrecorded) => {
            "Listed in every prompt, never used once. Still works whenever you type /name.".into()
        }
        Kind::Feature(ContextKind::Skill, Reason::Costly) => {
            "Always-on in every prompt, rarely triggered by the model. Switching to typed-only keeps /name working.".into()
        }
        Kind::Feature(ContextKind::Skill, Reason::Duplicate) => {
            "Duplicate skill copy riding along in every prompt. Keeping only the used copy saves tokens.".into()
        }
        Kind::Feature(ContextKind::Tool, Reason::Unused | Reason::Unrecorded) => {
            if grouped {
                "Built-in tool schemas sent on every request, never called once in this window."
                    .into()
            } else {
                "Sent on every request, never called once in this window.".into()
            }
        }
        Kind::Feature(ContextKind::Tool, Reason::Costly) => {
            "Tool schema rides along on every request and costs more per call than your budget."
                .into()
        }
        Kind::Feature(ContextKind::McpServer | ContextKind::McpTool | ContextKind::McpDiscovery, Reason::Unused | Reason::Unrecorded) => {
            "MCP server tools sent on every request, never called once in this window.".into()
        }
        Kind::Feature(ContextKind::McpServer | ContextKind::McpTool | ContextKind::McpDiscovery, Reason::Costly) => {
            "MCP tools cost more per call in prompt overhead than your budget.".into()
        }
        _ if name_lc.contains("fast mode") => {
            "Same model at a higher per-token price. Turn off unless faster replies are worth the premium.".into()
        }
        _ if name_lc.contains("auto memory") || name_lc.contains("memories") => {
            "Loads saved notes into every session, but was rarely read compared to what it costs."
                .into()
        }
        _ if name_lc.contains("prompt suggestion") => {
            "Sends an extra background request after every reply just to suggest your next prompt."
                .into()
        }
        _ if name_lc.contains("recap") => {
            "Sends an extra background request to summarize the session when you step away.".into()
        }
        _ if name_lc.contains("check-in") => {
            "Starts background turns to check on idle tasks even while you're away.".into()
        }
        _ if name_lc.contains("compact") => {
            "Compacting at this window saves more in later cache reads than the extra summaries cost."
                .into()
        }
        _ => {
            // Keep fallback short: first sentence of `what` or `why`, with "the maintainer" -> "Kixel".
            let raw = if !fix.what.is_empty() {
                fix.what.as_str()
            } else {
                why(&fix.evidence)
            };
            first_sentence(&replace_maintainer(raw))
        }
    }
}

/// Short, plain-English tip copy for the Vibes tab (human attention span, zero legal disclaimers).
fn concise_vibe_copy(item: &VibeItem) -> String {
    let name_lc = item.name.to_ascii_lowercase();
    let text_lc = item.text.to_ascii_lowercase();
    if name_lc.contains("fable") || text_lc.contains("fable") {
        if name_lc.contains("advisor") {
            return "Uses a stronger advisor only on hard steps instead of paying for it on every turn."
                .into();
        }
        return "Opus 5.5 xhigh is usually better and cheaper than Fable 5.2.".into();
    }
    if name_lc.contains("bash timeout") {
        return "A command hanging past your cache TTL lets the prompt cache expire. Lowering moves slow commands to the background sooner."
            .into();
    }
    if name_lc.contains("auto memory") {
        if item.how.is_some() {
            return "Your repos already have CLAUDE.md / AGENTS.md — keep lasting project facts there instead."
                .into();
        }
        return "Outside a repo, auto memory may be the only place facts persist — fine to keep on."
            .into();
    }
    if name_lc.contains("auto-compact") || name_lc.contains("compact window") {
        return "Compacting earlier keeps context sharp and avoids context rot, even when cost is similar."
            .into();
    }
    if name_lc.contains("main-session default") {
        return "Start sessions on Opus 5.5 at medium/high — cheaper than max with similar quality."
            .into();
    }
    if name_lc.contains("subagent") && name_lc.contains("effort") {
        return "Run subagents at high effort instead of max for a better cost/quality balance."
            .into();
    }
    if name_lc.contains("advisor") {
        return "Pairs your main model with a stronger advisor only on hard steps instead of every turn."
            .into();
    }
    if name_lc.contains("retention") || name_lc.contains("transcript") {
        return "Keep 365 days of transcripts so token-max can measure long-term habits.".into();
    }
    if name_lc.contains("claude.md")
        || name_lc.contains("agents.md")
        || name_lc.contains("instruction")
    {
        return "Keep root instructions under ~200 lines so adherence stays high and prefix cost stays lean."
            .into();
    }
    if name_lc.contains("lsp") {
        return "Gives the model real jump-to-definition and type errors instead of grepping files."
            .into();
    }
    first_sentence(&replace_maintainer(&item.text))
}

/// Short source attribution label for a vibes row (`Kixel`, `Anthropic`, `OpenAI`, etc.).
fn concise_vibe_source(sources: &[String]) -> &'static str {
    let joined = sources.join(" ").to_ascii_lowercase();
    if joined.contains("kixel") || joined.contains("maintainer") {
        "Kixel"
    } else if joined.contains("anthropic") || joined.contains("claude") {
        "Anthropic"
    } else if joined.contains("openai") || joined.contains("codex") {
        "OpenAI"
    } else {
        "tip"
    }
}

fn replace_maintainer(text: &str) -> String {
    text.replace("The maintainer", "Kixel")
        .replace("the maintainer", "Kixel")
}

fn first_sentence(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(idx) = trimmed.find(". ") {
        format!("{}.", &trimmed[..idx])
    } else {
        trimmed.to_string()
    }
}

/// Run the interactive inline TUI until the user writes changes, quits, or cancels.
pub(crate) fn run(
    mut app: App,
    style: &Style,
    bridge: &mut impl Bridge,
) -> Result<ExitKind, ScanError> {
    if app.tab == Tab::Vibes {
        app.vibes = Some(bridge.load_vibes()?);
    }

    // All-clear fast path: print crisp Hero Card + already-good green checks + manual steps.
    if app.is_all_clear() && !app.hero.read_only {
        let width = style.width.unwrap_or(80).clamp(48, 104);
        let mut stdout = io::stdout().lock();
        let failed = |_| ScanError::new("cannot use the terminal; no settings were changed");
        for line in app.render_hero(width) {
            writeln!(stdout, "{line}").map_err(failed)?;
        }
        if !app.hero.kept.is_empty() {
            let max_kept_usd = app
                .hero
                .kept
                .iter()
                .filter_map(|k| k.saving_usd)
                .fold(0.0_f64, f64::max);
            for ki in 0..app.hero.kept.len() {
                for line in app.render_kept_row(ki, false, false, max_kept_usd, width) {
                    writeln!(stdout, "{line}").map_err(failed)?;
                }
            }
            writeln!(stdout).map_err(failed)?;
        }
        write_by_hand(&mut stdout, &app.by_hand, style.color).map_err(failed)?;
        stdout.flush().map_err(failed)?;
        return Ok(ExitKind::Done);
    }

    let _raw = RawGuard::enter().map_err(|_| {
        ScanError::new("cannot enable interactive terminal mode; rerun with --plain")
    })?;
    let mut stdout = io::stdout().lock();
    let mut viewport = InlineViewport::default();

    loop {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 28));
        let frame = app.render(cols as usize, (rows as usize).saturating_sub(1));
        viewport
            .draw(&mut stdout, &frame)
            .map_err(|_| ScanError::new("cannot draw to terminal; no settings were changed"))?;

        let Ok(ev) = event::read() else {
            viewport.clear(&mut stdout).ok();
            return Ok(ExitKind::Done);
        };
        let TermEvent::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press | KeyEventKind::Repeat,
            ..
        }) = ev
        else {
            continue;
        };

        // Ctrl-C always cancels cleanly without touching files.
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c' | 'C')) {
            viewport.clear(&mut stdout).ok();
            return Ok(ExitKind::Cancelled);
        }

        app.status_error = None;

        match app.screen {
            Screen::List => match code {
                KeyCode::Char('q') => {
                    viewport.clear(&mut stdout).ok();
                    drop(_raw);
                    let _ = writeln!(
                        stdout,
                        "{}",
                        app.style(DIM, "  Nothing written; no settings were changed.")
                    );
                    let _ = stdout.flush();
                    return Ok(ExitKind::Done);
                }
                KeyCode::Esc => {
                    if app.expanded.is_some() {
                        app.expanded = None;
                    } else if app.show_caveats {
                        app.show_caveats = false;
                    } else {
                        viewport.clear(&mut stdout).ok();
                        drop(_raw);
                        let _ = writeln!(
                            stdout,
                            "{}",
                            app.style(DIM, "  Nothing written; no settings were changed.")
                        );
                        let _ = stdout.flush();
                        return Ok(ExitKind::Done);
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let len = app.visible_rows().len();
                    if len > 0 {
                        app.focus = (app.focus + len - 1) % len;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let len = app.visible_rows().len();
                    if len > 0 {
                        app.focus = (app.focus + 1) % len;
                    }
                }
                KeyCode::Char(' ') => {
                    app.toggle_focused();
                }
                KeyCode::Right | KeyCode::Char('l' | 'i') => {
                    if let Some(cur) = app.current_focus() {
                        app.expanded = if app.expanded == Some(cur) {
                            None
                        } else {
                            Some(cur)
                        };
                    }
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    app.expanded = None;
                }
                KeyCode::Char('s') => {
                    if let Some(Focus::Group(gi)) = app.current_focus()
                        && app.groups[gi].members.len() > 1
                    {
                        app.groups[gi].split = !app.groups[gi].split;
                        app.clamp_focus();
                    }
                }
                KeyCode::Char('a') => {
                    app.toggle_all_safe();
                }
                KeyCode::Char('b') => {
                    app.cycle_billing();
                }
                KeyCode::Char('m') => {
                    app.show_minor = !app.show_minor;
                    app.clamp_focus();
                }
                KeyCode::Char('?') => {
                    app.show_caveats = !app.show_caveats;
                }
                KeyCode::Char('v') | KeyCode::Tab => {
                    if !app.start_vibes_only && (app.hero.vibes_pending > 0 || app.vibes.is_some())
                    {
                        if app.tab == Tab::Measured {
                            if app.vibes.is_none() {
                                match bridge.load_vibes() {
                                    Ok(rows) => app.vibes = Some(rows),
                                    Err(e) => {
                                        app.status_error = Some(e.message);
                                        continue;
                                    }
                                }
                            }
                            app.tab = Tab::Vibes;
                        } else {
                            app.tab = Tab::Measured;
                        }
                        app.focus = 0;
                        app.expanded = None;
                    }
                }
                KeyCode::Enter => {
                    if app.hero.read_only {
                        app.status_error = Some(
                            "Preview only (--defaults) — run without --defaults to apply.".into(),
                        );
                        continue;
                    }
                    if app.checked_count() == 0 {
                        viewport.clear(&mut stdout).ok();
                        drop(_raw);
                        let _ = writeln!(
                            stdout,
                            "{}",
                            app.style(DIM, "  Nothing selected; no settings were changed.")
                        );
                        let _ = write_by_hand(&mut stdout, &app.by_hand, app.color);
                        let _ = stdout.flush();
                        return Ok(ExitKind::Done);
                    }
                    let result = match app.tab {
                        Tab::Measured => {
                            let events = app.measured_events();
                            bridge.preview_measured(&events)
                        }
                        Tab::Vibes => bridge.preview_vibes(app.vibes.as_deref().unwrap_or(&[])),
                    };
                    match result {
                        Ok(diff) => {
                            app.diff = Some(diff);
                            app.diff_scroll = 0;
                            app.diff_tab = app.tab;
                            app.screen = Screen::Diff;
                        }
                        Err(e) => app.status_error = Some(e.message),
                    }
                }
                _ => {}
            },
            Screen::Diff => match code {
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('n') => {
                    app.screen = Screen::List;
                    app.diff = None;
                }
                KeyCode::Char('q') => {
                    viewport.clear(&mut stdout).ok();
                    drop(_raw);
                    let _ = writeln!(
                        stdout,
                        "{}",
                        app.style(DIM, "  Cancelled; no settings were changed.")
                    );
                    let _ = stdout.flush();
                    return Ok(ExitKind::Done);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.diff_scroll = app.diff_scroll.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.diff_scroll = app.diff_scroll.saturating_add(1);
                }
                KeyCode::Enter | KeyCode::Char('y') => {
                    let saved_usd = if app.diff_tab == Tab::Measured {
                        app.selected_usd()
                    } else {
                        0.0
                    };
                    let written = match app.diff_tab {
                        Tab::Measured => bridge.write_measured()?,
                        Tab::Vibes => bridge.write_vibes()?,
                    };
                    // Snappy coin-drop celebration before printing the permanent receipt!
                    play_coin_drop(&mut stdout, &mut viewport, app.color, app.unicode);
                    viewport.clear(&mut stdout).ok();
                    drop(_raw);

                    let ok = if app.unicode { "✓" } else { "OK" };
                    let count = written.files_written;
                    let headline = if app.diff_tab == Tab::Measured && saved_usd > 0.0 {
                        format!(
                            "  {} — updated {} file{}",
                            app.style(
                                &format!("{BOLD}{GREEN}"),
                                &format!("{ok} Saved ~{}/mo", about(saved_usd))
                            ),
                            count,
                            if count == 1 { "" } else { "s" }
                        )
                    } else {
                        format!(
                            "  {}",
                            app.style(
                                &format!("{BOLD}{GREEN}"),
                                &format!(
                                    "{ok} Updated {} file{}",
                                    count,
                                    if count == 1 { "" } else { "s" }
                                )
                            )
                        )
                    };
                    let _ = writeln!(stdout, "{headline}");
                    for line in &written.lines {
                        let _ = writeln!(stdout, "  {}", app.style(DIM, line));
                    }
                    let _ = write_by_hand(&mut stdout, &app.by_hand, app.color);
                    if app.diff_tab == Tab::Measured && app.hero.vibes_pending > 0 {
                        let n = app.hero.vibes_pending;
                        let _ = writeln!(
                            stdout,
                            "  {}",
                            app.style(
                                DIM,
                                &format!(
                                    "Optional: run `token-max vibes` for {n} unmeasured tip{}.",
                                    if n == 1 { "" } else { "s" }
                                )
                            )
                        );
                    }
                    let _ = stdout.flush();
                    return Ok(ExitKind::Done);
                }
                _ => {}
            },
        }
    }
}

/// Helper used by `cli.rs` to construct vibes events from `VibeRow`s.
pub(crate) fn vibes_events_from(rows: &[VibeRow], host_count: usize) -> Vec<Vec<Event>> {
    let mut events: Vec<Vec<Event>> = (0..host_count).map(|_| vec![]).collect();
    for row in rows {
        if let Some(offer_index) = row.offer_index {
            events[row.host].push(Event::Choose {
                offer: offer_index,
                yes: row.checked,
            });
        }
    }
    events
}

/// Print compact manual steps (if any) after the TUI closes.
fn write_by_hand(
    out: &mut impl Write,
    steps: &[(Option<String>, String, String, String)],
    color: bool,
) -> io::Result<()> {
    if steps.is_empty() {
        return Ok(());
    }
    writeln!(out)?;
    if color {
        writeln!(out, "  {BOLD}STEPS BY HAND{RESET}")?;
    } else {
        writeln!(out, "  STEPS BY HAND")?;
    }
    for (host, name, step, reason) in steps {
        let prefix = host
            .as_deref()
            .map(|h| format!("[{h}] "))
            .unwrap_or_default();
        if color {
            writeln!(out, "  {DIM}·{RESET} {BOLD}{prefix}{name}{RESET}: {step}")?;
            writeln!(out, "    {DIM}({reason}){RESET}")?;
        } else {
            writeln!(out, "  - {prefix}{name}: {step}")?;
            writeln!(out, "    ({reason})")?;
        }
    }
    Ok(())
}

/// Roomy 32x10 half-block piggy-bank sprite faithfully matching `assets/logo/logo-on-dark.svg`,
/// with a round green coin that drops smoothly behind the violet back rim into the slot.
pub(crate) fn piggy_lines(color: bool, unicode: bool, frame: usize) -> Vec<String> {
    if !unicode {
        return vec![
            "         ___        ".into(),
            "     ___/   \\___    ".into(),
            "  ~ /   =====   \\   ".into(),
            "   (          o  )o ".into(),
            "    \\___________/   ".into(),
            "      ||     ||     ".into(),
        ];
    }

    // 32x20 pixel bitmap: '.' empty, 'P' violet piggy, 'G' green coin.
    // Rendered 2 vertical pixels per terminal row using Unicode half-blocks (10 rows total).
    // Proportions are calibrated for ~1:2 terminal cells so the coin, eye, and nostril stay round.
    const BODY: [&[u8; 32]; 20] = [
        b"................................", // 0  coin air
        b"................................", // 1  coin air
        b"................................", // 2  coin air
        b"......................PP........", // 3  ear tip
        b".....................PPPP.......", // 4  ear mid
        b"....................PPPPPP......", // 5  ear base
        b"........PPPPPPPPPPPPPPPPPP......", // 6  solid back rim (coin slips behind!)
        b"......PPPPP.......PPPPPPPPP.....", // 7  coin slot cutout (x=11..17)
        b".....PPPPPPPPPPPPPPPP.PPPPPP....", // 8  upper body + round eye top
        b"..P..PPPPPPPPPPPPPPPP.PPPPPP....", // 9  curly tail tip + round eye bot + forehead notch
        b".P...PPPPPPPPPPPPPPPPPPPPPPPPP..", // 10 curly tail curve + blunt snout top
        b"..PPPPPPPPPPPPPPPPPPPPPPPPPP.P..", // 11 tail join + snout upper + nostril top
        b"....PPPPPPPPPPPPPPPPPPPPPPPP.P..", // 12 lower body + snout lower + nostril bot
        b".....PPPPPPPPPPPPPPPPPPPPPPPPP..", // 13 lower body + blunt snout bottom
        b"......PPPPPPPPPPPPPPPPPPPPPP....", // 14 chin notch + belly curve
        b".......PPPPPPPPPPPPPPPPPPPP.....", // 15 belly bottom
        b"........PPPP........PPPP........", // 16 legs top
        b"........PPPP........PPPP........", // 17 legs mid
        b".........PP..........PP.........", // 18 rounded feet
        b"................................", // 19 bottom breathing room
    ];

    // Round coin centered over the slot (x=11..17, center=14): only fills '.' pixels so it
    // slides cleanly behind the solid violet rim at y=6 and shows inside the slot at y=7.
    const COINS: [&[(usize, usize, usize)]; 6] = [
        &[
            (0, 13, 15),
            (1, 12, 16),
            (2, 12, 16),
            (3, 12, 16),
            (4, 13, 15),
        ],
        &[
            (1, 13, 15),
            (2, 12, 16),
            (3, 12, 16),
            (4, 12, 16),
            (5, 13, 15),
        ],
        &[
            (3, 13, 15),
            (4, 12, 16),
            (5, 12, 16),
            (6, 12, 16),
            (7, 13, 15),
        ],
        &[(4, 13, 15), (5, 12, 16), (6, 12, 16), (7, 12, 16)],
        &[(5, 13, 15), (6, 13, 15), (7, 13, 15)],
        &[],
    ];

    let mut grid = [[b'.'; 32]; 20];
    for (r, row) in BODY.iter().enumerate() {
        grid[r] = **row;
    }
    for &(y, x0, x1) in COINS[frame % COINS.len()] {
        for cell in &mut grid[y][x0..=x1] {
            if *cell == b'.' {
                *cell = b'G';
            }
        }
    }

    let mut out = Vec::with_capacity(10);
    for [top, bot] in grid.as_chunks::<2>().0 {
        let mut line = String::with_capacity(128);
        for (&t, &b) in top.iter().zip(bot.iter()) {
            match (t, b) {
                (b'.', b'.') => line.push(' '),
                (a, c) if a == c => {
                    if color {
                        let fg = if a == b'P' { "35" } else { "32" };
                        line.push_str(&format!("\x1b[{fg}m█{RESET}"));
                    } else {
                        line.push('█');
                    }
                }
                (a, b'.') => {
                    if color {
                        let fg = if a == b'P' { "35" } else { "32" };
                        line.push_str(&format!("\x1b[{fg}m▀{RESET}"));
                    } else {
                        line.push('▀');
                    }
                }
                (b'.', c) => {
                    if color {
                        let fg = if c == b'P' { "35" } else { "32" };
                        line.push_str(&format!("\x1b[{fg}m▄{RESET}"));
                    } else {
                        line.push('▄');
                    }
                }
                (a, c) => {
                    if color {
                        let fg = if a == b'P' { "35" } else { "32" };
                        let bg = if c == b'P' { "45" } else { "42" };
                        line.push_str(&format!("\x1b[{fg};{bg}m▀{RESET}"));
                    } else {
                        line.push('▀');
                    }
                }
            }
        }
        out.push(line);
    }
    out
}

/// Play a quick coin-drop confirmation animation inside the inline viewport.
fn play_coin_drop(out: &mut impl Write, viewport: &mut InlineViewport, color: bool, unicode: bool) {
    let steps = [
        (0, "Writing settings..."),
        (2, "Saving into the piggy bank..."),
        (4, "Saving into the piggy bank..."),
        (5, "Done!"),
    ];
    for (frame, msg) in steps {
        let pig = piggy_lines(color, unicode, frame);
        let mid = pig.len() / 2;
        let lines: Vec<String> = pig
            .into_iter()
            .enumerate()
            .map(|(i, row)| {
                if i == mid {
                    let label = if color {
                        format!("{BOLD}{GREEN}{msg}{RESET}")
                    } else {
                        msg.to_string()
                    };
                    format!("  {row}   {label}")
                } else {
                    format!("  {row}")
                }
            })
            .collect();
        let _ = viewport.draw(out, &lines);
        thread::sleep(Duration::from_millis(75));
    }
}

/// Tracks whether the inline TUI currently owns raw mode and hid the cursor.
static RAW_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Restore terminal state if the inline TUI is currently active (safe from signal handlers).
pub(crate) fn restore_if_active() {
    if RAW_ACTIVE.swap(false, Ordering::SeqCst) {
        let mut out = io::stdout();
        let _ = execute!(out, Show);
        let _ = disable_raw_mode();
    }
}

/// RAII guard that enables raw mode and hides the cursor, always restoring on drop.
struct RawGuard;

impl RawGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        RAW_ACTIVE.store(true, Ordering::SeqCst);
        let mut out = io::stdout();
        execute!(out, Hide)?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        restore_if_active();
    }
}

/// Flicker-free inline viewport that rewrites only its own previously drawn lines.
#[derive(Default)]
struct InlineViewport {
    height: u16,
}

impl InlineViewport {
    fn draw(&mut self, out: &mut impl Write, lines: &[String]) -> io::Result<()> {
        execute!(out, BeginSynchronizedUpdate)?;
        if self.height > 0 {
            execute!(out, MoveUp(self.height), MoveToColumn(0))?;
        }
        execute!(out, Clear(ClearType::FromCursorDown))?;
        for line in lines {
            write!(out, "{line}\r\n")?;
        }
        execute!(out, EndSynchronizedUpdate)?;
        out.flush()?;
        self.height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        Ok(())
    }

    fn clear(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.height > 0 {
            execute!(
                out,
                BeginSynchronizedUpdate,
                MoveUp(self.height),
                MoveToColumn(0),
                Clear(ClearType::FromCursorDown),
                EndSynchronizedUpdate
            )?;
            out.flush()?;
            self.height = 0;
        }
        Ok(())
    }
}

fn is_unset_main_ttl(fix: &Fix) -> bool {
    fix.section.kind == Kind::CacheTtl && fix.section.member == "main" && fix.verb == UP_TO
}

/// Effective monthly saving of `fix` under the live `billing` toggle.
pub(crate) fn effective_saving(fix: &Fix, billing: BillingMode) -> Option<f64> {
    if is_unset_main_ttl(fix) {
        let to_1h = target(&fix.value) == "1h";
        return match billing {
            BillingMode::Auto => fix.saving,
            BillingMode::Api => Some(if to_1h { fix.saving? } else { 0.0 }),
            BillingMode::Plan => Some(if !to_1h { fix.saving? } else { 0.0 }),
        };
    }
    fix.saving
}

fn group_usd(g: &Group, billing: BillingMode) -> Option<f64> {
    let mut sum = 0.0;
    let mut any = false;
    for m in &g.members {
        if let Some(u) = effective_saving(&m.fix, billing) {
            sum += u;
            any = true;
        }
    }
    any.then_some(sum)
}

/// Window rendered item blocks so the focused block is always visible within `budget` rows.
fn window_blocks(
    blocks: &[(bool, Vec<String>)],
    budget: usize,
    width: usize,
    color: bool,
    unicode: bool,
) -> Vec<String> {
    let total_lines: usize = blocks.iter().map(|(_, l)| l.len()).sum();
    if total_lines <= budget {
        return blocks.iter().flat_map(|(_, l)| l.iter().cloned()).collect();
    }
    let focus_idx = blocks.iter().position(|(f, _)| *f).unwrap_or(0);
    let mut start = focus_idx;
    let mut end = focus_idx + 1;
    let mut used = blocks[focus_idx].1.len();
    let inner_budget = budget.saturating_sub(2).max(1);
    loop {
        let mut grew = false;
        if end < blocks.len() && used + blocks[end].1.len() <= inner_budget {
            used += blocks[end].1.len();
            end += 1;
            grew = true;
        }
        if start > 0 && used + blocks[start - 1].1.len() <= inner_budget {
            start -= 1;
            used += blocks[start].1.len();
            grew = true;
        }
        if !grew {
            break;
        }
    }
    let mut out = vec![];
    let (up, down) = if unicode { ("↑", "↓") } else { ("^", "v") };
    if start > 0 {
        let msg = format!("  {up} {start} more above");
        out.push(if color {
            format!("{DIM}{}{RESET}", fit_plain(&msg, width, unicode))
        } else {
            fit_plain(&msg, width, unicode)
        });
    }
    for (_, lines) in &blocks[start..end] {
        out.extend(lines.iter().cloned());
    }
    if end < blocks.len() {
        let more = blocks.len() - end;
        let msg = format!("  {down} {more} more below");
        out.push(if color {
            format!("{DIM}{}{RESET}", fit_plain(&msg, width, unicode))
        } else {
            fit_plain(&msg, width, unicode)
        });
    }
    out
}

/// Format an integer with comma thousands separators.
pub(crate) fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Display cell width of `s`, ignoring ANSI SGR escape sequences.
fn ansi_cells(s: &str) -> usize {
    let mut plain = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(c);
        }
    }
    cells(&plain)
}

/// Fit plain text to at most `max_cells` display cells, adding an ellipsis when cut.
fn fit_plain(text: &str, max_cells: usize, unicode: bool) -> String {
    let clean = terminal_text(text);
    if cells(&clean) <= max_cells {
        return clean;
    }
    if unicode {
        if max_cells <= 1 {
            return fit(&clean, max_cells).to_string();
        }
        format!("{}…", fit(&clean, max_cells - 1))
    } else {
        if max_cells <= 3 {
            return fit(&clean, max_cells).to_string();
        }
        format!("{}...", fit(&clean, max_cells - 3))
    }
}

/// Pad an ANSI-styled string on the right to exactly `target_cells` display cells, clipping if wider.
fn pad_ansi(s: &str, target_cells: usize) -> String {
    let trimmed = trim_ansi(s, target_cells);
    let used = ansi_cells(&trimmed);
    if used < target_cells {
        format!("{trimmed}{}", " ".repeat(target_cells - used))
    } else {
        trimmed
    }
}

/// Clip an ANSI-styled string to at most `max_cells` display cells, closing any open SGR sequence.
fn trim_ansi(s: &str, max_cells: usize) -> String {
    if ansi_cells(s) <= max_cells {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut used = 0usize;
    let mut in_style = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            out.push(c);
            out.push(chars.next().expect("peeked '['"));
            let mut code = String::new();
            for next in chars.by_ref() {
                out.push(next);
                if next.is_ascii_alphabetic() {
                    if next == 'm' {
                        in_style = code != "0" && !code.is_empty();
                    }
                    break;
                }
                code.push(next);
            }
            continue;
        }
        let mut buf = [0u8; 4];
        let w = cells(c.encode_utf8(&mut buf));
        if used + w > max_cells {
            break;
        }
        out.push(c);
        used += w;
    }
    if in_style {
        out.push_str(RESET);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{Edit, Op, Owner, Patch, Section};
    use crate::model::{ContextKind, Harness};
    use crate::report::Origin;

    fn sample_fix(
        name: &str,
        value: &str,
        usd: f64,
        suggested: bool,
        section: Section,
        file: &str,
    ) -> Fix {
        Fix {
            name: name.into(),
            value: value.into(),
            suggested,
            saving: Some(usd),
            margin: None,
            verb: "saves",
            share: Some(usd / 1000.0),
            what: format!("{name} description."),
            evidence: format!("Measured on 100 requests saving ${usd:.0}/mo."),
            how: How::Edit(Patch {
                file: file.into(),
                harness: Harness::Claude,
                edits: vec![Edit {
                    path: vec!["setting".into()],
                    value: serde_json::json!(true),
                    op: Op::Set,
                }],
            }),
            origins: vec![Origin::Check(0)],
            section,
            profile: String::new(),
            removal: vec![],
            proof: None,
        }
    }

    #[test]
    fn safe_wins_prechecked_costly_ungrouped_minor_folded_no_file_paths() {
        let dir = std::env::temp_dir().join(format!("token-max-tui-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let settings = dir.join("settings.json");
        std::fs::write(&settings, "{}\n").unwrap();
        let file = settings.to_string_lossy().into_owned();

        let fixes = vec![
            Fix {
                verb: UP_TO,
                proof: Some(Proof::CacheTtl {
                    five_m_usd: 691.0,
                    one_h_usd: 278.0,
                    requests: 10_000,
                }),
                ..sample_fix(
                    "Main-agent prompt cache TTL",
                    "unset -> 1h",
                    413.0,
                    false,
                    Section {
                        kind: Kind::CacheTtl,
                        label: "Prompt cache TTL".into(),
                        member: "main".into(),
                        location: None,
                    },
                    &file,
                )
            },
            Fix {
                proof: Some(Proof::CostPerUse {
                    cost_per_use_usd: 29.0,
                    threshold_usd: 1.0,
                    uses: 3,
                    requests: 10_000,
                }),
                ..sample_fix(
                    "Skill dataviz",
                    "on -> typed only",
                    87.0,
                    true,
                    Section {
                        kind: Kind::Feature(ContextKind::Skill, Reason::Costly),
                        label: "Skills over $1 per use".into(),
                        member: "dataviz".into(),
                        location: None,
                    },
                    &file,
                )
            },
            Fix {
                proof: Some(Proof::CostPerUse {
                    cost_per_use_usd: 8.0,
                    threshold_usd: 1.0,
                    uses: 2,
                    requests: 10_000,
                }),
                ..sample_fix(
                    "Skill clean",
                    "on -> typed only",
                    16.0,
                    true,
                    Section {
                        kind: Kind::Feature(ContextKind::Skill, Reason::Costly),
                        label: "Skills over $1 per use".into(),
                        member: "clean".into(),
                        location: None,
                    },
                    &file,
                )
            },
            sample_fix(
                "CronList tool",
                "on -> off",
                0.05,
                false,
                Section {
                    kind: Kind::Feature(ContextKind::Tool, Reason::Unused),
                    label: "Never-used tools".into(),
                    member: "CronList".into(),
                    location: None,
                },
                &file,
            ),
        ];

        let owner = Owner::new(fixes, vec![], None, 1000.0);
        let style = Style {
            width: Some(80),
            color: true,
            unicode: true,
            home: None,
            host: None,
        };
        let hosts = [Walking {
            label: None,
            offers: owner.offers(),
            style: &style,
        }];
        let hero = Hero {
            requests: 10_000,
            tokens: 500_000_000,
            sessions: 120,
            spend_text: "$1,000".into(),
            monthly_spend: 1000.0,
            credit: Some("Already saving ~$3,546/mo (9%) vs defaults".into()),
            caveats: vec!["Sample caveat".into()],
            host_count: 1,
            vibes_pending: 2,
            read_only: false,
            kept: vec![
                KeptItem {
                    host_label: None,
                    name: "Subagent prompt cache TTL".into(),
                    value: "1h".into(),
                    saving_usd: Some(3545.0),
                    default_value: Some("5m".into()),
                    summary: "Default is 5m — keeping 1h saves ~$3,545/mo.".into(),
                },
                KeptItem {
                    host_label: None,
                    name: "Fast mode".into(),
                    value: "off".into(),
                    saving_usd: None,
                    default_value: None,
                    summary: "Already at the recommended value (off).".into(),
                },
            ],
            kept_features: 4,
        };

        let mut app = App::new(&hosts, hero, Tab::Measured, true, true);
        // Safe win ($413) pre-checked; costly skills (`dataviz` $87 and `clean` $16) each get
        // their own top-level row (unchecked); minor ($0.05) folded; 2 kept rows below.
        assert_eq!(app.checked_count(), 1);
        assert!((app.selected_usd() - 413.0).abs() < 0.01);
        assert_eq!(app.visible_rows().len(), 5);

        // Live billing toggle: Auto -> API ($413) -> Plan ($0, auto-unchecks unset 1h!).
        app.cycle_billing();
        assert_eq!(app.billing, BillingMode::Api);
        assert!((app.selected_usd() - 413.0).abs() < 0.01);
        app.cycle_billing();
        assert_eq!(app.billing, BillingMode::Plan);
        assert!((app.selected_usd() - 0.0).abs() < 0.01);
        assert_eq!(app.checked_count(), 0);

        // Render width & height bounds hold strictly, show red ✗ / green ✓, and never show raw file paths.
        app.cycle_billing();
        app.expanded = None;
        let full_frame = app.render(90, 40).join("\n");
        assert!(
            full_frame.contains('✗'),
            "expected red X change mark: {full_frame}"
        );
        assert!(
            full_frame.contains("ALREADY GOOD") && full_frame.contains("Subagent prompt cache TTL"),
            "expected already-good green check section: {full_frame}"
        );
        app.expanded = Some(Focus::Group(0));
        for unicode in [true, false] {
            app.unicode = unicode;
            for width in [52, 72, 80, 100] {
                let lines = app.render(width, 32);
                assert!(lines.len() <= 32, "height {} > 32", lines.len());
                let joined = lines.join("\n");
                assert!(
                    !joined.contains("settings.json"),
                    "checklist/proof must not show settings file paths: {joined}"
                );
                for line in &lines {
                    assert!(
                        ansi_cells(line) <= width,
                        "line wider than {width} cells ({}): {line:?}",
                        ansi_cells(line)
                    );
                }
            }
        }

        // Read-only `--defaults` preview mode renders read-only pill and exit hint.
        app.unicode = true;
        app.hero.read_only = true;
        let ro_frame = app.render(90, 32).join("\n");
        assert!(ro_frame.contains("--defaults · read-only"), "{ro_frame}");
        assert!(ro_frame.contains("[q] Exit preview"), "{ro_frame}");
        assert!(!ro_frame.contains("Review diff"), "{ro_frame}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
