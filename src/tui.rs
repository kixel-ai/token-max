//! Inline interactive terminal UI for `token-max doctor` and `token-max vibes`.
//!
//! Renders an inline viewport in normal scrollback (no alternate screen) with synchronized
//! updates, safe-default pre-selection, live Plan/API billing recalculation, real measured
//! ASCII proof charts, and a two-keypress path (`Enter`, then `y`) from findings to
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
    Event, Fix, How, Kind, Proof, Reason, Style, Total, Walking, home_in, section_value, target,
    why,
};
use crate::report::{Origin, UP_TO, about, count, plural, share_text};
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

/// The smallest terminal the checklist draws in: narrower rows would wrap, which the inline
/// viewport cannot count, and fewer would push the keys off screen.
const MIN_COLS: usize = 48;
const MIN_ROWS: usize = 20;

/// Whether stdout's terminal is big enough for the checklist, which the line-by-line walk
/// takes the place of when it is not.
pub(crate) fn fits() -> bool {
    crossterm::terminal::size()
        .is_ok_and(|(cols, rows)| usize::from(cols) >= MIN_COLS && usize::from(rows) >= MIN_ROWS)
}

/// Summary figures displayed in the Hero Savings Card.
#[derive(Clone, Debug, Default)]
pub(crate) struct Hero {
    pub requests: u64,
    pub sessions: u64,
    pub spend_text: String,
    pub monthly_spend: f64,
    pub credit: Option<String>,
    pub caveats: Vec<String>,
    pub host_count: usize,
    pub vibes_pending: usize,
    /// Already-good settings (shown with green `✓` in the Measured scorecard).
    pub kept: Vec<KeptItem>,
    /// Count of kept features (tools, skills, MCP servers and the like) saving under $1/mo,
    /// folded into a single summary line.
    pub kept_features: usize,
}

/// One already-good setting shown with a green `✓` in the Measured checklist.
#[derive(Clone, Debug)]
pub(crate) struct KeptItem {
    pub host_label: Option<String>,
    pub name: String,
    pub value: String,
    pub saving_usd: Option<f64>,
    /// Whether the saving holds only while a default is the other value.
    pub up_to: bool,
    /// The profile it is kept on, when another profile keeping it compares differently.
    pub profile: Option<String>,
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
#[derive(Debug, Default)]
pub(crate) struct WriteResult {
    pub files_written: usize,
    pub lines: Vec<String>,
    /// The write that failed, after the hosts in `lines` were written: those writes stand,
    /// so the receipt prints them before the error.
    pub failed: Option<ScanError>,
}

/// One vibes card projected for the Vibes tab.
#[derive(Clone, Debug)]
pub(crate) struct VibeRow {
    pub host: usize,
    pub host_label: Option<String>,
    /// The card as read, then each version rebuilt on accepting another card's change.
    pub versions: Vec<VibeVersion>,
    /// Which of `versions` shows, as [`resolve_vibes`] picks it.
    pub shown: usize,
    pub checked: bool,
    /// Shown without its change, which `doctor` offers for this host once its checklist
    /// closes, as an SSH destination's cards are while `doctor`'s checklist runs.
    pub read_only: bool,
}

/// One version of a vibes card, and the change it offers.
#[derive(Clone, Debug)]
pub(crate) struct VibeVersion {
    /// The changes whose acceptance on an earlier card shows this version; empty for the
    /// card as read.
    pub after: Vec<Origin>,
    pub item: VibeItem,
    /// Index into the host's vibes `Offers::offers`, when it offers one.
    pub offer: Option<usize>,
    /// The report lines its offer's change comes from, which accepting it accepts.
    pub accepts: Vec<Origin>,
}

impl VibeRow {
    /// The version showing.
    pub(crate) fn item(&self) -> &VibeItem {
        &self.versions[self.shown].item
    }

    /// The offer the showing version makes.
    pub(crate) fn offer(&self) -> Option<usize> {
        self.versions[self.shown].offer
    }
}

/// Show each card's version as `vibes`' own walk does: the first rebuilt on a change checked on
/// an earlier card of its host, else the card as read. A card whose version changes, or that
/// offers nothing, is unchecked, so a check always means the change its card shows.
pub(crate) fn resolve_vibes(rows: &mut [VibeRow]) {
    let mut accepted: Vec<(usize, Origin)> = vec![];
    for row in rows.iter_mut() {
        let host = row.host;
        let was = row.shown;
        row.shown = row
            .versions
            .iter()
            .skip(1)
            .position(|v| v.after.iter().any(|o| accepted.contains(&(host, *o))))
            .map_or(0, |p| p + 1);
        let shown = &row.versions[row.shown];
        if shown.offer.is_none() || row.shown != was {
            row.checked = false;
        }
        if row.checked {
            accepted.extend(shown.accepts.iter().map(|o| (host, *o)));
        }
    }
}

/// Host callbacks connecting the pure TUI state to local/SSH file planning and writing.
pub(crate) trait Bridge {
    /// Preview exact diffs for `events` (one `Vec<Event>` per host) on the measured changes.
    fn preview_measured(&mut self, events: &[Vec<Event>]) -> Result<DiffPreview, ScanError>;
    /// Atomically write the last previewed measured selections.
    fn write_measured(&mut self) -> WriteResult;
    /// Load vibes entries (lazy on first switch to Vibes tab, or immediate for `vibes` command).
    fn load_vibes(&mut self) -> Result<Vec<VibeRow>, ScanError>;
    /// Preview exact diffs for the currently checked vibes `rows`.
    fn preview_vibes(&mut self, rows: &[VibeRow]) -> Result<DiffPreview, ScanError>;
    /// Atomically write the last previewed vibes selections.
    fn write_vibes(&mut self) -> WriteResult;
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
    host_label: Option<String>,
    offer_index: usize,
    fix: Fix,
    entries_count: usize,
    denials_count: usize,
    checked: bool,
    /// Whether the person checked or unchecked it, so a billing change keeps their answer.
    answered: bool,
    /// Removals the offer leaves by hand, the step and why, printed once it is accepted.
    manual: Vec<(String, String)>,
    /// Also remove the configuration entries and add the `deniedMcpServers` entries the offer
    /// names, which the plain walk asks apart after a yes: off until chosen with `r`.
    extras: bool,
}

impl MemberItem {
    fn has_extras(&self) -> bool {
        self.entries_count + self.denials_count > 0
    }
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

/// One top-level row in the Measured checklist: a section across every host, as the walk
/// asks it, so a section's total decides whether it folds the same way `vibes` reads it.
#[derive(Clone, Debug)]
struct Group {
    /// The host every member is on, when the walk covers another host than this one and all
    /// members share it.
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
    /// Rows scrolled down the diff, clamped by each render to what the diff has.
    diff_scroll: std::cell::Cell<usize>,
    /// Rows scrolled into a focused block taller than the list's window, clamped by each
    /// render to what the block has.
    block_scroll: std::cell::Cell<usize>,
    /// Whether the last frame fit the terminal whole, keys included.
    whole: std::cell::Cell<bool>,
    /// Whether an earlier checklist in this run wrote settings, so leaving says none further.
    pub(crate) wrote: bool,
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
        mut hero: Hero,
        start_tab: Tab,
        color: bool,
        unicode: bool,
    ) -> Self {
        for k in &mut hero.kept {
            for text in [&mut k.name, &mut k.value, &mut k.summary]
                .into_iter()
                .chain(k.host_label.as_mut())
                .chain(k.default_value.as_mut())
            {
                *text = terminal_text(text);
            }
        }
        let mut groups: Vec<Group> = vec![];
        let mut cleanups: Vec<CleanupItem> = vec![];
        let mut by_hand: Vec<(Option<String>, String, String, String)> = vec![];
        let mut has_unset_main_ttl = false;

        for (host_idx, walking) in hosts.iter().enumerate() {
            let label = walking.label.map(terminal_text);
            for (offer_index, offer) in walking.offers.offers.iter().enumerate() {
                let fix = &offer.fix;
                if is_unset_main_ttl(fix) {
                    has_unset_main_ttl = true;
                }
                let member = MemberItem {
                    host: host_idx,
                    host_label: label.clone(),
                    offer_index,
                    fix: clean_fix(fix, walking.style.home.as_deref()),
                    entries_count: offer.entries.len(),
                    denials_count: offer.denials.len(),
                    checked: false,
                    answered: false,
                    manual: offer
                        .manual
                        .iter()
                        .map(|(step, reason)| {
                            (home_in(step, walking.style.home.as_deref()), reason.clone())
                        })
                        .collect(),
                    extras: false,
                };
                // Costly tradeoffs (e.g. a single $87/mo skill like `dataviz`) stay ungrouped
                // as their own top-level rows with their own bar graph so huge outliers stand out.
                let keep_individual = matches!(fix.section.kind, Kind::Feature(_, Reason::Costly));
                if !keep_individual
                    && let Some(existing) = groups.iter_mut().find(|g| g.members[0].fix.joins(fix))
                {
                    if existing.host_label != member.host_label {
                        existing.host_label = None;
                    }
                    existing.members.push(member);
                } else {
                    groups.push(Group {
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
                    server: terminal_text(&entry.server),
                    checked: false,
                });
            }
            // Servers that are off but can't be removed for the person, as the walk lists them.
            for (step, reason) in &walking.offers.cleanup_by_hand {
                by_hand.push((
                    label.clone(),
                    "MCP servers that are off".into(),
                    home_in(step, walking.style.home.as_deref()),
                    reason.clone(),
                ));
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
                .sort_by(|a, b| crate::report::by_saving(a.fix.saving, b.fix.saving));
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
            crate::report::by_saving(
                group_usd(a, BillingMode::Auto),
                group_usd(b, BillingMode::Auto),
            )
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
            diff_scroll: std::cell::Cell::new(0),
            block_scroll: std::cell::Cell::new(0),
            whole: std::cell::Cell::new(false),
            wrote: false,
            diff_tab: start_tab,
            status_error: None,
            host_count: hosts.len(),
            color,
            unicode,
        }
    }

    /// Show the Vibes tab's `rows`, their text escaped and each card's version resolved.
    pub(crate) fn set_vibes(&mut self, mut rows: Vec<VibeRow>) {
        for row in &mut rows {
            if let Some(label) = &mut row.host_label {
                *label = terminal_text(label);
            }
            for v in &mut row.versions {
                let item = &mut v.item;
                for text in [
                    &mut item.name,
                    &mut item.value,
                    &mut item.what,
                    &mut item.text,
                ]
                .into_iter()
                .chain(item.sources.iter_mut())
                {
                    *text = terminal_text(text);
                }
            }
        }
        resolve_vibes(&mut rows);
        self.vibes = Some(rows);
    }

    /// How leaving now reads: no settings changed, or none further after an earlier write.
    fn unchanged(&self) -> &'static str {
        if self.wrote {
            "no further settings were changed"
        } else {
            "no settings were changed"
        }
    }

    /// The steps by hand to print on leaving: each change's that is one, the cleanup of
    /// servers that are off, and, once `wrote` says the measured changes were written, the
    /// removals each of them leaves by hand.
    fn steps_by_hand(&self, wrote: bool) -> Vec<(Option<String>, String, String, String)> {
        let mut steps = self.by_hand.clone();
        for m in self.groups.iter().flat_map(|g| &g.members) {
            if wrote && m.checked {
                for (step, reason) in &m.manual {
                    steps.push((
                        m.host_label.clone(),
                        m.fix.name.clone(),
                        step.clone(),
                        reason.clone(),
                    ));
                }
            }
        }
        steps
    }

    /// Whether open caveats take `code`: one that would move or toggle a row they may hide
    /// closes them instead, leaving them only their scroll, their toggle and the exits.
    fn caveats_take(&mut self, code: KeyCode) -> bool {
        let theirs = matches!(
            code,
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::Char('?' | 'q') | KeyCode::Esc
        );
        if self.screen != Screen::List || !self.show_caveats || theirs {
            return false;
        }
        self.show_caveats = false;
        self.block_scroll.set(0);
        true
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

    /// The checked changes' savings as the walk's running total adds them, the billing toggle
    /// applied: a saving that holds only under one default stays "up to" until billing names
    /// it, apart from the measured ones.
    fn selected_total(&self) -> Total {
        let fixes: Vec<Fix> = self
            .groups
            .iter()
            .flat_map(|g| &g.members)
            .filter(|m| m.checked)
            .map(|m| {
                let mut fix = m.fix.clone();
                if is_unset_main_ttl(&fix) && self.billing != BillingMode::Auto {
                    fix.saving = effective_saving(&fix, self.billing);
                    fix.verb = "saves";
                }
                fix
            })
            .collect();
        Total::of(&fixes)
    }

    /// The checked changes' savings, such as `Save ~$40/mo ± $3, up to $90/mo by billing`;
    /// None when nothing checked was measured.
    fn savings_text(&self, verb: &str) -> Option<String> {
        match self.selected_total().figures() {
            (Some(measured), Some(up_to)) => {
                Some(format!("{verb} {measured}, up to {up_to} more by billing"))
            }
            (Some(measured), None) => Some(format!("{verb} {measured}")),
            (None, Some(up_to)) => Some(format!("{verb} up to {up_to}, by billing")),
            (None, None) => None,
        }
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
                        .filter(|(_, r)| !r.item().quiet || self.show_minor)
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
                    (m.checked, m.answered) = (next, true);
                }
            }
            Focus::Member(gi, mi) => {
                let m = &mut self.groups[gi].members[mi];
                (m.checked, m.answered) = (!m.checked, true);
            }
            Focus::Cleanup(ci) => {
                self.cleanups[ci].checked = !self.cleanups[ci].checked;
            }
            Focus::Kept(_) => {}
            Focus::Vibe(vi) => {
                if let Some(vibes) = &mut self.vibes
                    && let Some(row) = vibes.get_mut(vi)
                    && row.offer().is_some()
                {
                    row.checked = !row.checked;
                    resolve_vibes(vibes);
                }
            }
        }
    }

    /// Toggle whether the focused change also removes the configuration and adds the blocks
    /// its offer names; a group toggles every member that has some.
    pub(crate) fn toggle_extras(&mut self) {
        let members: Vec<&mut MemberItem> = match self.current_focus() {
            Some(Focus::Group(gi)) => self.groups[gi]
                .members
                .iter_mut()
                .filter(|m| m.has_extras())
                .collect(),
            Some(Focus::Member(gi, mi)) => {
                let m = &mut self.groups[gi].members[mi];
                if m.has_extras() { vec![m] } else { vec![] }
            }
            _ => vec![],
        };
        let next = !members.iter().all(|m| m.extras);
        for m in members {
            m.extras = next;
        }
    }

    /// The line under a change offering removals or blocks, saying whether they are chosen.
    fn extras_line<'a>(
        &self,
        members: impl Iterator<Item = &'a MemberItem> + Clone,
        indent: &str,
        w: usize,
    ) -> Option<String> {
        let with: Vec<&MemberItem> = members.filter(|m| m.has_extras()).collect();
        if with.is_empty() {
            return None;
        }
        let entries: usize = with.iter().map(|m| m.entries_count).sum();
        let denials: usize = with.iter().map(|m| m.denials_count).sum();
        let mut what = vec![];
        if entries > 0 {
            what.push(format!(
                "remove {entries} config entr{}",
                if entries == 1 { "y" } else { "ies" }
            ));
        }
        if denials > 0 {
            what.push(format!("block {denials} by name wherever configured"));
        }
        let on = with.iter().all(|m| m.extras);
        let box_str = if on {
            self.style(GREEN, if self.unicode { "[✓]" } else { "[x]" })
        } else {
            self.style(DIM, "[ ]")
        };
        let text = self.fit_text(
            &format!("also {}  [r]", what.join(" and ")),
            w.saturating_sub(cells(indent) + 4),
        );
        Some(trim_ansi(
            &format!("{indent}{box_str} {}", self.style(DIM, &text)),
            w,
        ))
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
                m.answered = true;
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
        if self.tab != Tab::Measured || !self.has_unset_main_ttl {
            return;
        }
        self.billing = self.billing.next();
        for g in &mut self.groups {
            for m in &mut g.members {
                if is_unset_main_ttl(&m.fix) && !m.answered {
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
                if m.checked && m.extras {
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
        // A terminal shrunk below the checklist's size shows only how to go on, and `y`
        // writes nothing until the confirm is whole on screen again.
        if width < MIN_COLS || max_rows + 1 < MIN_ROWS {
            self.whole.set(false);
            let text = format!("  Enlarge the terminal to {MIN_COLS}x{MIN_ROWS}, or press q.");
            return vec![self.style(YELLOW, &fit_plain(&text, width.max(1), self.unicode))];
        }
        let w = width.min(104);
        let mut lines = match self.screen {
            Screen::List => self.render_list(w, max_rows),
            Screen::Diff => self.render_diff(w, max_rows),
        };
        self.whole.set(lines.len() <= max_rows);
        lines.truncate(max_rows);
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
    /// The summary box; `compact` keeps only its headline, unboxed, for a short terminal.
    fn render_hero(&self, w: usize, compact: bool) -> Vec<String> {
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
        let headline_rows;
        match self.tab {
            Tab::Measured => {
                let sel = self.selected_usd();
                let avail = self.total_available_usd();
                // The share is of the measured savings alone, as the walk's running total says.
                let measured: f64 = self
                    .groups
                    .iter()
                    .flat_map(|g| &g.members)
                    .filter(|m| m.checked)
                    .filter(|m| !is_unset_main_ttl(&m.fix) || self.billing != BillingMode::Auto)
                    .filter_map(|m| effective_saving(&m.fix, self.billing))
                    .sum();
                let pct = (self.hero.monthly_spend > 0.0 && measured > 0.0)
                    .then(|| share_text(measured / self.hero.monthly_spend))
                    .flatten()
                    .map_or_else(String::new, |s| format!(" ({s} of spend)"));
                let mut pills = vec![];
                if self.has_unset_main_ttl {
                    pills.push(format!("[b] Billing: {}", self.billing.label()));
                }
                let right_pill = pills.join("  ");
                let left: Vec<String> = if self.groups.is_empty() && self.cleanups.is_empty() {
                    let ok = if self.unicode { "✓" } else { "OK" };
                    vec![format!(
                        "{} {}",
                        self.style(&format!("{BOLD}{GREEN}"), &format!("{ok} All clear")),
                        self.style(DIM, "no changes recommended")
                    )]
                } else {
                    let one = self
                        .savings_text("Save")
                        .unwrap_or_else(|| "Select changes to preview".into());
                    // Too wide for one line, the measured figure and its share keep the first
                    // and the billing one takes the next, so neither loses its margin.
                    let (first, rest) = match self.selected_total().figures() {
                        (Some(measured), Some(up_to)) if cells(&one) + cells(&pct) > inner => (
                            format!("Save {measured}"),
                            Some(format!("up to {up_to} more by billing")),
                        ),
                        _ => (one, None),
                    };
                    let wrap = |text: &str| -> Vec<String> {
                        wrap_text(
                            text,
                            WrapOptions {
                                width: Some(inner),
                                ..Default::default()
                            },
                        )
                        .lines()
                        .map(str::to_string)
                        .collect()
                    };
                    let green = format!("{BOLD}{GREEN}");
                    let mut lines: Vec<String> = if cells(&first) + cells(&pct) <= inner {
                        vec![format!(
                            "{}{}",
                            self.style(&green, &first),
                            self.style(DIM, &pct)
                        )]
                    } else {
                        let mut lines: Vec<String> =
                            wrap(&first).iter().map(|l| self.style(&green, l)).collect();
                        lines.extend(wrap(pct.trim()).iter().map(|l| self.style(DIM, l)));
                        lines
                    };
                    for line in rest.iter().flat_map(|r| wrap(r)) {
                        lines.push(self.style(&green, &line));
                    }
                    lines
                };
                let right = (!right_pill.is_empty()).then(|| self.style(BRAND, &right_pill));
                match (left.as_slice(), right) {
                    ([only], Some(right)) if ansi_cells(only) + ansi_cells(&right) + 2 <= inner => {
                        let gap = inner - ansi_cells(only) - ansi_cells(&right);
                        body_lines.push(format!("{only}{}{right}", " ".repeat(gap)));
                    }
                    (_, right) => {
                        body_lines.extend(left.iter().cloned());
                        body_lines.extend(right);
                    }
                }
                headline_rows = body_lines.len();

                let mut parts = vec![];
                if self.hero.sessions > 0 {
                    parts.push(format!("{} sessions", count(self.hero.sessions)));
                }
                if self.hero.requests > 0 {
                    parts.push(format!("{} reqs", count(self.hero.requests)));
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
                    v.iter().filter(|r| !r.item().quiet).count()
                });
                body_lines.push(format!(
                    "{} {}",
                    self.style(&format!("{BOLD}{BRAND}"), "token-max vibes"),
                    self.style(
                        DIM,
                        &format!("{count} unmeasured tips{}no verdict", self.sep())
                    )
                ));
                headline_rows = 1;
                body_lines.push(self.style(
                    DIM,
                    "Tips transcripts can't price. Unchecked by default — Space to opt in.",
                ));
            }
        }

        if compact {
            let mut out: Vec<String> = body_lines[..headline_rows]
                .iter()
                .map(|l| trim_ansi(&format!("  {l}"), w))
                .collect();
            out.push(String::new());
            return out;
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
        let mut head = vec![];

        // Optional tab strip when both Measured and Vibes are relevant.
        if !self.start_vibes_only && (self.hero.vibes_pending > 0 || self.vibes.is_some()) {
            let m_cnt = self.groups.len() + self.cleanups.len();
            let v_cnt = self.vibes.as_deref().map_or(self.hero.vibes_pending, |v| {
                v.iter().filter(|r| !r.item().quiet).count()
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
                    "  {tri} {} under $1/mo (+{}/mo)",
                    plural(minor_cnt, "minor tweak"),
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
            let by_hand = self.steps_by_hand(true).len();
            if by_hand > 0 {
                foot.push(self.style(
                    DIM,
                    &self.fit_text(
                        &format!(
                            "  ! {} by hand (shown after writing)",
                            plural(by_hand, "step")
                        ),
                        w,
                    ),
                ));
            }
        } else if let Some(vibes) = &self.vibes {
            let quiet_cnt = vibes.iter().filter(|r| r.item().quiet).count();
            if quiet_cnt > 0 {
                let tri = if self.show_minor {
                    if self.unicode { "▾" } else { "v" }
                } else if self.unicode {
                    "▸"
                } else {
                    ">"
                };
                let left = format!("  {tri} {}", plural(quiet_cnt, "already-aligned item"));
                let right = if self.show_minor {
                    "[m] hide"
                } else {
                    "[m] show"
                };
                let gap = w.saturating_sub(cells(&left) + cells(right) + 2).max(1);
                foot.push(self.style(DIM, &format!("{left}{}{right}", " ".repeat(gap))));
            }
        }

        // Caveats read in the body, which scrolls, so they never push the keys off the frame.
        let mut caveats = vec![];
        if self.show_caveats && !self.hero.caveats.is_empty() {
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
                    caveats.push(self.style(DIM, wrapped));
                }
            }
            caveats.push(String::new());
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
        let primary = if checked > 0 {
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
        keys.push(self.style(DIM, "[q] quit"));
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
                    "        {} {} within budget",
                    self.style(GREEN, ok),
                    plural(n, "feature"),
                );
                body_blocks.push((false, vec![String::new(), self.style(DIM, &msg)]));
            }
        }

        if !caveats.is_empty() {
            for block in &mut body_blocks {
                block.0 = false;
            }
            body_blocks.insert(0, (true, caveats));
        }

        // Window body lines so head + body + foot <= max_rows, the summary shrinking to its
        // headline when the body would get fewer than four.
        let mut hero = self.render_hero(w, false);
        if hero.len() + head.len() + foot.len() + 4 > max_rows {
            hero = self.render_hero(w, true);
        }
        hero.append(&mut head);
        let head = hero;
        let budget = max_rows.saturating_sub(head.len() + foot.len()).max(2);
        let (body, max_scroll) = window_blocks(
            &body_blocks,
            budget,
            w,
            self.color,
            self.unicode,
            self.block_scroll.get(),
        );
        self.block_scroll
            .set(self.block_scroll.get().min(max_scroll));

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
        if !g.split
            && let Some(line) = self.extras_line(g.members.iter(), "          ", w)
        {
            rows.push(line);
        }

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
                    count(uses),
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
        let name_fit = self.fit_text(&member_name(m, &m.fix.name), left_budget);
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
        if let Some(line) = self.extras_line(std::iter::once(m), "              ", w) {
            rows.push(line);
        }
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
            Some(u) if u > 0.0 && k.up_to => format!("up to +{}/mo", about(u)),
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
        // The profile leads, as the host does, so a narrow row cuts the name rather than it.
        // A directory's name may hold controls, so it prints escaped wherever it shows.
        let profile = k.profile.as_deref().map(terminal_text);
        let profile_tag = profile
            .as_deref()
            .map(|p| format!("[{p}] "))
            .unwrap_or_default();
        let full_title = format!("{host_tag}{profile_tag}{}", k.name);
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
                let up_to = if k.up_to { "up to " } else { "" };
                tags.push(format!("saving {up_to}~{}/mo vs {def} default", about(u)));
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
            let summary = match &profile {
                Some(profile) => format!("On {profile}: {}", k.summary),
                None => k.summary.clone(),
            };
            for line in wrap_text(
                &summary,
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
        let box_str = if v.offer().is_none() {
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
        let val = if v.item().value.is_empty() {
            String::new()
        } else {
            format!("  {}", self.style(DIM, &terminal_text(&v.item().value)))
        };
        let source = concise_vibe_source(&v.item().sources);
        let tag = self.style(DIM, source);
        let name = self.fit_text(&format!("{host}{}", v.item().name), w.saturating_sub(24));
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
                let item = v.item();
                let mut paragraphs = vec![];
                if !item.what.is_empty() {
                    paragraphs.push(item.what.clone());
                }
                paragraphs.push(item.text.clone());
                if !item.sources.is_empty() {
                    paragraphs.push(format!("Source: {}", item.sources.join(" ")));
                }
                // A change this checklist can't write is a step by hand, as `vibes`' walk says.
                if let Some(How::Manual { step, reason }) = &item.how {
                    paragraphs.push(format!(
                        "Change by hand, since {}: {}",
                        terminal_text(reason),
                        terminal_text(step)
                    ));
                }
                if v.read_only && matches!(item.how, Some(How::Edit(_))) {
                    paragraphs.push(
                        "Its edit is offered on this host once this checklist closes, when doctor asks to show vibes' advice."
                            .into(),
                    );
                }
                for paragraph in paragraphs {
                    for line in wrap_text(
                        &paragraph,
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

    /// The bar down a proof drawer's left edge.
    fn proof_pipe(&self) -> String {
        self.style(BRAND, if self.unicode { "│" } else { "|" })
    }

    /// Render the expandable inline proof drawer for a group (lists every member cleanly with bars, no cutoffs, no file paths).
    fn render_group_proof(&self, g: &Group, w: usize) -> Vec<String> {
        if g.members.len() == 1 {
            return self.render_fix_proof(&g.members[0].fix, w);
        }
        let pipe = self.proof_pipe();
        let wrap_w = proof_wrap(w);
        let mut out = vec![];

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
        // One chart speaks for the group only when every member was replayed and all favor
        // the same TTL; otherwise each member shows its own comparison.
        let winners: Vec<bool> = g
            .members
            .iter()
            .filter_map(|m| match m.fix.proof {
                Some(Proof::CacheTtl {
                    five_m_usd,
                    one_h_usd,
                    ..
                }) => Some(one_h_usd <= five_m_usd),
                _ => None,
            })
            .collect();
        let agree = winners.windows(2).all(|w| w[0] == w[1]);
        if ttl_count == g.members.len() && agree && (five > 0.0 || one > 0.0) {
            out.push(trim_ansi(&format!("      {pipe}"), w));
            out.extend(self.render_ttl_chart(five, one, reqs, wrap_w, &pipe, w));
        } else if ttl_count > 0 {
            for m in &g.members {
                out.push(trim_ansi(&format!("      {pipe}"), w));
                let name = member_name(m, &m.fix.section.member);
                out.push(trim_ansi(
                    &format!(
                        "      {pipe} {}",
                        self.style(BOLD, &format!("{name}: {}", m.fix.value))
                    ),
                    w,
                ));
                match m.fix.proof {
                    Some(Proof::CacheTtl {
                        five_m_usd,
                        one_h_usd,
                        requests,
                    }) => out.extend(
                        self.render_ttl_chart(five_m_usd, one_h_usd, requests, wrap_w, &pipe, w),
                    ),
                    _ => out.push(trim_ansi(
                        &format!("      {pipe} {}", self.style(DIM, "not replayed")),
                        w,
                    )),
                }
            }
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
                .map(|m| cells(&member_name(m, &m.fix.section.member)))
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
                let name = fit_plain(
                    &member_name(m, &m.fix.section.member),
                    max_name,
                    self.unicode,
                );
                let pad = " ".repeat(max_name.saturating_sub(cells(&name)));
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
        let pipe = self.proof_pipe();
        let wrap_w = proof_wrap(w);
        let mut out = vec![];

        // The report line's own words: what the setting does, then what was measured.
        for paragraph in [fix.what.as_str(), why(&fix.evidence)] {
            if paragraph.is_empty() {
                continue;
            }
            for line in wrap_text(
                paragraph,
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
                        w,
                    ));
                }
                Proof::Unused { requests, tokens } => {
                    let tok = tokens
                        .map(|t| format!(" (~{} tok/req)", count(t.max(0))))
                        .unwrap_or_default();
                    let line = format!("0 uses across {} requests{tok}", count(requests));
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
        let hdr = format!("Replayed cache cost on {} reqs (per 30d):", count(requests));
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

    fn render_cpu_chart(
        &self,
        cost_per_use: f64,
        threshold: f64,
        uses: u64,
        requests: u64,
        w: usize,
    ) -> Vec<String> {
        let pipe = self.proof_pipe();
        let wrap_w = proof_wrap(w);
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
            "Cost vs usage ({} across {} reqs):",
            plural(uses, "use"),
            count(requests)
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
                            "{}{} across {}",
                            self.sep(),
                            plural(diff.change_count, "change"),
                            plural(diff.file_count, "file")
                        )
                    )
                ),
                w,
            ),
            self.style(DIM, &format!(" {}", rule.repeat(w.saturating_sub(2)))),
            String::new(),
        ];

        let backups: Vec<String> = diff
            .backups
            .iter()
            .map(|b| self.style(DIM, &self.fit_text(&format!("  {b}"), w)))
            .collect();
        let mut foot = vec![];
        if let Some(err) = &self.status_error {
            foot.push(self.style(RED, &self.fit_text(&format!("  ! {err}"), w)));
        }
        foot.push(String::new());
        foot.push(self.style(DIM, &format!(" {}", rule.repeat(w.saturating_sub(2)))));
        foot.push(trim_ansi(
            &format!(
                "  {}  {}  {}",
                self.style(&format!("{BOLD}{GREEN}"), "[y] Write settings"),
                self.style(DIM, "[Esc] Back"),
                self.style(DIM, "[q] Cancel"),
            ),
            w,
        ));

        let mut raw_lines: Vec<String> = diff.text.lines().flat_map(|l| wrap_ansi(l, w)).collect();
        // Backups stay beside the keys while five diff rows still fit, else they scroll last.
        if head.len() + foot.len() + backups.len() + 5 <= max_rows {
            foot.splice(0..0, backups);
        } else {
            raw_lines.push(String::new());
            raw_lines.extend(backups);
        }
        let mut budget = max_rows.saturating_sub(head.len() + foot.len()).max(4);
        // A diff that scrolls gives one of its rows to the scroll line, so the keys stay on screen.
        if raw_lines.len() > budget {
            budget = budget.saturating_sub(1).max(1);
        }
        let max_scroll = raw_lines.len().saturating_sub(budget);
        let scroll = self.diff_scroll.get().min(max_scroll);
        self.diff_scroll.set(scroll);
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

/// Run the interactive inline TUI until the user writes changes, quits, or cancels.
pub(crate) fn run(
    mut app: App,
    style: &Style,
    bridge: &mut impl Bridge,
) -> Result<ExitKind, ScanError> {
    if app.tab == Tab::Vibes {
        app.set_vibes(bridge.load_vibes()?);
    }

    // All-clear fast path: print crisp Hero Card + already-good green checks + manual steps.
    if app.is_all_clear() {
        let width = style.width.unwrap_or(80).clamp(48, 104);
        let mut stdout = io::stdout().lock();
        let failed = |_| ScanError::new(format!("cannot use the terminal; {}", app.unchanged()));
        for line in app.render_hero(width, false) {
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
                // A row naming its profile prints its proof too, which names it whole, since
                // nothing is left to expand once this report prints.
                let expand = app.hero.kept[ki].profile.is_some();
                for line in app.render_kept_row(ki, false, expand, max_kept_usd, width) {
                    writeln!(stdout, "{line}").map_err(failed)?;
                }
            }
            writeln!(stdout).map_err(failed)?;
        }
        write_by_hand(&mut stdout, &app.steps_by_hand(false), style.color).map_err(failed)?;
        stdout.flush().map_err(failed)?;
        return Ok(ExitKind::Done);
    }

    let _raw = RawGuard::enter().map_err(|_| {
        ScanError::new("cannot enable interactive terminal mode; rerun with --plain")
    })?;
    // Unlocked, so a signal can restore the terminal while this waits on a key.
    let mut stdout = io::stdout();
    let mut viewport = InlineViewport::default();

    loop {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 28));
        let frame = app.render(cols as usize, (rows as usize).saturating_sub(1));
        viewport
            .draw(&mut stdout, &frame)
            .map_err(|_| ScanError::new(format!("cannot draw to terminal; {}", app.unchanged())))?;

        let Ok(ev) = event::read() else {
            viewport.clear(&mut stdout).ok();
            return Err(ScanError::new(format!(
                "cannot read the terminal; {}",
                app.unchanged()
            )));
        };
        let TermEvent::Key(KeyEvent {
            code,
            modifiers,
            kind,
            ..
        }) = ev
        else {
            continue;
        };
        // Only moving and scrolling repeat when a key is held; every action takes its own press.
        let moves = matches!(
            code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Char('j' | 'k')
        );
        if kind == KeyEventKind::Release || (kind == KeyEventKind::Repeat && !moves) {
            continue;
        }

        // Ctrl-C always cancels cleanly without touching files.
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c' | 'C')) {
            viewport.clear(&mut stdout).ok();
            return Ok(ExitKind::Cancelled);
        }

        app.status_error = None;

        // Open caveats can fill the body and hide every row, so a key that would move or
        // toggle one closes them instead; they keep only their scroll, toggle and exits.
        if app.caveats_take(code) {
            continue;
        }
        if !matches!(code, KeyCode::PageUp | KeyCode::PageDown) {
            app.block_scroll.set(0);
        }
        match app.screen {
            Screen::List => match code {
                KeyCode::PageDown => {
                    app.block_scroll.set(app.block_scroll.get() + 1);
                }
                KeyCode::PageUp => {
                    app.block_scroll
                        .set(app.block_scroll.get().saturating_sub(1));
                }
                KeyCode::Char('q') => {
                    viewport.clear(&mut stdout).ok();
                    drop(_raw);
                    let _ = writeln!(
                        stdout,
                        "{}",
                        app.style(DIM, &format!("  Nothing written; {}.", app.unchanged()))
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
                            app.style(DIM, &format!("  Nothing written; {}.", app.unchanged()))
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
                KeyCode::Char('r') => {
                    app.toggle_extras();
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
                                    Ok(rows) => app.set_vibes(rows),
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
                    if app.checked_count() == 0 {
                        viewport.clear(&mut stdout).ok();
                        drop(_raw);
                        let _ = writeln!(
                            stdout,
                            "{}",
                            app.style(DIM, &format!("  Nothing selected; {}.", app.unchanged()))
                        );
                        let _ = write_by_hand(&mut stdout, &app.steps_by_hand(false), app.color);
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
                            app.diff_scroll.set(0);
                            app.diff_tab = app.tab;
                            app.screen = Screen::Diff;
                        }
                        Err(error) => {
                            viewport.clear(&mut stdout).ok();
                            return Err(error);
                        }
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
                        app.style(DIM, &format!("  Cancelled; {}.", app.unchanged()))
                    );
                    let _ = stdout.flush();
                    return Ok(ExitKind::Done);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.diff_scroll.set(app.diff_scroll.get().saturating_sub(1));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    app.diff_scroll.set(app.diff_scroll.get().saturating_add(1));
                }
                // Enter opened this screen, so writing takes `y`: a held or doubled Enter, which
                // a terminal sends as more presses, never writes before the diff is read.
                KeyCode::Char('y') if !app.whole.get() => {
                    app.status_error = Some("Enlarge the terminal to see the whole diff.".into());
                }
                KeyCode::Char('y') => {
                    let saved = (app.diff_tab == Tab::Measured)
                        .then(|| app.savings_text("Saved"))
                        .flatten();
                    let written = match app.diff_tab {
                        Tab::Measured => bridge.write_measured(),
                        Tab::Vibes => bridge.write_vibes(),
                    };
                    if let Some(failed) = written.failed {
                        // What was written before the failure stands, so it prints first.
                        viewport.clear(&mut stdout).ok();
                        drop(_raw);
                        for line in &written.lines {
                            let _ = writeln!(stdout, "  {line}");
                        }
                        let _ = stdout.flush();
                        return Err(failed);
                    }
                    // Snappy coin-drop celebration before printing the permanent receipt!
                    play_coin_drop(&mut stdout, &mut viewport, app.color, app.unicode);
                    viewport.clear(&mut stdout).ok();
                    drop(_raw);

                    let ok = if app.unicode { "✓" } else { "OK" };
                    let files = plural(written.files_written, "file");
                    let headline = match saved {
                        Some(saved) => format!(
                            "  {} {} updated {files}",
                            app.style(&format!("{BOLD}{GREEN}"), &format!("{ok} {saved}")),
                            app.sep().trim(),
                        ),
                        None => format!(
                            "  {}",
                            app.style(&format!("{BOLD}{GREEN}"), &format!("{ok} Updated {files}"))
                        ),
                    };
                    let _ = writeln!(stdout, "{headline}");
                    for line in &written.lines {
                        let _ = writeln!(stdout, "  {}", app.style(DIM, line));
                    }
                    let measured = app.diff_tab == Tab::Measured;
                    let _ = write_by_hand(&mut stdout, &app.steps_by_hand(measured), app.color);
                    let _ = stdout.flush();
                    return Ok(ExitKind::Done);
                }
                _ => {}
            },
        }
    }
}

/// Ask whether to show `vibes`' `count` pieces of advice once `doctor`'s checklist closes,
/// reading one key as the checklist does, since its keys typed ahead wait in the same queue;
/// `y` is a yes, Ctrl-C None, and any other key a no.
pub(crate) fn offer_vibes(count: usize, style: &Style) -> io::Result<Option<bool>> {
    let show = if count == 1 { "Show it?" } else { "Show them?" };
    let prompt = style.ask(
        &format!("{} {show}", crate::vibes::hint(count)),
        "[y]es, [n]o",
    );
    let mut stdout = io::stdout();
    write!(stdout, "\n{}", prompt.trim_end_matches('\n'))?;
    stdout.flush()?;
    let yes = {
        let _raw = RawGuard::enter()?;
        loop {
            match event::read()? {
                TermEvent::Key(KeyEvent {
                    code: KeyCode::Char('c' | 'C'),
                    modifiers,
                    kind: KeyEventKind::Press,
                    ..
                }) if modifiers.contains(KeyModifiers::CONTROL) => break None,
                TermEvent::Key(KeyEvent {
                    code,
                    kind: KeyEventKind::Press,
                    ..
                }) => break Some(matches!(code, KeyCode::Char('y' | 'Y'))),
                _ => continue,
            }
        }
    };
    let answer = match yes {
        Some(true) => "y",
        Some(false) => "n",
        None => "^C",
    };
    writeln!(stdout, " {answer}")?;
    Ok(yes)
}

/// Each host's answers to the offers `rows` show: one per offer, yes when any card showing it
/// is checked, since two cards can name one edit.
pub(crate) fn vibes_events_from(rows: &[VibeRow], host_count: usize) -> Vec<Vec<Event>> {
    let mut events: Vec<Vec<Event>> = (0..host_count).map(|_| vec![]).collect();
    for row in rows {
        let Some(offer) = row.offer() else { continue };
        let answered = events[row.host].iter_mut().find_map(|e| match e {
            Event::Choose { offer: o, yes } if *o == offer => Some(yes),
            _ => None,
        });
        match answered {
            Some(yes) => *yes |= row.checked,
            None => events[row.host].push(Event::Choose {
                offer,
                yes: row.checked,
            }),
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
            .map(|h| format!("[{}] ", terminal_text(h)))
            .unwrap_or_default();
        let (name, step, reason) = (
            terminal_text(name),
            terminal_text(step),
            terminal_text(reason),
        );
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

/// A member's `name` after its host's, when the walk covers another host than this one.
fn member_name(m: &MemberItem, name: &str) -> String {
    match &m.host_label {
        Some(host) => format!("[{host}] {name}"),
        None => name.to_string(),
    }
}

/// `fix` with the text the TUI prints escaped, since names and settings come from transcripts
/// and configuration, or a destination, and may hold terminal controls; its prose names paths
/// under `home` from `~`, as the walk's cards do.
fn clean_fix(fix: &Fix, home: Option<&str>) -> Fix {
    let mut fix = fix.clone();
    fix.what = home_in(&fix.what, home);
    fix.evidence = home_in(&fix.evidence, home);
    for text in [
        &mut fix.name,
        &mut fix.value,
        &mut fix.what,
        &mut fix.evidence,
        &mut fix.section.label,
        &mut fix.section.member,
        &mut fix.profile,
    ] {
        *text = terminal_text(text);
    }
    fix
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

/// Window rendered item blocks so the focused block is always visible within `budget` rows,
/// and return the furthest `scroll` can go. A focused block taller than the window shows the
/// rows from `scroll` on, with a line saying which, so a long proof never pushes out the keys.
fn window_blocks(
    blocks: &[(bool, Vec<String>)],
    budget: usize,
    width: usize,
    color: bool,
    unicode: bool,
    scroll: usize,
) -> (Vec<String>, usize) {
    let total_lines: usize = blocks.iter().map(|(_, l)| l.len()).sum();
    if total_lines <= budget {
        return (
            blocks.iter().flat_map(|(_, l)| l.iter().cloned()).collect(),
            0,
        );
    }
    let focus_idx = blocks.iter().position(|(f, _)| *f).unwrap_or(0);
    let inner_budget = budget.saturating_sub(2).max(1);
    let dim = |msg: String| {
        let fitted = fit_plain(&msg, width, unicode);
        if color {
            format!("{DIM}{fitted}{RESET}")
        } else {
            fitted
        }
    };
    let focused = &blocks[focus_idx].1;
    if focused.len() > inner_budget {
        let room = inner_budget.saturating_sub(1).max(1);
        let max_scroll = focused.len() - room;
        let from = scroll.min(max_scroll);
        let mut out: Vec<String> = focused[from..from + room].to_vec();
        out.push(dim(format!(
            "  (PgUp/PgDn scroll{}lines {}-{} of {})",
            if unicode { " · " } else { " | " },
            from + 1,
            from + room,
            focused.len()
        )));
        return (out, max_scroll);
    }
    let mut start = focus_idx;
    let mut end = focus_idx + 1;
    let mut used = focused.len();
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
        out.push(dim(format!("  {up} {start} more above")));
    }
    for (_, lines) in &blocks[start..end] {
        out.extend(lines.iter().cloned());
    }
    if end < blocks.len() {
        let more = blocks.len() - end;
        out.push(dim(format!("  {down} {more} more below")));
    }
    (out, 0)
}

/// The width a proof drawer's text wraps to in a `w`-column terminal.
fn proof_wrap(w: usize) -> usize {
    w.saturating_sub(11).max(20)
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

/// Split an ANSI-styled line into rows of at most `max_cells` cells, so none of it is cut
/// off; each row closes the style it ends in and the next reopens it.
fn wrap_ansi(s: &str, max_cells: usize) -> Vec<String> {
    let max_cells = max_cells.max(1);
    let mut rows = vec![];
    let mut row = String::new();
    let mut used = 0usize;
    // The SGR sequences in effect since the last reset.
    let mut active = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            let mut seq = String::from(c);
            seq.push(chars.next().expect("peeked '['"));
            let mut code = String::new();
            for next in chars.by_ref() {
                seq.push(next);
                if next.is_ascii_alphabetic() {
                    if next == 'm' {
                        if code.is_empty() || code == "0" {
                            active.clear();
                        } else {
                            active.push_str(&seq);
                        }
                    }
                    break;
                }
                code.push(next);
            }
            row.push_str(&seq);
            continue;
        }
        let mut buf = [0u8; 4];
        let w = cells(c.encode_utf8(&mut buf));
        if used + w > max_cells && used > 0 {
            if !active.is_empty() {
                row.push_str(RESET);
            }
            rows.push(std::mem::take(&mut row));
            row.push_str(&active);
            used = 0;
        }
        row.push(c);
        used += w;
    }
    rows.push(row);
    rows
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
            sessions: 120,
            spend_text: "$1,000".into(),
            monthly_spend: 1000.0,
            credit: Some("Already saving ~$3,546/mo (9%) vs defaults".into()),
            caveats: vec!["Sample caveat".into()],
            host_count: 1,
            vibes_pending: 2,
            kept: vec![
                KeptItem {
                    host_label: None,
                    name: "Subagent prompt cache TTL".into(),
                    value: "1h".into(),
                    saving_usd: Some(3545.0),
                    up_to: false,
                    profile: None,
                    default_value: Some("5m".into()),
                    summary: "Default is 5m — keeping 1h saves ~$3,545/mo.".into(),
                },
                KeptItem {
                    host_label: None,
                    name: "Fast mode".into(),
                    value: "off".into(),
                    saving_usd: None,
                    up_to: false,
                    profile: None,
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
        // An unset main TTL's saving holds only under one default, so it stays "up to" until
        // billing names it.
        assert_eq!(
            app.savings_text("Save").as_deref(),
            Some("Save up to $413/mo, by billing")
        );

        // Live billing toggle: Auto -> API ($413) -> Plan ($0, auto-unchecks unset 1h!).
        app.cycle_billing();
        assert_eq!(app.billing, BillingMode::Api);
        assert!((app.selected_usd() - 413.0).abs() < 0.01);
        assert_eq!(app.savings_text("Save").as_deref(), Some("Save $413/mo"));
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

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn item(name: &str) -> VibeItem {
        VibeItem {
            name: name.into(),
            value: String::new(),
            what: String::new(),
            text: String::new(),
            sources: vec![],
            how: None,
            quiet: false,
            after: vec![],
            defers: None,
        }
    }

    fn version(after: Vec<Origin>, offer: usize, accepts: Vec<Origin>) -> VibeVersion {
        VibeVersion {
            after,
            item: item("card"),
            offer: Some(offer),
            accepts,
        }
    }

    #[test]
    fn a_rebuilt_card_follows_the_check_on_an_earlier_card() {
        // Card 1 offers its own change, and another once card 0's change is accepted, as an
        // advisor card does once an Opus main default is.
        let mut rows = vec![
            VibeRow {
                host: 0,
                host_label: None,
                versions: vec![version(vec![], 0, vec![Origin::Vibe(0)])],
                shown: 0,
                checked: false,
                read_only: false,
            },
            VibeRow {
                host: 0,
                host_label: None,
                versions: vec![
                    version(vec![], 1, vec![Origin::Vibe(1)]),
                    version(vec![Origin::Vibe(0)], 2, vec![Origin::Walked(1, 0)]),
                ],
                shown: 0,
                checked: true,
                read_only: false,
            },
        ];
        resolve_vibes(&mut rows);
        let choose = |offer, yes| Event::Choose { offer, yes };
        assert_eq!(
            vibes_events_from(&rows, 1),
            [vec![choose(0, false), choose(1, true)]]
        );
        // Checking card 0 shows card 1's rebuilt version, unchecked: its check was for the
        // change it no longer shows.
        rows[0].checked = true;
        resolve_vibes(&mut rows);
        assert_eq!(rows[1].shown, 1);
        assert!(!rows[1].checked);
        assert_eq!(
            vibes_events_from(&rows, 1),
            [vec![choose(0, true), choose(2, false)]]
        );
        // Another host's acceptance moves nothing here.
        rows[1].host = 1;
        resolve_vibes(&mut rows);
        assert_eq!(rows[1].shown, 0);
    }

    #[test]
    fn two_cards_naming_one_edit_answer_it_once() {
        let rows: Vec<VibeRow> = [true, false]
            .into_iter()
            .map(|checked| VibeRow {
                host: 0,
                host_label: None,
                versions: vec![version(vec![], 3, vec![])],
                shown: 0,
                checked,
                read_only: false,
            })
            .collect();
        assert_eq!(
            vibes_events_from(&rows, 1),
            [vec![Event::Choose {
                offer: 3,
                yes: true
            }]]
        );
    }

    #[test]
    fn removals_and_blocks_wait_for_their_own_choice() {
        let mut app = App::new(&[], Hero::default(), Tab::Measured, false, false);
        app.host_count = 1;
        let fix = sample_fix(
            "MCP server gmail",
            "on -> off",
            5.0,
            false,
            Section {
                kind: Kind::Feature(ContextKind::McpServer, Reason::Unused),
                label: "Never-used MCP servers".into(),
                member: "gmail".into(),
                location: None,
            },
            "/synthetic/settings.json",
        );
        app.groups.push(Group {
            host_label: None,
            members: vec![MemberItem {
                host: 0,
                host_label: None,
                offer_index: 0,
                fix,
                entries_count: 2,
                denials_count: 1,
                checked: true,
                answered: false,
                manual: vec![(
                    "remove it from /etc/managed.json".into(),
                    "it is managed".into(),
                )],
                extras: false,
            }],
            split: false,
            minor: false,
        });
        // A removal left by hand prints once its change is written, as the walk prints it:
        // a write of `vibes`' changes alone leaves it out.
        let step = |app: &App, wrote: bool| {
            app.steps_by_hand(wrote)
                .iter()
                .any(|(_, _, step, _)| step == "remove it from /etc/managed.json")
        };
        assert!(step(&app, true));
        assert!(!step(&app, false));
        app.groups[0].members[0].checked = false;
        assert!(!step(&app, true));
        app.groups[0].members[0].checked = true;
        let frame = app.render(90, 40).join("\n");
        assert!(
            frame.contains("[ ] also remove 2 config entries and block 1"),
            "{frame}"
        );
        assert_eq!(
            app.measured_events(),
            [vec![Event::Choose {
                offer: 0,
                yes: true
            }]]
        );
        app.toggle_extras();
        assert_eq!(
            app.measured_events(),
            [vec![
                Event::Choose {
                    offer: 0,
                    yes: true
                },
                Event::Remove { offer: 0, entry: 0 },
                Event::Remove { offer: 0, entry: 1 },
                Event::Deny { offer: 0, name: 0 },
            ]]
        );
        // Unchecking the change drops its extras with it.
        app.toggle_focused();
        assert_eq!(
            app.measured_events(),
            [vec![Event::Choose {
                offer: 0,
                yes: false
            }]]
        );
    }

    #[test]
    fn terminal_controls_in_names_print_escaped() {
        let evil = "gmail\u{1b}]0;owned\u{7}\u{1b}[2J";
        let mut app = App::new(&[], Hero::default(), Tab::Measured, false, true);
        app.host_count = 1;
        let section = |member: &str| Section {
            kind: Kind::Feature(ContextKind::McpServer, Reason::Unused),
            label: "Never-used MCP servers".into(),
            member: member.into(),
            location: None,
        };
        let members = [evil, "drive"]
            .into_iter()
            .enumerate()
            .map(|(offer_index, name)| MemberItem {
                host: 0,
                host_label: None,
                offer_index,
                fix: clean_fix(
                    &sample_fix(name, "on -> off", 5.0, false, section(name), "/s"),
                    None,
                ),
                entries_count: 0,
                denials_count: 0,
                checked: false,
                answered: false,
                manual: vec![],
                extras: false,
            })
            .collect();
        app.groups.push(Group {
            host_label: None,
            members,
            split: false,
            minor: false,
        });
        app.expanded = Some(Focus::Group(0));
        let frame = app.render(90, 40).join("\n");
        assert!(frame.contains("gmail\\u001b]0;owned"), "{frame}");
        assert!(
            !frame.contains('\u{1b}') && !frame.contains('\u{7}'),
            "{frame}"
        );

        let mut by_hand = vec![];
        let steps = [(
            Some(evil.to_string()),
            evil.into(),
            evil.into(),
            evil.into(),
        )];
        write_by_hand(&mut by_hand, &steps, false).unwrap();
        let by_hand = String::from_utf8(by_hand).unwrap();
        assert!(
            !by_hand.contains('\u{1b}') && !by_hand.contains('\u{7}'),
            "{by_hand}"
        );
    }

    #[test]
    fn a_proof_taller_than_the_window_scrolls_and_keeps_the_keys() {
        let lines = |n: usize, tag: &str| (0..n).map(|i| format!("{tag}{i}")).collect();
        let blocks = vec![
            (false, lines(3, "a")),
            (true, lines(50, "p")),
            (false, lines(3, "b")),
        ];
        let (out, max) = window_blocks(&blocks, 10, 80, false, false, 0);
        assert!(out.len() <= 10, "{out:?}");
        assert_eq!(out[0], "p0");
        assert_eq!(max, 50 - 7);
        let (out, _) = window_blocks(&blocks, 10, 80, false, false, 1000);
        assert_eq!(out[out.len() - 2], "p49");
        assert!(out[out.len() - 1].contains("lines 44-50 of 50"), "{out:?}");
    }

    #[test]
    fn a_long_diff_line_wraps_whole() {
        let long = format!("{GREEN}+ \"{}\"{RESET} tail", "x".repeat(230));
        let rows = wrap_ansi(&long, 80);
        assert_eq!(rows.len(), 3);
        let plain = |s: &str| {
            let mut out = String::new();
            let mut chars = s.chars();
            while let Some(c) = chars.next() {
                if c == '\u{1b}' {
                    for n in chars.by_ref() {
                        if n.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        };
        assert!(rows.iter().all(|r| ansi_cells(r) <= 80), "{rows:?}");
        assert_eq!(
            rows.iter().map(|r| plain(r)).collect::<String>(),
            plain(&long)
        );
        // A row ending inside the style closes it, and the next reopens it.
        assert!(
            rows[0].ends_with(RESET) && rows[1].starts_with(GREEN),
            "{rows:?}"
        );
    }

    #[test]
    fn a_section_spans_hosts_as_the_walk_asks_it() {
        let dir = std::env::temp_dir().join(format!("token-max-tui-hosts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        std::fs::write(&settings, "{}\n").unwrap();
        let file = settings.to_string_lossy().into_owned();
        let fix = || {
            sample_fix(
                "Auto memory",
                "on -> off",
                0.6,
                false,
                Section {
                    kind: Kind::Setting,
                    label: "Auto memory".into(),
                    member: "Auto memory".into(),
                    location: None,
                },
                &file,
            )
        };
        let owners = [
            Owner::new(vec![fix()], vec![], None, 100.0),
            Owner::new(vec![fix()], vec![], None, 100.0),
        ];
        let style = Style {
            width: Some(80),
            color: false,
            unicode: false,
            home: None,
            host: None,
        };
        let hosts: Vec<Walking> = owners
            .iter()
            .zip([None, Some("devbox")])
            .map(|(owner, label)| Walking {
                label,
                offers: owner.offers(),
                style: &style,
            })
            .collect();
        let app = App::new(&hosts, Hero::default(), Tab::Measured, false, false);
        // $0.60 on each host is one $1.20 section, which neither folds nor reads as two.
        assert_eq!(app.groups.len(), 1);
        assert_eq!(app.groups[0].members.len(), 2);
        assert!(!app.groups[0].minor);
        assert_eq!(app.groups[0].host_label, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_scrolling_diff_keeps_its_keys_on_screen() {
        let mut app = App::new(&[], Hero::default(), Tab::Measured, false, false);
        app.screen = Screen::Diff;
        let text: Vec<String> = (0..40).map(|n| format!("+ line {n}")).collect();
        app.diff = Some(DiffPreview {
            text: text.join("\n"),
            file_count: 1,
            change_count: 1,
            backups: vec!["Backs up ~/.claude/settings.json first.".into()],
        });
        for scroll in [0, 1000] {
            app.diff_scroll.set(scroll);
            let out = app.render(80, 23);
            assert!(app.whole.get(), "{out:?}");
            assert!(
                out.last().unwrap().contains("[y] Write settings"),
                "{out:?}"
            );
            assert!(out.iter().any(|l| l.contains("of 40)")), "{out:?}");
        }
        assert!(app.render(80, 23).iter().any(|l| l == "+ line 39"));
    }

    #[test]
    fn both_savings_keep_their_margins_in_a_narrow_headline() {
        let dir = std::env::temp_dir().join(format!("token-max-tui-narrow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        std::fs::write(&settings, "{}\n").unwrap();
        let file = settings.to_string_lossy().into_owned();
        let section = |kind, member: &str| Section {
            kind,
            label: member.into(),
            member: member.into(),
            location: None,
        };
        let fixes = vec![
            Fix {
                margin: Some(crate::model::Margin::Usd(123.0)),
                ..sample_fix(
                    "Fast mode",
                    "on -> off",
                    1234.0,
                    true,
                    section(Kind::Setting, "Fast mode"),
                    &file,
                )
            },
            Fix {
                verb: UP_TO,
                margin: Some(crate::model::Margin::Usd(91.0)),
                ..sample_fix(
                    "Main-agent prompt cache TTL",
                    "unset -> 1h",
                    913.0,
                    false,
                    section(Kind::CacheTtl, "main"),
                    &file,
                )
            },
        ];
        let owner = Owner::new(fixes, vec![], None, 3000.0);
        let style = Style {
            width: Some(48),
            color: false,
            unicode: false,
            home: None,
            host: None,
        };
        let hosts = [Walking {
            label: None,
            offers: owner.offers(),
            style: &style,
        }];
        let hero = Hero {
            monthly_spend: 3000.0,
            sessions: 1234,
            requests: 123_456,
            spend_text: "$3,000".into(),
            credit: Some("Already saving ~$96/mo (16%) vs defaults".into()),
            caveats: vec![
                "A caveat long enough to wrap over several lines of a narrow pane.".into();
                4
            ],
            vibes_pending: 3,
            ..Hero::default()
        };
        let mut app = App::new(&hosts, hero, Tab::Measured, false, false);
        for member in app.groups.iter_mut().flat_map(|g| g.members.iter_mut()) {
            member.checked = true;
        }
        let out = app.render(48, 40);
        assert!(out.iter().all(|l| ansi_cells(l) <= 48), "{out:#?}");
        let text = out.join("\n");
        assert!(text.contains("Save ~$1,234/mo ± $123"), "{text}");
        assert!(
            text.contains("up to ~$913/mo ± $91 more by billing"),
            "{text}"
        );

        // At the smallest supported size the summary shrinks to its headline so the keys stay,
        // with a proof open and with the caveats, which scroll in the body.
        app.expanded = app.current_focus();
        for caveats in [false, true] {
            app.show_caveats = caveats;
            let out = app.render(48, 19);
            assert!(app.whole.get(), "{out:#?}");
            assert!(
                out.last().unwrap().contains("[Enter] Review diff"),
                "{out:#?}"
            );
            assert!(out.join("\n").contains("Save ~$1,234/mo"), "{out:#?}");
        }
        app.show_caveats = false;

        // Open caveats may hide every row, so a row key closes them and moves nothing; their
        // scroll stays theirs.
        app.show_caveats = true;
        let focus = app.focus;
        assert!(!app.caveats_take(KeyCode::PageDown));
        assert!(app.caveats_take(KeyCode::Down));
        assert!(!app.show_caveats && app.focus == focus);
        assert!(!app.caveats_take(KeyCode::Down));

        // Billing re-prices the TTL left unanswered, but keeps an answer the person gave.
        let ttl = app
            .groups
            .iter()
            .position(|g| g.members[0].fix.name.starts_with("Main-agent"))
            .unwrap();
        app.focus = app
            .visible_rows()
            .iter()
            .position(|f| *f == Focus::Group(ttl))
            .unwrap();
        app.toggle_focused();
        assert!(!app.groups[ttl].members[0].checked);
        for _ in 0..3 {
            app.cycle_billing();
            assert!(!app.groups[ttl].members[0].checked);
        }
        // `b` belongs to the Measured tab.
        app.tab = Tab::Vibes;
        let billing = app.billing;
        app.cycle_billing();
        assert_eq!(app.billing, billing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_group_writing_different_ttls_proves_each_member() {
        let app = App::new(&[], Hero::default(), Tab::Measured, false, false);
        let member = |name: &str, value: &str, five: f64, one: f64| {
            let section = Section {
                kind: Kind::CacheTtl,
                label: "Prompt cache TTL".into(),
                member: name.into(),
                location: None,
            };
            MemberItem {
                host: 0,
                host_label: None,
                offer_index: 0,
                fix: Fix {
                    proof: Some(Proof::CacheTtl {
                        five_m_usd: five,
                        one_h_usd: one,
                        requests: 100,
                    }),
                    ..sample_fix(name, value, (five - one).abs(), false, section, "/s")
                },
                entries_count: 0,
                denials_count: 0,
                checked: true,
                answered: false,
                manual: vec![],
                extras: false,
            }
        };
        let mut group = Group {
            host_label: None,
            members: vec![
                member("main", "5m -> 1h", 400.0, 100.0),
                member("subagents", "1h -> 5m", 50.0, 90.0),
            ],
            split: false,
            minor: false,
        };
        let charts = |group: &Group| {
            app.render_group_proof(group, 80)
                .iter()
                .filter(|l| l.contains("Replayed cache cost"))
                .count()
        };
        assert_eq!(charts(&group), 2);
        // Members that agree share one chart.
        group.members[1] = member("subagents", "5m -> 1h", 90.0, 50.0);
        assert_eq!(charts(&group), 1);
    }

    #[test]
    fn a_kept_row_names_its_profile_before_its_setting() {
        let mut app = App::new(&[], Hero::default(), Tab::Measured, false, false);
        app.hero.kept = vec![KeptItem {
            host_label: None,
            name: "Main-agent prompt cache TTL".into(),
            value: "1h".into(),
            saving_usd: Some(40.0),
            up_to: true,
            profile: Some("~/.claude-api-credits".into()),
            default_value: Some("5m".into()),
            summary: "Keeping 1h saves up to ~$40/mo.".into(),
        }];
        let row = app.render_kept_row(0, true, true, 40.0, 48).join("\n");
        // The title leads with the profile, cut only where the path itself is too long, and
        // the proof names it whole.
        assert!(row.contains("+ [~/.claude-api"), "{row}"); // slash ok: the profile label passes through unchanged
        assert!(row.contains("up to"), "{row}");
        assert!(row.contains("On ~/.claude-api-credits:"), "{row}"); // slash ok: the profile label passes through unchanged
        app.hero.kept[0].profile = Some("~/.claude-\u{1b}]0;x\u{7}".into()); // slash ok: a profile label
        let row = app.render_kept_row(0, true, true, 40.0, 200).join("\n");
        assert!(
            !row.contains('\u{7}') && !row.contains("\u{1b}]"),
            "{row:?}"
        );
        assert!(row.contains("On ~/.claude-\\u001b]0;x\\u0007:"), "{row:?}"); // slash ok: a profile label
    }

    #[test]
    fn a_card_left_by_hand_shows_its_step() {
        let mut app = App::new(&[], Hero::default(), Tab::Vibes, false, false);
        let card = VibeItem {
            how: Some(How::Manual {
                step: "run `token-max skill install`".into(),
                reason: "doctor and vibes write only settings files".into(),
            }),
            ..item("Token audit skill")
        };
        app.set_vibes(vec![VibeRow {
            host: 0,
            host_label: None,
            versions: vec![VibeVersion {
                after: vec![],
                item: card,
                offer: None,
                accepts: vec![],
            }],
            shown: 0,
            checked: false,
            read_only: false,
        }]);
        let drawer = app.render_vibe_row(0, true, true, 200).join(" ");
        assert!(
            drawer.contains("Change by hand, since doctor and vibes write only settings files: run `token-max skill install`"),
            "{drawer}"
        );
    }
}
