//! The terminal report: one verdict per setting by default, summarized evidence in details.

use std::fmt::Write as _;

use crate::collections::{IndexMap, IndexSet};

use crate::context::{
    CLAUDE_MEMORY, CODEX_MEMORY, SYNCED_SKILLS, control_text, incomplete_profiles, setting_rows,
    typed_only,
};
use crate::controls::CAPPED_SHARED;
use crate::keepalive::duration as keepalive_duration;
use crate::model::{
    Action, BackgroundCheck, BackgroundFeature, BashCapCheck, CacheReplayEvidence, CheckBase,
    CheckResult, ContextCost, ContextKind, ControlValue, Cost, Coverage, EstimateEvidence,
    Evidence, Harness, KeepaliveCheck, MemoryCheck, Millis, ModelSource, PeriodStatus, Profile,
    Report, RequestEvidence, Scalar, Share, SubagentCost, T3IdleCheck, T3TextGeneration, Ttl,
    UsageEstimate, Usd, Verdict,
};
use crate::prices::{TIER_SENSITIVITY, add_costs};
use crate::safe::{display, iso, parse_instant, terminal_text};
use crate::terminal::{WrapOptions, cells, wrap_text};

/// How to lay out the text report; presentation never changes the analysis.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReportOptions<'a> {
    /// Terminal width to wrap to; None keeps each line whole.
    pub width: Option<usize>,
    /// Style headings and verdicts with ANSI colors.
    pub color: bool,
    /// Use Unicode decorations such as arrows.
    pub unicode: bool,
    /// Show each check's evidence and aggregate repeated periods.
    pub details: bool,
    /// The host a report of several hosts describes; None for a report of this host alone.
    pub host: Option<Host<'a>>,
}

/// One host's section of a report covering several hosts.
#[derive(Clone, Copy, Debug)]
pub struct Host<'a> {
    /// The SSH destination, or "this host".
    pub name: &'a str,
    /// That host's home directory, shown as `~`.
    pub home: Option<&'a str>,
}

const DAY: f64 = 86_400_000.0;

/// JavaScript's `Number.prototype.toString`.
fn js_number(value: f64) -> String {
    match serde_json::Number::from_f64(value) {
        Some(n) => crate::exposure::number(&n),
        None if value.is_nan() => "NaN".into(),
        None if value > 0.0 => "Infinity".into(),
        None => "-Infinity".into(),
    }
}

/// JavaScript's `Math.round`: halves round up, toward positive infinity.
fn js_round(value: f64) -> f64 {
    let floor = value.floor();
    if value - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

/// JavaScript's `toFixed(2)` for a non-negative value, which rounds exact binary halves up.
fn to_fixed2(value: f64) -> String {
    if !value.is_finite() || value >= 1e21 {
        return js_number(value);
    }
    // A value whose exact binary expansion ends in a half cent is an odd multiple
    // of 1/8. Rust rounds those to even; JavaScript picks the larger.
    let eighths = value * 8.0;
    if eighths.fract() == 0.0 && eighths % 2.0 == 1.0 {
        let cents = (value * 100.0).floor() as u64 + 1;
        return format!("{}.{:02}", cents / 100, cents % 100);
    }
    format!("{value:.2}")
}

fn usd(value: f64) -> String {
    // Preserve the sign of small changes instead of rounding them to a free $0.00.
    if value != 0.0 && value.abs() < 0.01 {
        return if value < 0.0 { "-<$0.01" } else { "<$0.01" }.into();
    }
    format!(
        "{}${}",
        if value < 0.0 { "-" } else { "" },
        to_fixed2(value.abs())
    )
}

fn money(cost: &Cost) -> String {
    match cost.total_usd {
        None => format!("unknown ({} known subtotal)", usd(cost.known_usd.to_f64())),
        Some(total) => usd(total.to_f64()),
    }
}

fn instant(value: Option<Millis>) -> String {
    value.map_or_else(|| "unknown".into(), iso)
}

fn estimate(value: Option<f64>) -> String {
    value.map_or_else(|| "unknown".into(), usd)
}

/// A whole number grouped like `toLocaleString("en-US")`.
fn count(value: impl Into<i128>) -> String {
    let value = value.into();
    let digits = value.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if value < 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub(crate) fn about(value: f64) -> String {
    let rounded = if value.abs() >= 10.0 {
        js_round(value)
    } else {
        value
    };
    let text = usd(rounded);
    match text.strip_suffix(".00") {
        Some(whole) => whole.to_string(),
        None => text,
    }
}

fn days(since: Option<Millis>, until: Option<Millis>) -> String {
    let (Some(since), Some(until)) = (since, until) else {
        return "the window".into();
    };
    let n = js_round((until - since) as f64 / DAY).max(1.0);
    format!("{} day{}", js_number(n), if n == 1.0 { "" } else { "s" })
}

/// Share to one decimal, rounded down so a partial share never reads as 100%.
fn percent(part: u64, whole: u64) -> String {
    js_number(((part as f64 / whole as f64) * 1000.0).floor() / 10.0)
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A unit variant's name in kebab case, such as "not-applied".
fn label(value: &impl std::fmt::Debug) -> String {
    let mut name = String::new();
    for c in format!("{value:?}").chars() {
        if c.is_ascii_uppercase() && !name.is_empty() {
            name.push('-');
        }
        name.push(c.to_ascii_lowercase());
    }
    name
}

fn or_unknown(value: Option<impl ToString>) -> String {
    value.map_or_else(|| "unknown".into(), |v| v.to_string())
}

/// The first ten characters of an ISO timestamp: its date.
fn date(text: &str) -> &str {
    text.char_indices()
        .nth(10)
        .map_or(text, |(i, _)| &text[..i])
}

/// A check's current or recommended value.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Setting<'a> {
    Ttl(Ttl),
    Window(i64),
    Idle(bool),
    T3Idle(crate::model::T3IdleMode),
    Speed(&'a str),
    Toggle(bool),
    Cap(Millis),
    Keepalive(&'a str),
}

/// The fields every check shares, whatever its value type.
struct Common<'a> {
    check: &'static str,
    bucket: &'static str,
    profile: &'a str,
    history_profiles: &'a [String],
    evidence: Evidence,
    action: Action,
    current: Option<Setting<'a>>,
    recommended: Option<Setting<'a>>,
    owner: &'a str,
    since: Option<Millis>,
    until: Option<Millis>,
    window_basis: &'a str,
    baseline: &'a str,
    requests: u64,
    coverage: Option<Coverage>,
    savings_usd: Option<f64>,
    monthly_savings_usd: Option<f64>,
    break_even: Option<&'a str>,
    assumptions: &'a [String],
    uncertainty: &'a [String],
    sources: &'a [String],
    usage_estimates: &'a [UsageEstimate],
}

fn base<'a, V>(
    check: &'static str,
    bucket: &'static str,
    b: &'a CheckBase<V>,
    usage_estimates: &'a [UsageEstimate],
    setting: impl Fn(&'a V) -> Setting<'a>,
) -> Common<'a> {
    Common {
        check,
        bucket,
        profile: &b.profile,
        history_profiles: &b.history_profiles,
        evidence: b.evidence,
        action: b.action,
        current: b.current.as_ref().map(&setting),
        recommended: b.recommended.as_ref().map(&setting),
        owner: &b.owner,
        since: b.since,
        until: b.until,
        window_basis: &b.window_basis,
        baseline: &b.baseline,
        requests: b.requests,
        coverage: b.coverage,
        savings_usd: b.savings_usd,
        monthly_savings_usd: b.monthly_savings_usd,
        break_even: b.break_even.as_deref(),
        assumptions: &b.assumptions,
        uncertainty: &b.uncertainty,
        sources: &b.sources,
        usage_estimates,
    }
}

fn common(result: &CheckResult) -> Common<'_> {
    match result {
        CheckResult::Ttl(c) => base(
            "claude-cache-ttl",
            c.bucket.as_str(),
            &c.base,
            &c.usage_estimates,
            |t| Setting::Ttl(*t),
        ),
        CheckResult::Compaction(c) => base(
            "compaction-window",
            c.bucket,
            &c.base,
            &c.usage_estimates,
            |n| Setting::Window(*n),
        ),
        CheckResult::Idle(c) => base(
            "idle-compaction",
            c.bucket,
            &c.base,
            &c.usage_estimates,
            |b| Setting::Idle(*b),
        ),
        CheckResult::T3Idle(c) => base("t3-idle-compaction", "main", &c.base, &[], |mode| {
            Setting::T3Idle(*mode)
        }),
        CheckResult::Speed(c) => base(
            "fast-mode",
            c.bucket,
            &c.base,
            &c.usage_estimates,
            |s: &String| Setting::Speed(s),
        ),
        CheckResult::BashCap(c) => base("bash-timeout-cap", c.bucket, &c.base, &[], |n| {
            Setting::Cap(*n)
        }),
        CheckResult::Background(c) => base(
            "background-generation",
            feature_label(c.feature),
            &c.base,
            &[],
            |b| Setting::Toggle(*b),
        ),
        CheckResult::Keepalive(c) => base("keepalive", c.bucket, &c.base, &[], |s: &String| {
            Setting::Keepalive(s)
        }),
        CheckResult::Memory(c) => base("auto-memory", "memory", &c.base, &[], |b| {
            Setting::Toggle(*b)
        }),
    }
}

/// How to turn auto memory off: an environment variable as is, a setting in its file.
fn memory_off(c: &MemoryCheck, out: &Out<'_>) -> String {
    let set = match &c.off_file {
        Some(file) => format!("Set {} in {}", c.off, out.home(file)),
        None => format!("Set {}", c.off),
    };
    if c.overrides.is_empty() {
        return set;
    }
    // Settings that may override it have to be turned off where they set memories too.
    let files: Vec<String> = c.overrides.iter().map(|f| out.home(f)).collect();
    format!("{set}, and in {} where it sets memories", files.join(", "))
}

/// Whether Codex memories may have run in the window, generating notes in requests
/// that are recorded only in telemetry.
fn codex_unmeasured(c: &MemoryCheck) -> bool {
    c.unmeasured_generation
}

/// Why auto memory is not recommended, whatever it cost.
const MEMORY_ADVICE: &str = "Opinion: notes the agent writes for itself skip review and encourage slop, so keep durable guidance in reviewed, checked-in CLAUDE.md or AGENTS.md";

/// Why fast mode is recommended off, whatever it cost.
const FAST_ADVICE: &str =
    "Opinion: standard speed is enough, and fast mode bills a premium for the same work";

/// Why auto memory's state is unknown, as a sentence without its period.
fn memory_unresolved(c: &MemoryCheck) -> String {
    format!(
        "Unknown whether it is on: {}",
        c.unresolved.as_deref().unwrap_or_default()
    )
}

/// Whether auto memory cost nothing and was never used in the window.
fn memory_free(c: &MemoryCheck) -> bool {
    c.base.savings_usd == Some(0.0) && c.reads + c.writes == 0
}

/// Auto memory's reads and writes, and what writes cost when they could not be priced.
fn memory_uses(c: &MemoryCheck) -> String {
    let uses = c.reads + c.writes;
    if uses == 0 {
        return "never read back or written".into();
    }
    format!(
        "used {} time{} ({} read{}, {} write{})",
        count(uses),
        if uses == 1 { "" } else { "s" },
        count(c.reads),
        if c.reads == 1 { "" } else { "s" },
        count(c.writes),
        if c.writes == 1 { "" } else { "s" },
    )
}

fn feature_label(feature: BackgroundFeature) -> &'static str {
    match feature {
        BackgroundFeature::PromptSuggestions => "prompt-suggestions",
        BackgroundFeature::SessionRecaps => "session-recaps",
        BackgroundFeature::GoalCheckIns => "goal-check-ins",
        BackgroundFeature::AutoRecaps => "auto-recaps",
    }
}

/// How to turn a background feature off: an environment variable as is, a setting in its file.
fn background_off(c: &BackgroundCheck, out: &Out<'_>) -> String {
    match &c.off_file {
        Some(file) => format!("Set {} in {}", c.off, out.home(file)),
        None => format!("Set {}", c.off),
    }
}

/// A duration in the largest whole unit, such as `55m`.
fn duration(ms: Millis) -> String {
    for (unit, size) in [("h", 3_600_000), ("m", 60_000), ("s", 1000)] {
        if ms % size == 0 {
            return format!("{}{unit}", ms / size);
        }
    }
    format!("{ms}ms")
}

/// A value as the evidence cards print it.
fn setting(value: Option<Setting<'_>>) -> String {
    match value {
        None => "unknown".into(),
        Some(Setting::Ttl(t)) => t.as_str().into(),
        Some(Setting::Window(n)) => format!("{n} tokens"),
        Some(Setting::Idle(b)) => b.to_string(),
        Some(Setting::T3Idle(mode)) => mode.as_str().into(),
        Some(Setting::Speed(s)) => s.into(),
        Some(Setting::Toggle(b)) => if b { "on" } else { "off" }.into(),
        Some(Setting::Cap(ms)) => format!("{ms} ms"),
        Some(Setting::Keepalive(s)) => s.into(),
    }
}

/// A value as the summary prints it.
fn setting_value(value: Option<Setting<'_>>) -> String {
    match value {
        None => "unknown".into(),
        Some(Setting::T3Idle(mode)) => mode.as_str().into(),
        Some(Setting::Idle(b) | Setting::Toggle(b)) => if b { "on" } else { "off" }.into(),
        Some(Setting::Speed("standard")) => "off".into(),
        Some(Setting::Speed(s)) => s.into(),
        Some(Setting::Window(n)) => format!("{} tokens", count(n)),
        Some(Setting::Ttl(t)) => t.as_str().into(),
        Some(Setting::Cap(ms)) => duration(ms),
        Some(Setting::Keepalive(s)) => s.into(),
    }
}

/// The observed token range, retaining unknown when no size is known.
fn token_range(mut values: impl Iterator<Item = i64>) -> String {
    match values.next() {
        Some(first) => {
            let (min, max) = values.fold((first, first), |(min, max), n| (min.min(n), max.max(n)));
            format!("{min} to {max} tokens")
        }
        None => "unknown token range".into(),
    }
}

/// An unstyled line.
const PLAIN: &str = "";
/// A heading's ANSI style.
const BOLD: &str = "1";

/// A verdict's ANSI color, which only repeats what the line's words say: red for
/// a recommended change, yellow for an advisory, green for a value already right.
fn tier(action: Action) -> &'static str {
    match action {
        Action::Change => "31",
        Action::Advisory => "33",
        Action::Keep => "32",
    }
}

/// A feature row's color: its own verdict, red when its server's off covers it, and
/// yellow when it can be switched off today but has no verdict. Other rows have no verdict.
fn feature_tier(c: &ContextCost, covered: bool) -> &'static str {
    match c.verdict {
        Some(Verdict::Off) => tier(Action::Change),
        _ if covered => tier(Action::Change),
        Some(Verdict::Consider) => tier(Action::Advisory),
        Some(Verdict::Keep) => tier(Action::Keep),
        None if c.current && c.control.is_some() => tier(Action::Advisory),
        None => PLAIN,
    }
}

/// `path` shown from `~` when it is under `home`.
pub(crate) fn home_path(home: Option<&str>, path: &str) -> String {
    match home {
        Some(dir)
            // A Windows home, local or remote, also separates with backslashes.
            if path == dir
                || path.strip_prefix(dir).is_some_and(|rest| {
                    rest.starts_with('/') || (dir.contains('\\') && rest.starts_with('\\'))
                }) =>
        {
            format!("~{}", &path[dir.len()..])
        }
        _ => path.to_string(),
    }
}

/// The text being built, one wrapped line at a time.
struct Out<'a> {
    text: String,
    options: ReportOptions<'a>,
    arrow: &'static str,
    home: Option<String>,
    profiles: IndexMap<&'a str, &'a Profile>,
    /// Instances removed from T3's settings, by the profile key their history keeps.
    removed: crate::collections::HashMap<String, &'a str>,
}

impl<'a> Out<'a> {
    fn new(report: &'a Report, options: ReportOptions<'a>) -> Self {
        let mut profiles = IndexMap::default();
        for p in &report.scan.profiles {
            profiles.insert(p.id.as_str(), p);
        }
        Out {
            text: String::new(),
            options,
            arrow: if options.unicode { "→" } else { "->" },
            home: match options.host {
                Some(host) => host.home.map(str::to_string),
                None => std::env::home_dir().map(|dir| dir.to_string_lossy().into_owned()),
            },
            profiles,
            removed: report
                .scan
                .t3
                .iter()
                .flat_map(|s| {
                    s.removed
                        .keys()
                        .map(|i| (crate::t3::profile_key(&s.path, i), i.as_str()))
                })
                .collect(),
        }
    }
    fn line(&mut self, text: &str, indent: usize, style: &str) {
        self.wrap(text, indent, None, style);
    }
    /// A line whose wrapped rows hang at `continuation` cells.
    fn hang(&mut self, text: &str, continuation: usize, style: &str) {
        self.wrap(text, 0, Some(continuation), style);
    }
    fn wrap(&mut self, text: &str, indent: usize, continuation: Option<usize>, style: &str) {
        // Escape metadata before wrapping or adding our own ANSI. Long paths are
        // hard-wrapped, never shortened; wide characters use terminal cell widths.
        let wrapped = wrap_text(
            &terminal_text(text),
            WrapOptions {
                width: self.options.width,
                indent,
                continuation,
            },
        );
        for part in wrapped.split('\n') {
            if self.options.color && !style.is_empty() {
                let _ = write!(self.text, "\x1b[{style}m{part}\x1b[0m");
            } else {
                self.text.push_str(part);
            }
            self.text.push('\n');
        }
    }
    fn blank(&mut self) {
        self.line("", 0, PLAIN);
    }
    /// What each color means, in its own color; only printed when color is on.
    fn legend(&mut self) {
        let parts = [
            (Mark::Change, "Red: change."),
            (Mark::Consider, "Yellow: consider."),
            (Mark::Keep, "Green: keep."),
            (Mark::Unknown, "Dim: no verdict."),
        ];
        // Wrap between parts, so each keeps its own color on a narrow terminal.
        let mut used = 0;
        for (mark, text) in parts {
            let cells = cells(text);
            if used > 0 {
                if self.options.width.is_some_and(|w| used + 1 + cells > w) {
                    self.text.push('\n');
                    used = 0;
                } else {
                    self.text.push(' ');
                    used += 1;
                }
            }
            let _ = write!(self.text, "\x1b[{}m{text}\x1b[0m", mark.style());
            used += cells;
        }
        self.text.push('\n');
    }
    /// A line kept whole at any width, such as a JSON snippet.
    fn raw(&mut self, text: &str) {
        self.text.push_str(&terminal_text(text));
        self.text.push('\n');
    }
    fn section(&mut self, title: &str) {
        self.blank();
        self.line(title, 0, BOLD);
    }
    fn field(&mut self, name: &str, value: &str) {
        self.line(&format!("{name}: {value}"), 2, PLAIN);
    }
    fn notes(&mut self, name: &str, values: &[String]) {
        for value in values.iter().collect::<IndexSet<_>>() {
            self.field(name, value);
        }
    }
    fn cost(&mut self, name: &str, value: &Cost) {
        self.field(name, &money(value));
        self.notes("Unknown", &value.unknown);
        self.notes("Assumption", &value.assumptions);
    }
    fn witnesses(&mut self, values: &[CacheReplayEvidence]) {
        let mut groups = IndexMap::default();
        let mut predecessors = IndexMap::default();
        let mut unrecorded = 0;
        for e in values {
            let group = groups
                .entry((e.ttl, e.state, e.outcome, e.reason))
                .or_insert((0usize, e));
            group.0 += 1;
            unrecorded += usize::from(e.predecessors.is_empty());
            for p in &e.predecessors {
                let group = predecessors
                    .entry((p.state, p.prefix_evidence, p.cohort_known, p.reason))
                    .or_insert((0usize, p));
                group.0 += 1;
            }
        }
        for (count, e) in groups.into_values() {
            self.field(
                "Cache evidence",
                &format!(
                    "{count} parts; {}: {}, {}; {}; example {}/{}",
                    e.ttl.as_str(),
                    label(&e.state),
                    label(&e.outcome),
                    e.reason,
                    e.request,
                    e.part
                ),
            );
        }
        for (count, p) in predecessors.into_values() {
            self.field(
                "Predecessor",
                &format!(
                    "{count} witnesses; {}, prefix {}, cohort {}; {}; example {}/{} at {}",
                    label(&p.state),
                    label(&p.prefix_evidence),
                    if p.cohort_known {
                        "known"
                    } else {
                        "incomplete"
                    },
                    p.reason,
                    p.request,
                    p.part,
                    instant(Some(p.time))
                ),
            );
        }
        if unrecorded > 0 {
            self.field("Predecessor", &format!("unrecorded for {unrecorded} parts"));
        }
    }
    fn title(&mut self) {
        let title = match self.options.host {
            Some(host) => format!("token-max: {}", host.name),
            None => "token-max".into(),
        };
        self.line(&title, 0, BOLD);
    }
    fn home(&self, path: &str) -> String {
        home_path(self.home.as_deref(), path)
    }
    /// `text` with every path under the home directory shown from `~`.
    fn home_in(&self, text: &str) -> String {
        match &self.home {
            Some(dir) => text.replace(&format!("{dir}/"), "~/"),
            None => text.to_string(),
        }
    }
    /// A profile by harness and quoted root, for evidence cards.
    fn profile_name(&self, id: &str) -> String {
        match self.profiles.get(id) {
            Some(p) => match &p.t3 {
                Some(t) => format!(
                    "T3 {} {} ({})",
                    t.instance,
                    label(&p.harness),
                    display(&p.root)
                ),
                None => format!("{} {}", label(&p.harness), display(&p.root)),
            },
            None => self.removed_name(id).unwrap_or_else(|| display(id)),
        }
    }
    /// A profile by harness and home-relative root, for the summary.
    fn short_profile(&self, id: &str) -> String {
        match self.profiles.get(id) {
            Some(p) => match &p.t3 {
                Some(t) => format!(
                    "T3 {} {} ({})",
                    t.instance,
                    label(&p.harness),
                    self.home(&p.root)
                ),
                None => format!("{} {}", label(&p.harness), self.home(&p.root)),
            },
            None => self.removed_name(id).unwrap_or_else(|| self.home(id)),
        }
    }
    fn removed_name(&self, id: &str) -> Option<String> {
        self.removed
            .get(id)
            .map(|instance| format!("T3 {instance} (removed from T3 settings)"))
    }
}

/// Render all findings in a wrapping flow, with grouped evidence in details.
pub fn format_report<'a>(report: &'a Report, options: ReportOptions<'a>) -> String {
    let mut out = Out::new(report, options);
    if options.details {
        details(report, &mut out);
    } else {
        summary(report, &mut out);
    }
    out.text
}

/// The changes the report recommends or suggests, each with its settings edit or the
/// step to take by hand, for output that `token-max doctor` does not walk through.
pub fn format_changes<'a>(report: &'a Report, options: ReportOptions<'a>) -> String {
    let mut out = Out::new(report, options);
    let fixes = crate::doctor::fixes(report, options.unicode);
    if fixes.is_empty() {
        return out.text;
    }
    out.blank();
    out.line(
        "Changes to make; on a terminal, token-max doctor offers to make each one:",
        0,
        BOLD,
    );
    for fix in &fixes {
        let lines = crate::doctor::describe(fix, out.home.as_deref());
        out.hang(&format!("  {}", lines.head), 4, PLAIN);
        for (text, raw) in lines.body {
            if raw {
                out.raw(&format!("    {text}"));
            } else {
                out.line(&text, 4, PLAIN);
            }
        }
    }
    out.text
}

fn generated_at(report: &Report) -> Millis {
    parse_instant(&report.generated_at).expect("reports record an ISO generation time")
}

/// One line per T3 state for its text-generation model, whose cost no native record holds.
fn t3_text_generation(report: &Report, out: &Out<'_>) -> Vec<String> {
    let text = |g: &T3TextGeneration| {
        let mut line = format!(
            "T3 text generation: {} {} ({}). Cost unmeasured: T3 writes thread titles, branch names, PR text and other generated text in ephemeral sessions that keep no native records. Set in {}.",
            g.selection.instance,
            g.selection.model,
            g.resolution,
            out.home(&g.owner)
        );
        for (key, s) in &g.overrides {
            let _ = write!(line, " {key} replaces it with {} {}.", s.instance, s.model);
        }
        if g.project_overrides > 0 {
            let _ = write!(
                line,
                " {} project{} replace these selections in projectSettingsOverrides.",
                count(g.project_overrides),
                if g.project_overrides == 1 { "" } else { "s" }
            );
        }
        line
    };
    report
        .scan
        .t3
        .iter()
        .filter_map(|s| s.text_generation.as_ref())
        .map(text)
        .collect()
}

fn t3_coverage(report: &Report, out: &mut Out<'_>) {
    for state in report.scan.t3.iter().filter(|s| s.status == "selected") {
        out.field(
            "T3 state",
            &format!("{}; {}", display(&state.path), state.metadata),
        );
        for exclusion in &state.exclusions {
            out.field("T3 coverage", exclusion);
        }
        for (cause, n) in &state.unresolved {
            out.field(
                "T3 unresolved",
                &format!(
                    "{} request{}: {cause}",
                    count(*n),
                    if *n == 1 { "" } else { "s" }
                ),
            );
        }
        for (instance, n) in &state.removed {
            out.field(
                "T3 removed",
                &format!(
                    "{} request{} of instance {instance}, no longer in T3's settings: billed, left out of every current profile's checks",
                    count(*n),
                    if *n == 1 { "" } else { "s" }
                ),
            );
        }
    }
    if out.options.details {
        for line in t3_text_generation(report, out) {
            out.line(&line, 2, PLAIN);
        }
    }
}

fn details(report: &Report, out: &mut Out<'_>) {
    let arrow = out.arrow;
    out.title();
    t3_coverage(report, out);
    out.line(
        "Public API-price estimates, not subscription bills.",
        0,
        PLAIN,
    );
    let requests = &report.scan.requests;
    let earliest = requests.first().map(|r| r.time);
    out.field(
        "Window",
        &format!(
            "{}; {} {arrow} {}",
            if report.scan.since.is_none() {
                "available history"
            } else {
                "explicit --since"
            },
            instant(report.scan.since.or(earliest)),
            report.generated_at
        ),
    );
    out.field(
        "Profiles",
        &format!("{} selected; settings remain read-only", out.profiles.len()),
    );
    let profiles: Vec<&Profile> = out.profiles.values().copied().collect();
    for p in profiles {
        out.field("Profile", &out.profile_name(&p.id));
        out.field("Settings", &display(&p.settings_file));
        for alias in &p.aliases {
            out.field("Alias", &display(alias));
        }
        for history in &p.histories {
            out.field("History", &display(history));
        }
    }
    let coverage = &report.scan.coverage;
    let measured = requests
        .iter()
        .filter(|r| r.evidence == RequestEvidence::Measured)
        .count();
    let skipped = if coverage.skipped_lines > 0 {
        format!(
            "; {} unreadable transcript lines in {} files skipped (truncated or interleaved writes)",
            coverage.skipped_lines, coverage.skipped_files
        )
    } else {
        String::new()
    };
    out.field(
        "Coverage",
        &format!(
            "{} selected requests in {} files; {measured} with measured usage; {} missing usage{skipped}",
            coverage.requests,
            report.scan.snapshots.len(),
            coverage.missing_usage
        ),
    );
    out.field(
        "Prices",
        &format!(
            "{}; {}; catalog timestamp {}",
            if report.prices.offline {
                "offline/local"
            } else {
                "online"
            },
            display(&report.prices.source),
            report.prices.fetched_at.as_deref().unwrap_or("unknown")
        ),
    );

    // Features recommended off are changes too, and also cause exit 1.
    let changes = report
        .checks
        .iter()
        .filter(|c| c.action() == Action::Change)
        .count()
        + setting_rows(&report.context)
            .filter(|c| c.verdict == Some(Verdict::Off))
            .count();
    out.section(&if changes > 0 {
        format!(
            "{changes} recommended change{}",
            if changes == 1 { "" } else { "s" }
        )
    } else {
        "No applicable setting changes".into()
    });
    if report.scan.snapshots.is_empty() {
        out.line("No transcript histories found. Select an existing profile with --claude-dir or --codex-dir; run after using that agent.", 0, PLAIN);
    } else if coverage.requests == 0 {
        out.line(
            "No selected requests. Try an earlier --since or select another profile.",
            0,
            PLAIN,
        );
    } else if report.checks.is_empty()
        && !setting_rows(&report.context).any(|c| c.verdict.is_some())
    {
        out.line("No applicable checks for the selected requests. Usage coverage does not establish that every setting is optimal.", 0, PLAIN);
    } else if changes == 0 {
        out.line(
            if report.checks.iter().any(|c| c.action() == Action::Advisory) {
                "Review advisory findings for unresolved settings or missing evidence."
            } else {
                "Available checks favor current settings. Coverage limits still apply."
            },
            0,
            PLAIN,
        );
    }
    if !report.checks.is_empty() {
        out.line(
            "Each check has its own baseline. Do not sum independent savings.",
            0,
            PLAIN,
        );
    }

    let groups = [
        (Action::Change, "Recommended changes"),
        (Action::Advisory, "Advisory findings"),
        (Action::Keep, "Keep current settings"),
    ];
    for (action, title) in groups {
        let results: Vec<&CheckResult> = report
            .checks
            .iter()
            .filter(|c| c.action() == action)
            .collect();
        if results.is_empty() {
            continue;
        }
        out.section(title);
        for result in results {
            card(result, action, out);
        }
    }

    context_section(report, out);

    if !report.subagents.is_empty() {
        out.section("Subagents");
        out.line(SUBAGENT_EVIDENCE, 0, PLAIN);
        for c in &report.subagents {
            let names: Vec<String> = c.profiles.iter().map(|p| out.profile_name(p)).collect();
            out.blank();
            out.line(
                &format!("{} [{}]", subagent_name(c), names.join(", ")),
                0,
                BOLD,
            );
            out.field(
                "Runs",
                &format!(
                    "{} runs; {} requests, {} priced",
                    count(c.runs),
                    count(c.requests),
                    count(c.priced)
                ),
            );
            out.cost("Cost", &c.cost);
            out.field("Models", &shares(&c.models, c.requests));
            out.field("Efforts", &shares(&c.efforts, c.requests));
            for s in &c.sources {
                out.field(
                    "Model source",
                    &format!(
                        "{}: {} ({})",
                        label(&s.source),
                        out.home_in(&s.detail),
                        runs(s.runs)
                    ),
                );
            }
            match (&c.cheapest, &c.reprice) {
                (Some(model), Some(cost)) => {
                    out.cost(&format!("Same tokens on {model}"), cost);
                }
                _ => out.field("Cheapest model", &reprice_text(c, &usd)),
            }
            out.field("Model setting", &out.home_in(&c.model_setting));
            out.field("Effort setting", &out.home_in(&c.effort_setting));
        }
    }

    out.section("Recorded usage and coverage");
    out.field(
        "Attribution",
        &format!(
            "source/trigger {}/{}; service tier {}/{}",
            coverage.source_known, coverage.requests, coverage.tier_known, coverage.requests
        ),
    );
    out.field(
        "Snapshot",
        &format!(
            "{} {arrow} {}; {} files with unterminated final lines deferred",
            report.scan.snapshot_started_at,
            report.scan.snapshot_finished_at,
            coverage.deferred_files
        ),
    );
    out.notes("Coverage limit", &coverage.issues);
    for root in &report.scan.roots {
        out.field(
            "Root",
            &format!(
                "{} {} {} ({})",
                label(&root.status),
                label(&root.harness),
                display(&root.path),
                label(&root.origin)
            ),
        );
    }
    for group in &report.historical {
        let names: Vec<String> = group
            .profiles
            .iter()
            .map(|id| out.profile_name(id))
            .collect();
        out.field(
            "Estimated recorded-request cost",
            &format!(
                "{}; {} requests; {}{}",
                names.join(", "),
                group.requests,
                money(&group.cost),
                if group.profiles.len() > 1 {
                    "; shared, profile attribution unknown"
                } else {
                    ""
                }
            ),
        );
        out.notes("Unknown", &group.cost.unknown);
        out.notes("Pricing assumption", &group.cost.assumptions);
        if !group.tier_sensitivity.is_empty() {
            out.field(
                "Tier coverage",
                &format!(
                    "{} of {} requests priced; {} use conditional service tiers",
                    group.priced_requests, group.requests, group.conditional_requests
                ),
            );
            for scenario in &group.tier_sensitivity {
                out.field(
                    "Served-tier scenario",
                    &format!("{}: {}", scenario.tier, money(&scenario.cost)),
                );
                out.notes("Unknown", &scenario.cost.unknown);
            }
            out.line("Tier scenarios replace only conditionally priced tiers, preserve recorded billing, and are not bounds on actual charges.", 0, PLAIN);
        }
    }
    if report.historical.is_empty() {
        out.line(
            "Recorded-request cost: unknown; no selected request evidence.",
            0,
            PLAIN,
        );
    }
    let exposure = &report.scan.exposure;
    out.field(
        "Component coverage",
        &format!(
            "{} revisions; {}/{} requests with wire exposure; {} invocation observations; {} calibrated models. Requests without a recorded shape are not priced per component.",
            exposure.components.len(),
            exposure
                .requests
                .iter()
                .filter(|r| r.view.linkage == crate::model::Linkage::Wire)
                .count(),
            exposure.requests.len(),
            exposure.invocations.len(),
            report.exposure.calibration.len()
        ),
    );
    if !report.compactions.is_empty() {
        out.field(
            "Compactions",
            &format!(
                "{} linked usage; {} summary proxies; {} unknown costs",
                report
                    .compactions
                    .iter()
                    .filter(|c| c.request.is_some())
                    .count(),
                report
                    .compactions
                    .iter()
                    .filter(|c| c.evidence == Evidence::Estimated)
                    .count(),
                report
                    .compactions
                    .iter()
                    .filter(|c| c.evidence == Evidence::Unknown)
                    .count()
            ),
        );
    }
    for charge in &report.compactions {
        out.field(
            "Compaction",
            &format!(
                "{}; request {}; {}",
                charge.event,
                charge.request.as_deref().unwrap_or("unlinked"),
                label(&charge.evidence)
            ),
        );
        out.cost("Summary cost", &charge.cost);
        out.notes("Summary assumption", &charge.assumptions);
    }
    out.notes("Report assumption", &report.assumptions);
    out.notes("Exposure assumption", &report.exposure.assumptions);
    out.blank();
    out.line(
        "Repeated evidence is grouped; cache and attempt identifiers are examples.",
        0,
        PLAIN,
    );
}

/// Why the subagent lines never recommend a change.
const SUBAGENT_EVIDENCE: &str = "Subagent lines are evidence, not recommendations: a cheaper model or lower effort trades capability, and the main agent's extra work after a weaker subagent cannot be attributed, so no saving is claimed. A lower effort changes the output tokens, so its cost cannot be measured.";

/// A subagent type by harness: a Claude agent type, a Codex role or other Codex subagent.
fn subagent_name(c: &SubagentCost) -> String {
    match (c.harness, c.kind.as_str()) {
        (_, crate::reader::UNRECORDED) => {
            format!("{} subagents of unrecorded type", label(&c.harness))
        }
        (Harness::Claude, kind) => format!("{kind} subagent"),
        (Harness::Codex, kind) if c.native => format!("Codex {kind} subagent"),
        (Harness::Codex, kind) => format!("{kind} agent role"),
    }
}

/// A count of subagent runs.
fn runs(n: u64) -> String {
    format!("{} run{}", count(n), if n == 1 { "" } else { "s" })
}

/// Values with their share of `whole`, or the value alone when there is only one.
fn shares(values: &[Share], whole: u64) -> String {
    match values {
        [one] => one.name.clone(),
        _ => values
            .iter()
            .map(|s| format!("{} ({}%)", s.name, percent(s.count, whole)))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// A cost scaled by `scale`, a known lower bound, or unknown.
fn scaled(cost: &Cost, scale: &dyn Fn(f64) -> String) -> String {
    match cost.total_usd {
        Some(value) => scale(value.to_f64()),
        None if cost.known_usd > Usd::ZERO => {
            format!("at least {}", scale(cost.known_usd.to_f64()))
        }
        None => "an unknown amount".into(),
    }
}

/// What the same tokens would cost on the provider's cheapest current model.
fn reprice_text(c: &SubagentCost, scale: &dyn Fn(f64) -> String) -> String {
    match (&c.cheapest, &c.reprice) {
        (None, _) => "The price catalog has no cheapest current model for its provider.".into(),
        (Some(model), None) => {
            format!("It already ran on {model}, its provider's cheapest current model.")
        }
        (Some(model), Some(cost)) => format!(
            "On {model} the same recorded tokens would cost {}, assuming the same turns.",
            scaled(cost, scale)
        ),
    }
}

/// One subagent type's evidence: its runs, models, efforts and what a cheaper model would cost.
fn subagent_line(c: &SubagentCost, scale: &dyn Fn(f64) -> String, out: &Out<'_>) -> String {
    let coverage = if c.priced < c.requests {
        format!(
            " ({} of {} requests priced)",
            count(c.priced),
            count(c.requests)
        )
    } else {
        String::new()
    };
    let effort = if c
        .efforts
        .iter()
        .all(|e| e.name == crate::reader::UNRECORDED)
    {
        "an unrecorded effort".to_string()
    } else {
        format!("{} effort", shares(&c.efforts, c.requests))
    };
    let sources: Vec<String> = c
        .sources
        .iter()
        .map(|s| {
            let detail = out.home_in(&s.detail);
            let detail = if s.source == ModelSource::Unknown {
                format!("unknown, {detail}")
            } else {
                detail
            };
            if c.sources.len() == 1 {
                detail
            } else {
                format!("{detail} ({})", runs(s.runs))
            }
        })
        .collect();
    format!(
        "{}{coverage} on {} at {effort}. Model source: {}. {} Model setting: {}. Effort setting: {}.",
        capitalize(&runs(c.runs)),
        shares(&c.models, c.requests),
        sources.join("; "),
        reprice_text(c, scale),
        out.home_in(&c.model_setting),
        out.home_in(&c.effort_setting),
    )
}

/// One check's full evidence card.
fn card(result: &CheckResult, action: Action, out: &mut Out<'_>) {
    let arrow = out.arrow;
    let r = common(result);
    out.blank();
    out.line(
        &format!(
            "{}  {} / {}  [{}]",
            label(&action).to_uppercase(),
            r.check,
            r.bucket,
            label(&r.evidence)
        ),
        0,
        tier(action),
    );
    out.field("Claim", Claim::of(result).name());
    out.field("Profile", &out.profile_name(r.profile));
    out.field("Owner", &display(r.owner));
    out.field("Current", &setting(r.current));
    out.field("Suggested", &setting(r.recommended));
    out.field("Reason", &reason(result, &r));
    out.field("Baseline", r.baseline);
    out.field(
        "Window",
        &format!(
            "{} {arrow} {}; {} requests; {}",
            instant(r.since),
            instant(r.until),
            r.requests,
            r.window_basis
        ),
    );
    out.field(
        match r.check {
            "fast-mode" => "Estimated rate-only saving vs current speed",
            // What it cost, which turning it off saves only while it is still on.
            "auto-memory" => "Total cost",
            _ => "Estimated net saving vs baseline",
        },
        &format!(
            "{} over window; {} / 30 days",
            estimate(r.savings_usd),
            estimate(r.monthly_savings_usd)
        ),
    );
    match result {
        CheckResult::Memory(m) => {
            out.field("Turn off", &memory_off(m, out));
            out.field("Loaded memory and reads", &estimate(m.context_usd));
            out.field(
                "Writes, estimated at the output price",
                &estimate(m.writes_usd),
            );
            out.field("Uses", &memory_uses(m));
        }
        CheckResult::Background(b) => {
            out.field("Ownership", &b.reason);
            for cwd in &b.directories {
                out.field(
                    "Recorded directory",
                    &cwd.as_ref()
                        .map_or_else(|| "absent".into(), |d| out.home(d)),
                );
            }
            out.field("Turn off", &background_off(b, out));
            out.field(
                "Estimated requests",
                &b.generations
                    .map_or_else(|| "unknown".into(), |n| n.to_string()),
            );
            out.cost("Estimated request cost", &b.cost);
            out.cost("Later input costs its reads avoided", &b.lost_warming);
        }
        CheckResult::Ttl(t) => {
            out.field(
                "Replay cost",
                &format!(
                    "5m {}; 1h {}",
                    money(&t.cost.five_minutes),
                    money(&t.cost.one_hour)
                ),
            );
            out.field(
                "Cache comparison",
                &format!(
                    "5m {}; 1h {}",
                    money(&t.comparison_cost.five_minutes),
                    money(&t.comparison_cost.one_hour)
                ),
            );
            if t.fixed_cost.total_usd.is_none() {
                out.field(
                    "Fixed costs",
                    "Identical fixed costs remain unknown and cancel in the cache comparison.",
                );
            }
        }
        CheckResult::T3Idle(i) => {
            for comparison in [&i.current_settings, &i.after_recommendations] {
                out.field("T3 baseline", &comparison.baseline);
                for scenario in &comparison.scenarios {
                    out.cost(scenario.mode.as_str(), &scenario.cost);
                    out.field(
                        "Summaries",
                        &format!(
                            "{} generated, {} with no observed return",
                            scenario.summaries, scenario.final_summaries
                        ),
                    );
                }
            }
        }
        CheckResult::Idle(i) => {
            out.field(
                "Effective idle threshold",
                &i.threshold
                    .map_or_else(|| "unknown".into(), |t| format!("{t} tokens")),
            );
            for scenario in [&i.current_settings, &i.after_recommendations] {
                out.field(
                    "Idle comparison",
                    &format!(
                        "{}; {}; on {}, off {}",
                        scenario.baseline,
                        if scenario.applicable {
                            "applicable"
                        } else {
                            "inapplicable"
                        },
                        money(&scenario.cost.on),
                        money(&scenario.cost.off)
                    ),
                );
                out.field(
                    "On minus off",
                    &format!(
                        "{} over window; {} / 30 days",
                        estimate(scenario.delta_usd),
                        estimate(scenario.monthly_delta_usd)
                    ),
                );
                let periods = &scenario.periods;
                let with = |f: &dyn Fn(&crate::model::IdlePeriod) -> bool| {
                    periods.iter().filter(|p| f(p)).count()
                };
                out.field(
                    "Idle periods",
                    &format!(
                        "{} request/final periods; {} eligible, {} confirmed; {} censored; {} final periods without observed return",
                        periods.len(),
                        with(&|p| p.status == PeriodStatus::Eligible),
                        with(&|p| p.confirmed),
                        with(&|p| p.status == PeriodStatus::Censored),
                        with(&|p| p.resume.is_none())
                    ),
                );
            }
            let s = &i.after_recommendations.sensitivity;
            out.field(
                "Idle sensitivity, on minus off",
                &format!(
                    "closed {}; open/awake {}; larger summary/full prefix {}; late trigger {}. The aggregate estimate decides.",
                    estimate(s.closed_delta_usd),
                    estimate(s.open_delta_usd),
                    estimate(s.larger_summary_delta_usd),
                    estimate(s.late_trigger_delta_usd)
                ),
            );
            out.field(
                "Extra re-read break-even per resumed summary",
                &estimate(s.reread_break_even_usd_per_resumed_summary),
            );
        }
        CheckResult::BashCap(b) => {
            out.field(
                "Compared TTL",
                &format!("{}; {}", b.ttl.map_or("unknown", Ttl::as_str), b.ttl_basis),
            );
            out.field("Replacement", &b.replacement);
            let w = &b.waits;
            out.field(
                "Foreground Bash waits",
                &format!(
                    "{} calls; {} reached the cache TTL ({} timed out, {} polling loops)",
                    w.calls, w.long, w.timed_out, w.polling
                ),
            );
        }
        CheckResult::Keepalive(k) => {
            out.field(
                "Cache lifetime",
                &match (k.lifetime_ms, k.interval_ms) {
                    (Some(l), Some(i)) => {
                        format!(
                            "{}; pings every {}",
                            keepalive_duration(l),
                            keepalive_duration(i)
                        )
                    }
                    _ => "unknown".into(),
                },
            );
            out.field(
                "Ping size",
                &format!(
                    "{} prompt and {} reply tokens; prompt size from consecutive context growth and reply size from median recorded output",
                    or_unknown(k.ping_input_tokens),
                    or_unknown(k.ping_output_tokens)
                ),
            );
            out.field(
                "Idle periods",
                &format!(
                    "{} returns after a gap longer than the interval; {} final periods; {} unpriced",
                    k.returns, k.finals, k.unpriced
                ),
            );
            out.field(
                "Idle compaction saving",
                &k.idle_compaction_savings_usd
                    .map_or_else(|| "unknown or unsupported".into(), usd),
            );
            out.field("Best cap over best alternative", &estimate(k.margin_usd));
        }
        CheckResult::Speed(s) => {
            out.field("Standard-speed replacement", &s.replacement);
            out.field(
                "Observed response-tier premium (rate-only API equivalent)",
                &format!(
                    "{} over selected window; {} / 30 days",
                    money(&s.observed.premium),
                    estimate(s.observed.monthly_premium_usd)
                ),
            );
            out.field(
                "Speed coverage",
                &format!(
                    "{} premium, {} standard, {} unknown billed parts",
                    s.observed.premium_parts, s.observed.standard_parts, s.observed.unknown_parts
                ),
            );
            out.field(
                "Current-setting premium estimate (rate-only)",
                &format!(
                    "{}; {} priced premium parts, {} unknown parts",
                    money(&s.current_estimate.premium),
                    s.current_estimate.priced_parts,
                    s.current_estimate.unknown_parts
                ),
            );
            out.field(
                "Speed limits",
                "Recorded cache categories held constant; Claude cache-switch effects excluded. API-price equivalents, not subscription savings. Work held constant; latency value excluded.",
            );
        }
        CheckResult::Compaction(c) => {
            let models: Vec<String> = c
                .models
                .iter()
                .map(|m| m.as_deref().map_or_else(|| "unknown".into(), display))
                .collect();
            out.field("Models", &models.join(", "));
            if let Some(setting) = c.setting {
                out.field(
                    "Window setting",
                    &format!("{setting} tokens, capped at the model's context window"),
                );
            }
            out.field(
                "Conditional TTL baseline",
                &format!(
                    "main {}; subagent {}; window comparison follows available TTL recommendations",
                    or_unknown(c.ttls.main.map(Ttl::as_str)),
                    or_unknown(c.ttls.subagent.map(Ttl::as_str))
                ),
            );
            out.field("Current-window baseline cost", &money(&c.baseline_cost));
            let suggested = c
                .scenarios
                .iter()
                .find(|s| Some(s.window) == c.base.recommended);
            out.field(
                "Suggested-window cost",
                &suggested.map_or_else(|| "unknown".into(), |s| money(&s.cost)),
            );
            out.field(
                "Existing summaries (linked/proxy)",
                &money(&c.observed_summary_cost),
            );
            if let Some(s) = suggested {
                out.field(
                    "Net sensitivity",
                    &format!(
                        "larger observed summary/prefix {}; cold summary {}; extra re-read break-even {}. Conditional scenarios; the aggregate estimate decides.",
                        estimate(s.sensitivity.larger_summary_savings_usd),
                        estimate(s.sensitivity.cold_summary_savings_usd),
                        estimate(s.sensitivity.reread_break_even_usd)
                    ),
                );
            }
        }
    }
    if !r.usage_estimates.is_empty() {
        out.field(
            "Unlinked attempts",
            &format!(
                "{} estimated inputs; {} unsupported inputs. Original usage and historical billing stay unknown.",
                r.usage_estimates
                    .iter()
                    .filter(|e| e.evidence == EstimateEvidence::Estimated)
                    .count(),
                r.usage_estimates
                    .iter()
                    .filter(|e| e.evidence == EstimateEvidence::Unknown)
                    .count()
            ),
        );
    }
    if let Some(break_even) = r.break_even.filter(|b| !b.is_empty()) {
        out.field("Break-even", break_even);
    }
    out.field(
        "Limits",
        &format!(
            "{}Fixed-behavior replay with current settings applied to history; monthly values extrapolate the comparison window. Full assumptions and uncertainty: --details.",
            if r.check == "compaction-window" {
                "Earlier compaction loses detail and can add re-reads or changed work. "
            } else {
                ""
            }
        ),
    );
    if action == Action::Change {
        out.field(
            "Next",
            &format!(
                "Review evidence with --details, then update {} yourself.",
                display(r.owner)
            ),
        );
    }
    if action == Action::Advisory {
        out.field(
            "Next",
            if matches!(result, CheckResult::Memory(_)) {
                "Move what is worth keeping into a reviewed CLAUDE.md or AGENTS.md, then turn it off; advisory findings do not affect the exit code."
            } else {
                "Review evidence and resolve current ownership/coverage before changing a setting; advisory findings do not affect the exit code."
            },
        );
    }
    for id in r.history_profiles {
        out.field("History profile", &out.profile_name(id));
    }
    match result {
        CheckResult::Background(_) | CheckResult::Memory(_) => {}
        CheckResult::Ttl(t) => {
            for ttl in Ttl::ALL {
                let name = ttl.as_str();
                out.cost(&format!("{name} replay"), t.cost.get(ttl));
                out.cost(
                    &format!("{name} cache comparison"),
                    t.comparison_cost.get(ttl),
                );
                out.cost(&format!("{name} summary proxies"), t.summary_cost.get(ttl));
            }
            out.cost("Identical fixed costs", &t.fixed_cost);
            out.field(
                "Cold-prefix 1h replay",
                &estimate(t.sensitivity.cold_prefix1h_usd),
            );
            out.field(
                "Cold-prefix 1h cache comparison",
                &estimate(t.sensitivity.cold_prefix1h_comparison_usd),
            );
            if let Some(unlinked) = &t.sensitivity.unlinked {
                out.field(
                    "Attempt sensitivity",
                    "Conditional cache-only scenarios, not guaranteed bounds; the aggregate estimate decides.",
                );
                for ttl in Ttl::ALL {
                    let name = ttl.as_str();
                    out.cost(
                        &format!("{name}, no billed/cache-affecting attempt"),
                        unlinked.no_billed_attempt.get(ttl),
                    );
                    out.cost(
                        &format!("{name}, cold estimated input"),
                        unlinked.cold_cache.get(ttl),
                    );
                }
            }
        }
        CheckResult::T3Idle(i) => {
            for comparison in [&i.current_settings, &i.after_recommendations] {
                out.field("T3 period baseline", &comparison.baseline);
                let mut groups = IndexMap::default();
                for p in &comparison.periods {
                    *groups
                        .entry((p.status, p.return_timing, p.outcome, p.reason))
                        .or_insert(0usize) += 1;
                }
                for ((status, timing, outcome, reason), count) in groups {
                    out.field(
                        "T3 idle period group",
                        &format!(
                            "{count} periods; {}; {}; {}; {reason}",
                            label(&status),
                            label(&timing),
                            label(&outcome)
                        ),
                    );
                }
            }
        }
        CheckResult::Idle(i) => {
            for scenario in [&i.current_settings, &i.after_recommendations] {
                out.field("Idle scenario", &scenario.baseline);
                out.field(
                    "Conditional TTL baseline",
                    &format!(
                        "main {}; outside-main {}; project-specific policies are retained in the period evidence",
                        or_unknown(scenario.ttls.main.map(Ttl::as_str)),
                        or_unknown(scenario.ttls.subagent.map(Ttl::as_str))
                    ),
                );
                for w in &scenario.windows {
                    let models: Vec<&str> = w
                        .models
                        .iter()
                        .map(|m| m.as_deref().unwrap_or("unknown"))
                        .collect();
                    out.field(
                        "Window control",
                        &format!(
                            "{}; {} tokens; models {}",
                            w.owner,
                            or_unknown(w.value),
                            models.join(", ")
                        ),
                    );
                }
                out.cost("Idle on", &scenario.cost.on);
                out.cost("Idle off", &scenario.cost.off);
                out.cost(
                    "On adjustment to common recorded baseline",
                    &scenario.adjustment.on,
                );
                out.cost(
                    "Off adjustment to common recorded baseline",
                    &scenario.adjustment.off,
                );
                out.field(
                    "Idle context sizes",
                    &format!(
                        "{} distinct sizes ({} unknown); {}",
                        scenario.by_context.len(),
                        scenario
                            .by_context
                            .iter()
                            .filter(|s| s.tokens.is_none())
                            .count(),
                        token_range(scenario.by_context.iter().filter_map(|s| s.tokens))
                    ),
                );
                let mut groups = IndexMap::default();
                for p in &scenario.periods {
                    *groups
                        .entry((p.status, p.return_timing, p.outcome, p.reason))
                        .or_insert(0usize) += 1;
                }
                for ((status, timing, outcome, reason), count) in groups {
                    out.field(
                        "Idle period group",
                        &format!(
                            "{count} periods; {}; {}; {}; {reason}",
                            label(&status),
                            label(&timing),
                            label(&outcome)
                        ),
                    );
                }
                for cohort in &scenario.activation_cohorts {
                    out.field(
                        "Activation cohort",
                        &format!(
                            "{}/{}/{}: {}/{} applied ({})",
                            cohort.version,
                            cohort.client,
                            cohort.flag_state,
                            cohort.applied,
                            cohort.eligible,
                            js_number(cohort.fraction)
                        ),
                    );
                }
                out.notes("Idle unknown", &scenario.unknown);
            }
        }
        CheckResult::BashCap(b) => out.cost("Cache misses after long waits", &b.waits.miss_cost),
        CheckResult::Keepalive(k) => {
            for s in &k.scenarios {
                out.field(
                    &format!("Cap {}", keepalive_duration(s.cap_ms)),
                    &format!(
                        "{} return pings and {} final pings cost {}; {} rescued returns; recorded work saves {}; net {}; {} if final sessions closed at once",
                        s.pings,
                        s.final_pings,
                        usd(s.ping_cost_usd),
                        s.rescued,
                        usd(s.avoided_usd),
                        usd(s.savings_usd),
                        usd(s.closed_savings_usd)
                    ),
                );
            }
        }
        CheckResult::Speed(s) => {
            out.cost("Observed response-tier premium", &s.observed.premium);
            out.cost(
                "Current-setting premium estimate",
                &s.current_estimate.premium,
            );
        }
        CheckResult::Compaction(c) => {
            out.cost("Current-window baseline", &c.baseline_cost);
            out.cost("Existing summaries", &c.observed_summary_cost);
            for scenario in &c.scenarios {
                out.field(
                    "Candidate window",
                    &format!(
                        "{} tokens; {} affected requests; {} added compactions",
                        scenario.window,
                        scenario.affected_requests,
                        scenario.added.len()
                    ),
                );
                out.cost("Candidate full cost", &scenario.cost);
                out.cost(
                    "Candidate baseline comparison",
                    &scenario.baseline_comparison_cost,
                );
                out.cost("Candidate comparison", &scenario.comparison_cost);
                out.field(
                    "Candidate net saving vs baseline",
                    &estimate(scenario.savings_usd),
                );
                out.field(
                    "Candidate sensitivity",
                    &format!(
                        "larger observed summary/prefix net saving {}; cold summary net saving {}; extra re-read break-even {}",
                        estimate(scenario.sensitivity.larger_summary_savings_usd),
                        estimate(scenario.sensitivity.cold_summary_savings_usd),
                        estimate(scenario.sensitivity.reread_break_even_usd)
                    ),
                );
                if !scenario.added.is_empty() {
                    out.field(
                        "Added compactions",
                        &format!(
                            "{} summaries; input {}; output {}; resumed prefix {}",
                            scenario.added.len(),
                            token_range(scenario.added.iter().map(|a| a.input_tokens)),
                            token_range(scenario.added.iter().map(|a| a.summary_tokens)),
                            token_range(scenario.added.iter().map(|a| a.prefix_tokens))
                        ),
                    );
                    out.cost(
                        "Added summaries",
                        &add_costs(scenario.added.iter().map(|a| &a.summary_cost)),
                    );
                    out.cost(
                        "Resumed prefixes",
                        &add_costs(scenario.added.iter().map(|a| &a.prefix_cost)),
                    );
                    let basis: IndexSet<_> = scenario
                        .added
                        .iter()
                        .flat_map(|a| &a.basis)
                        .map(|b| (&b.event, &b.request))
                        .collect();
                    for (event, request) in basis {
                        out.field(
                            "Geometry basis",
                            &format!("boundary {event}; resumed request {request}"),
                        );
                    }
                }
                out.witnesses(&scenario.cache_evidence);
            }
        }
    }
    let mut attempts = IndexMap::default();
    for e in r.usage_estimates {
        let group = attempts
            .entry((e.evidence, e.max_output_tokens, e.reason))
            .or_insert((0usize, e, IndexSet::default()));
        group.0 += 1;
        group.2.extend(&e.assumptions);
    }
    for (count, e, assumptions) in attempts.into_values() {
        out.field(
            "Attempt",
            &format!(
                "{count} attempts; example {}: {}; {}; output budget {} (not observed output)",
                e.request,
                label(&e.evidence),
                e.reason,
                or_unknown(e.max_output_tokens)
            ),
        );
        for assumption in assumptions {
            out.field("Attempt assumption", assumption);
        }
    }
    match result {
        CheckResult::Ttl(t) => out.witnesses(&t.cache_evidence),
        CheckResult::Compaction(c) => out.witnesses(&c.cache_evidence),
        CheckResult::T3Idle(_)
        | CheckResult::Idle(_)
        | CheckResult::Speed(_)
        | CheckResult::Background(_)
        | CheckResult::BashCap(_)
        | CheckResult::Keepalive(_)
        | CheckResult::Memory(_) => {}
    }
    out.notes("Assumption", r.assumptions);
    out.notes("Uncertainty", r.uncertainty);
    out.notes("Source", r.sources);
}

fn reason(result: &CheckResult, r: &Common<'_>) -> String {
    match result {
        CheckResult::Speed(s) => return s.reason.clone(),
        CheckResult::Memory(m)
            if r.action == Action::Keep && memory_free(m) && !codex_unmeasured(m) =>
        {
            return "Off; it costs nothing.".into();
        }
        CheckResult::Memory(_) if r.action == Action::Keep => {
            return "Off now; its cost and uses earlier in the window are shown for reference.".into();
        }
        CheckResult::Memory(m) if m.unresolved.is_some() => {
            return format!(
                "{}. {MEMORY_ADVICE}. Its cost and uses are shown for reference, not weighed against --max-cost-per-use.",
                memory_unresolved(m)
            );
        }
        CheckResult::Memory(_) => return format!("{MEMORY_ADVICE}. Its cost and uses are shown for reference, not weighed against --max-cost-per-use."),
        CheckResult::Background(b) => {
            return match r.action {
                Action::Advisory if r.savings_usd.is_some() => {
                    "Its whole cost is above --max-cost-per-use, but its uses are not recorded, so it may still cost less per use.".into()
                }
                Action::Advisory => format!("{} {}", background_unknown(b), b.reason),
                _ if r.current == Some(Setting::Toggle(false)) => "Off; it costs nothing.".into(),
                _ => "Its whole cost is within --max-cost-per-use, so it costs no more than that per use.".into(),
            };
        }
        CheckResult::BashCap(b) => {
            return match (b.ttl, r.action) {
                (None, _) => "No main-agent cache TTL to compare the cap with.".into(),
                (Some(ttl), Action::Change) => format!(
                    "A foreground wait as long as the cap outlasts the {} cache; lower it below the TTL.",
                    ttl.as_str()
                ),
                (Some(ttl), _) => format!("The cap is under the {} cache TTL.", ttl.as_str()),
            };
        }
        CheckResult::Keepalive(_) => {
            return if r.recommended.is_none() {
                "The cache lifetime, ping size or too many idle periods could not be priced."
            } else {
                "Lowest estimated aggregate net cost among no pings, each cap and recommended idle compaction; final periods ping for the full cap."
            }
            .into();
        }
        CheckResult::T3Idle(_) => return "Lowest estimated changed cost among off, conditional on-return acceptance and conditional proactive server liveness; unavailable lifecycle or summary evidence stays unpriced.".into(),
        CheckResult::Idle(_) => {
            return if r.recommended.is_none() {
                "Idle prerequisites are inapplicable or required cost/eligibility inputs remain unpriced."
            } else {
                "Lowest estimated aggregate on/off net cost, including unresumed summaries; sensitivity does not veto the point estimate."
            }
            .into();
        }
        _ => {}
    }
    if r.evidence == Evidence::Guidance {
        return format!(
            "Provider guidance; dollars {}. {}",
            if r.savings_usd.is_none() {
                "unknown"
            } else {
                "estimated separately"
            },
            r.assumptions.join("; ")
        );
    }
    if r.recommended.is_none() {
        if r.uncertainty.iter().any(|s| s == TIER_SENSITIVITY) {
            return "Conditional API estimates are available, but missing effective tiers could reverse the winner.".into();
        }
        return "No priced winner established; missing evidence is unknown, never zero.".into();
    }
    if r.action == Action::Advisory {
        return "Conditional replay winner; current effective setting or applicability is unresolved.".into();
    }
    if r.check == "compaction-window" {
        "Lowest estimated aggregate net cost, including added summaries and resumed-prefix charges; existing boundaries retained."
    } else {
        "Lowest estimated aggregate cache cost under the stated replay assumptions."
    }
    .into()
}

/// Plain name for the setting a check is about.
fn setting_name(result: &CheckResult) -> String {
    match result {
        CheckResult::Ttl(t) => match t.bucket {
            crate::model::Bucket::Main => "Main-agent prompt cache TTL".into(),
            crate::model::Bucket::Subagent => "Subagent prompt cache TTL".into(),
        },
        CheckResult::Compaction(c) => {
            let models: Vec<&str> = c.models.iter().flatten().map(|m| &**m).collect();
            if models.is_empty() {
                "Auto-compact window".into()
            } else {
                format!("Auto-compact window ({})", models.join(", "))
            }
        }
        CheckResult::T3Idle(_) => "T3 idle compaction".into(),
        CheckResult::Idle(_) => "Idle compaction".into(),
        CheckResult::Speed(_) => "Fast mode".into(),
        CheckResult::Keepalive(_) => "Keepalive pings".into(),
        CheckResult::BashCap(_) => "Bash timeout cap".into(),
        CheckResult::Memory(m) => match m.harness {
            Harness::Codex => CODEX_MEMORY.into(),
            Harness::Claude => CLAUDE_MEMORY.into(),
        },
        CheckResult::Background(b) => match b.feature {
            BackgroundFeature::PromptSuggestions => "Prompt suggestions".into(),
            BackgroundFeature::SessionRecaps => "Session recaps".into(),
            BackgroundFeature::GoalCheckIns => "Goal check-ins".into(),
            BackgroundFeature::AutoRecaps => "Codex automatic recaps".into(),
        },
    }
}

/// Why a background feature's cost is unknown.
fn background_unknown(b: &BackgroundCheck) -> &'static str {
    if b.base.current.is_none() {
        return "Cost unknown: current scope or launch selection is unresolved.";
    }
    match b.feature {
        BackgroundFeature::GoalCheckIns => {
            "Cost unknown: check-in turns are not marked in transcripts."
        }
        BackgroundFeature::AutoRecaps => "Cost unknown: Codex does not record recap requests.",
        _ => "Cost unknown: none of the turns where it would have run have recorded usage.",
    }
}

/// A dollar amount per 30 days at the window's rate.
fn per_month(usd: f64, since: Option<Millis>, until: Option<Millis>) -> String {
    match (since, until) {
        (Some(from), Some(to)) if to > from => about((usd * 30.0 * DAY) / (to - from) as f64),
        _ => about(usd),
    }
}

/// Why keepalive pings stay off.
fn keepalive_keep(k: &KeepaliveCheck, r: &Common<'_>) -> String {
    let interval = k.interval_ms.map(keepalive_duration).unwrap_or_default();
    match k.margin_usd {
        Some(m) => format!(
            "Pinging idle sessions every {interval} would cost {} more than {}.",
            per_month(-m, r.since, r.until),
            if k.idle_compaction_savings_usd.is_some_and(|s| s > 0.0) {
                "idle compaction"
            } else {
                "leaving them idle"
            }
        ),
        None => format!(
            "No session sat idle past its {} cache and came back, so pings would only add cost.",
            k.lifetime_ms.map(keepalive_duration).unwrap_or_default()
        ),
    }
}

/// Why a keepalive check gives no actionable verdict.
fn keepalive_unresolved(k: &KeepaliveCheck, r: &Common<'_>) -> String {
    if r.recommended == Some(Setting::Keepalive("off")) {
        return keepalive_keep(k, r);
    }
    if let Some(Setting::Keepalive(policy)) = r.recommended {
        return format!(
            "Pinging idle sessions {policy} would save {}, but {}.",
            r.monthly_savings_usd.map_or_else(
                || per_month(r.savings_usd.unwrap_or(0.0), r.since, r.until),
                about
            ),
            r.owner.strip_prefix("no setting: ").unwrap_or(r.owner)
        );
    }
    if k.lifetime_ms.is_none() {
        "The main cache TTL is unknown, so pings could not be priced.".into()
    } else if k.ping_input_tokens.is_none() || k.ping_output_tokens.is_none() {
        "No consecutive request geometry establishes a ping input size.".into()
    } else if r.coverage.is_none_or(|c| c.priced == 0) {
        "No idle period could be priced.".into()
    } else if let (Some(best), Some(interval)) = (
        k.scenarios
            .iter()
            .max_by(|a, b| a.savings_usd.total_cmp(&b.savings_usd)),
        k.interval_ms,
    ) {
        if k.unpriced == 0 && k.idle_compaction_savings_usd.is_none() {
            return format!(
                "Pinging {} would {} {} compared with no pings, but idle compaction could not be priced.",
                crate::keepalive::policy(interval, best.cap_ms),
                if best.savings_usd > 0.0 {
                    "save"
                } else {
                    "cost"
                },
                per_month(best.savings_usd.abs(), r.since, r.until)
            );
        }
        let margin = k.margin_usd.unwrap_or(best.savings_usd);
        format!(
            "On fully priced sessions, pinging {} would {} {} compared with no pings, but {} unpriced periods leave the aggregate comparison unresolved.",
            crate::keepalive::policy(interval, best.cap_ms),
            if margin > 0.0 { "save" } else { "cost" },
            per_month(margin.abs(), r.since, r.until),
            k.unpriced,
        )
    } else {
        "Too many idle periods could not be priced to tell whether pings pay off.".into()
    }
}

/// One plain sentence on why a check could not reach a verdict.
fn unresolved(result: &CheckResult, r: &Common<'_>) -> String {
    if r.uncertainty.iter().any(|s| s == TIER_SENSITIVITY) {
        let baseline = match result {
            CheckResult::Compaction(c) => c
                .baseline_cost
                .total_usd
                .map(|usd| {
                    format!(
                        "Conditional baseline cost {}. ",
                        per_month(usd.to_f64(), r.since, r.until)
                    )
                })
                .unwrap_or_default(),
            _ => String::new(),
        };
        return format!(
            "{baseline}The winning setting is unresolved because effective service tiers could reverse it."
        );
    }
    match result {
        CheckResult::Speed(s) => return capitalize(&s.reason) + ".",
        CheckResult::Background(b) => return background_unknown(b).into(),
        _ => {}
    }
    if let CheckResult::BashCap(b) = result {
        return format!(
            "{}, so the cap has nothing to compare with.",
            capitalize(&b.ttl_basis)
        );
    }
    if let CheckResult::Keepalive(k) = result {
        return keepalive_unresolved(k, r);
    }
    if let Some(recommended) = r.recommended {
        return if Some(recommended) == r.current {
            "The replay favors the current value, but could not confirm it applies to your current sessions.".into()
        } else {
            format!(
                "The replay favors {}, but could not confirm the current setting applies to your sessions.",
                setting_value(Some(recommended))
            )
        };
    }
    if r.requests == 0 {
        return "No requests in this window to compare.".into();
    }
    match result {
        CheckResult::Ttl(t) => {
            if r.coverage.is_none_or(|c| c.priced == 0) {
                return "No requests with measured cache usage to compare.".into();
            }
            format!(
                "5m and 1h differ by only {} over {} on measured requests, too close to call.",
                about(t.one_hour_savings_usd.abs()),
                days(r.since, r.until)
            )
        }
        CheckResult::Compaction(c) => {
            if r.current.is_none() {
                return if r
                    .uncertainty
                    .iter()
                    .any(|u| u.contains("default is not established"))
                {
                    "No window is set for this model and neither a recorded compaction nor Claude Code's documented defaults establish its default."
                } else {
                    "The current window setting could not be resolved."
                }
                .into();
            }
            if r.uncertainty.iter().any(|u| u.contains(CAPPED_SHARED)) {
                return format!("The model's context window caps it, and {CAPPED_SHARED}.");
            }
            if c.scenarios
                .iter()
                .flat_map(|s| &s.comparison_cost.unknown)
                .any(|u| u.contains("compaction summary"))
            {
                return "Too few recorded compactions to model a different window.".into();
            }
            "Too many requests lack recorded usage to compare windows.".into()
        }
        CheckResult::T3Idle(c) => t3_idle_unresolved(c, r),
        CheckResult::Idle(i) => {
            let periods = &i.after_recommendations.periods;
            if !periods.is_empty()
                && periods
                    .iter()
                    .all(|p| matches!(p.status, PeriodStatus::Censored | PeriodStatus::Ineligible))
            {
                return "Sessions were never idle long enough for it to trigger, so it made no difference.".into();
            }
            if !periods.iter().any(|p| p.status == PeriodStatus::Eligible) {
                return "No idle gaps could be compared with it on and off.".into();
            }
            if i.after_recommendations.delta_usd == Some(0.0) {
                return "On and off cost the same on recorded idle gaps, so neither is cheaper."
                    .into();
            }
            "Idle summaries or resumed sessions could not be priced.".into()
        }
        CheckResult::Speed(_)
        | CheckResult::Background(_)
        | CheckResult::BashCap(_)
        | CheckResult::Keepalive(_)
        | CheckResult::Memory(_) => {
            unreachable!("handled above")
        }
    }
}

/// The modes T3 could price, and the first input the others lack.
fn t3_idle_unresolved(c: &T3IdleCheck, r: &Common<'_>) -> String {
    let scenarios = &c.after_recommendations.scenarios;
    let priced: Vec<String> = scenarios
        .iter()
        .filter_map(|s| {
            let usd = s.cost.total_usd?.to_f64();
            Some(format!(
                "{} {}",
                s.mode.as_str(),
                per_month(usd, r.since, r.until)
            ))
        })
        .collect();
    let priced = if priced.is_empty() {
        String::new()
    } else {
        format!("Changed cost per month: {}. ", priced.join(", "))
    };
    let unpriced: Vec<&str> = scenarios
        .iter()
        .filter(|s| s.cost.total_usd.is_none())
        .map(|s| s.mode.as_str())
        .collect();
    let reasons: IndexSet<&str> = scenarios
        .iter()
        .flat_map(|s| s.cost.unknown.iter().map(String::as_str))
        .collect();
    let Some(first) = reasons.first() else {
        if c.after_recommendations.periods.is_empty() {
            return "No main-thread idle gaps in this window to compare.".into();
        }
        // Every mode is priced, so only a disabled instance or ownership keeps it advisory.
        return format!(
            "{priced}The instance is disabled or its sessions' ownership is unresolved, so no mode is recommended."
        );
    };
    let more = match reasons.len() - 1 {
        0 => String::new(),
        1 => " (and 1 other missing input)".into(),
        n => format!(" (and {n} other missing inputs)"),
    };
    let modes = match unpriced[..] {
        _ if unpriced.len() == scenarios.len() => "No mode is priced".into(),
        [mode] => format!("{mode} is unpriced"),
        _ => format!("{} are unpriced", unpriced.join(" and ")),
    };
    format!("{priced}{modes}: {first}{more}.")
}

/// How to turn off a namespaced skill, which `skillOverrides` does not reach.
const PLUGIN_SKILL: &str =
    "Turn off its plugin with /plugin, or remove the skill from the added directory";

fn piece_label(c: &ContextCost, out: &Out<'_>) -> String {
    match c.kind {
        ContextKind::Tool => format!("{} tool", c.name),
        ContextKind::McpServer if c.harness == Harness::Codex && c.name == "codex_apps" => {
            "ChatGPT apps".into()
        }
        ContextKind::McpServer => format!("MCP server {}", c.name),
        ContextKind::McpTool => format!("MCP tool {} (included in server total)", c.name),
        ContextKind::McpDiscovery => format!("MCP discovery {} (included in server total)", c.name),
        ContextKind::Skill if c.name == SYNCED_SKILLS => "Skills synced from claude.ai".into(),
        ContextKind::ToolOutput => match c.name.strip_prefix("mcp__") {
            Some(server) => format!("MCP server {server} output"),
            None => format!("{} output", c.name),
        },
        ContextKind::Skill if c.skill.as_ref().is_some_and(|s| s.path.is_some()) => format!(
            "Skill {} ({})",
            c.name,
            out.home(c.skill.as_ref().unwrap().path.as_ref().unwrap())
        ),
        ContextKind::Skill => format!("Skill {}", c.name),
        ContextKind::Instructions => out.home(&c.name),
        ContextKind::System | ContextKind::Memory | ContextKind::Other => c.name.clone(),
    }
}

/// A piece's name for a report line, whose evidence says when it is part of a server's cost.
fn short_label(c: &ContextCost, out: &Out<'_>) -> String {
    match c.kind {
        ContextKind::McpTool => format!("MCP tool {}", c.name),
        _ => piece_label(c, out),
    }
}

/// How often a feature was used over the window. Only the model's calls count for a
/// skill that stays typeable.
fn uses(c: &ContextCost) -> String {
    let typed = typed_only(c.control.as_ref());
    match c.calls {
        None => "recorded uses are unknown".into(),
        Some(calls) if c.skill.is_some() => format!("{} observed skill turns", count(calls)),
        Some(0) if typed => "the model never used it".into(),
        Some(0) => "never used".into(),
        Some(calls) => format!(
            "{} {} time{}",
            if typed { "the model used it" } else { "used" },
            count(calls),
            if calls == 1 { "" } else { "s" }
        ),
    }
}

/// Codex counts positive request/load evidence, never inferring successful execution or nonuse.
fn catalog_evidence(c: &ContextCost) -> String {
    let Some(s) = &c.skill else {
        return String::new();
    };
    let mut text = format!(
        "{} requested; {} confirmed SKILL.md loads; successful invocation unknown",
        count(s.requested),
        count(s.loaded)
    );
    if s.ambiguous_requests > 0 {
        text += &format!(
            "; {} name-only requests ambiguous across paths, excluded from uses",
            count(s.ambiguous_requests)
        );
    }
    text
}

fn catalog_control(c: &ContextCost) -> String {
    match c.skill.as_ref().and_then(|s| s.path.as_ref()) {
        Some(path) => format!("Disable in config.toml with [[skills.config]], path = {}, enabled = false", toml::Value::String(path.clone())),
        None => "Disable control unavailable: the recorded catalog does not resolve an absolute SKILL.md path".into(),
    }
}

fn catalog_guidance(report: &Report, out: &mut Out<'_>) {
    if !report.context.iter().any(|c| c.skill.is_some()) {
        return;
    }
    out.line("Codex skill amounts are rendered listing costs, not promised savings: removing a skill can backfill the catalog or expand other descriptions. A request and confirmed loads in one user turn count as one use; absent turn boundaries, evidence is conservatively grouped within its session. Visibility is incomplete; zero observations do not prove nonuse. Disabling also removes explicit requests. allow_implicit_invocation = false is not a catalog-cost control.", 0, PLAIN);
    let mut seen = IndexSet::default();
    for c in report
        .context
        .iter()
        .filter(|c| c.current && c.skill.as_ref().is_some_and(|s| s.duplicate_entries > 1))
        .filter(|c| seen.insert((&c.profiles, &c.name)))
    {
        let s = c.skill.as_ref().unwrap();
        let retained = s.retained_path.as_ref().map_or_else(
            || "an unresolved path; resolve the listed paths before disabling copies".into(),
            |p| out.home(p),
        );
        out.line(&format!("Duplicate skill {}: {} same-named catalog entries. Preferred copy: {retained}; disable redundant paths. The preferred copy still follows --max-cost-per-use. Repeated entries of the same path need their duplicate discovery source removed. If a copy is symlinked, remove the redundant link at its source; transcripts cannot establish symlink identity.", c.name, count(s.duplicate_entries)), 2, PLAIN);
    }
}

/// One harness's MCP loading guidance, ending with what the report cannot measure about it.
struct Loading {
    notes: Vec<&'static str>,
    source: Option<&'static str>,
}

/// The MCP loading guidance for each harness with a server in `costs`.
fn mcp_guidance<'a>(costs: impl Iterator<Item = &'a ContextCost>) -> Vec<Loading> {
    let mut claude = false;
    let mut codex = false;
    let mut any = false;
    for c in costs.filter(|c| c.kind == ContextKind::McpServer) {
        any = true;
        match c.harness {
            Harness::Claude => claude = true,
            Harness::Codex if c.name != "codex_apps" => codex = true,
            _ => {}
        }
    }
    let unmeasured = "Tool definitions and discovery context are included in their server's cost. Missing discovery records mean unknown overhead, not zero. Search turns, returned definitions, cache changes and repeated loading cannot be attributed completely, so net deferral savings are unknown and no loading change is recommended.";
    let mut guidance = vec![];
    if claude {
        guidance.push(Loading {
            notes: vec![
                "Claude MCP loading: env.ENABLE_TOOL_SEARCH in settings.json defaults to deferral on supported models. true defers tools; false loads upfront. auto uses a 10% schema threshold; auto:N changes the percentage. A server's alwaysLoad=true loads upfront; alwaysLoad=false overrides tool-author upfront annotations on Claude Code 2.1.287+. A small frequently used toolset can cost less upfront.",
                "Claude tool search needs Opus 4/Sonnet 4/Haiku 4.5 or later; Google Agent Platform needs 4.5 or later. Azure-hosted Foundry deployments do not support it. Experimental-beta restrictions, HIPAA policy, denying ToolSearch, and gateways can prevent deferral.",
                unmeasured,
            ],
            source: Some("https://code.claude.com/docs/en/mcp#configure-tool-search"),
        });
    }
    if codex {
        guidance.push(Loading {
            notes: vec![
                "Codex MCP loading: omit_tools_from changes exposure on direct (initial tools), deferred (tool search), and code_mode surfaces. Omission does not block calls or guarantee another surface exists. Use enabled_tools to allow tools or disabled_tools to block them; the deny list wins. A per-tool patch requires a native completion proving the raw tool name in every owning profile; exposed names may be sanitized or shortened.",
                unmeasured,
            ],
            source: Some("https://learn.chatgpt.com/docs/config-file/config-reference"),
        });
    }
    // ChatGPT apps alone have no loading switch, only what the report cannot measure.
    if any && !claude && !codex {
        guidance.push(Loading {
            notes: vec![unmeasured],
            source: None,
        });
    }
    guidance
}

/// The `project_doc_max_bytes` budget every profile that loaded an AGENTS.md shares,
/// or why it is unknown.
fn agents_budget(c: &ContextCost, profiles: &[Profile]) -> Result<usize, String> {
    let selected = || profiles.iter().filter(|p| c.profiles.contains(&p.id));
    if let Some(reason) = selected().find_map(|p| p.instruction_budget_unresolved.as_ref()) {
        return Err(format!("{reason}."));
    }
    let budgets: crate::collections::HashSet<Option<usize>> = selected()
        .map(|p| match p.settings.get("project_doc_max_bytes") {
            None => Some(32 * 1024),
            Some(Scalar::Number(n)) if *n >= 0.0 && n.fract() == 0.0 && *n < usize::MAX as f64 => {
                Some(*n as usize)
            }
            _ => None,
        })
        .collect();
    match budgets.into_iter().collect::<Vec<_>>()[..] {
        [Some(budget)] => Ok(budget),
        _ => Err(
            "The selected profiles' project_doc_max_bytes budgets are unavailable or differ."
                .into(),
        ),
    }
}

/// Per-server controls and instruction guidance beneath their owning row.
fn context_guidance(c: &ContextCost, profiles: &[Profile], name: &str) -> Vec<String> {
    if c.kind == ContextKind::McpServer {
        return match c.harness {
            Harness::Claude => vec![format!(
                "Server override: mcpServers.{}.alwaysLoad in the MCP configuration that declares it (.mcp.json or that scope's configuration).", c.name
            )],
            Harness::Codex if c.name == "codex_apps" => vec![
                "Per-app controls: apps.<id>.enabled=false, apps.<id>.tools.<tool>.enabled=false, and apps.<id>.omit_tools_from=[\"direct\"] use the app id reported by app/installed, not its display name. These transcripts do not establish that id mapping, so per-app costs and settings patches are unavailable. https://learn.chatgpt.com/docs/app-server#apps-connectors".into()
            ],
            Harness::Codex => c.control.as_ref()
                .filter(|control| control.path.first().is_some_and(|p| p == "mcp_servers"))
                .map(|control| {
                    let mut omission = control.clone();
                    omission.path[2] = "omit_tools_from".into();
                    omission.value = ControlValue::Text("direct".into());
                    omission.append = true;
                    vec![format!("Exposure: {} omits this server's initial tools.", control_text(&omission, Harness::Codex))]
                }).unwrap_or_default(),
        };
    }
    if c.kind != ContextKind::Instructions {
        return vec![];
    }
    let claude_file = c
        .name
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|n| n == "CLAUDE.md" || n == "CLAUDE.local.md");
    if c.harness == Harness::Claude && !claude_file {
        return vec![];
    }
    let Some(size) = c.instruction_size else {
        return vec![
            "Instruction budget unknown: this record has no rendered instruction text to measure."
                .into(),
        ];
    };
    match c.harness {
        Harness::Claude => {
            vec![format!(
                "{}: {} recorded lines, {} UTF-8 bytes; {}Claude recommends under 200 lines per CLAUDE.md. This is guidance, not a quality score or a savings estimate. Prefer path-scoped rules for scoped instructions; imports still load together. https://code.claude.com/docs/en/memory#write-effective-instructions",
                name,
                size.lines,
                size.bytes,
                if size.lines >= 200 {
                    "at or above the recommended length. "
                } else {
                    ""
                }
            )]
        }
        Harness::Codex => {
            let budget = agents_budget(c, profiles);
            vec![format!(
                "Recorded rendered AGENTS.md: {} UTF-8 bytes, including any headers or wrappers. {}The project_doc_max_bytes default is 32768 bytes across the source instruction chain. Rendered bytes cannot establish whether that source chain exceeded its budget. Command-line overrides and original source-file sizes are unavailable; loaded text may already be truncated. Trim or scope guidance to preserve important instructions. Length alone does not establish quality. https://learn.chatgpt.com/docs/agent-configuration/agents-md",
                size.bytes,
                budget.map_or_else(
                    |reason| format!("{reason} "),
                    |n| format!("Selected profile budget: {n} bytes. ")
                )
            )]
        }
    }
}

/// A setting that caps how much of a tool's output stays in context.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Cap {
    label: &'static str,
    /// Its current value, briefly.
    value: String,
    /// A sentence naming the setting and its value.
    text: String,
}

/// The setting that caps how much of one tool's output stays in context, and its current value.
fn output_cap(report: &Report, c: &ContextCost) -> Option<Cap> {
    let profile = report
        .scan
        .profiles
        .iter()
        .find(|p| p.id == first_profile(c));
    let number = |key: &str| match profile.and_then(|p| p.settings.get(key)) {
        Some(Scalar::Number(n)) => Some(count(*n as i64)),
        _ => None,
    };
    let cap = |label, value: String, text: String| Cap { label, value, text };
    Some(match c.harness {
        Harness::Claude if c.name == "Bash" => match (
            number("bashOutputMaxChars"),
            number("BASH_MAX_OUTPUT_LENGTH"),
        ) {
            (Some(n), _) => cap(
                "Bash output",
                format!("{n} chars"),
                format!("bashOutputMaxChars is {n} characters"),
            ),
            (None, Some(n)) => cap(
                "Bash output",
                format!("{n} chars"),
                format!("BASH_MAX_OUTPUT_LENGTH is {n} characters"),
            ),
            (None, None) => cap(
                "Bash output",
                "unset".into(),
                "bashOutputMaxChars is unset, so about 30,000 characters".into(),
            ),
        },
        Harness::Claude if c.name.starts_with("mcp__") => match number("MAX_MCP_OUTPUT_TOKENS") {
            Some(n) => cap(
                "MCP output",
                format!("{n} tokens"),
                format!("MAX_MCP_OUTPUT_TOKENS is {n} tokens"),
            ),
            None => cap(
                "MCP output",
                "unset".into(),
                "MAX_MCP_OUTPUT_TOKENS is unset, so 25,000 tokens".into(),
            ),
        },
        Harness::Codex => match number("tool_output_token_limit") {
            Some(n) => cap(
                "Tool output",
                format!("{n} tokens"),
                format!("tool_output_token_limit is {n} tokens"),
            ),
            None => cap(
                "Tool output",
                "unset".into(),
                "tool_output_token_limit is unset".into(),
            ),
        },
        Harness::Claude => return None,
    })
}

/// Anthropic's quality guidance when a Claude window is larger than a priced candidate.
fn context_rot(report: &Report, result: &CheckResult) -> &'static str {
    match result {
        CheckResult::Compaction(c)
            if report
                .scan
                .profiles
                .iter()
                .any(|p| p.id == c.base.profile && p.harness == Harness::Claude)
                && c.base.current.is_some_and(|current| {
                    c.scenarios
                        .iter()
                        .any(|s| s.window < current && s.savings_usd.is_some())
                }) =>
        {
            " Anthropic notes that accuracy and recall degrade as context grows (context rot); that quality effect is not priced."
        }
        _ => "",
    }
}

/// How large requests on a compaction check's models got, to judge a window by.
fn context_sizes(report: &Report, result: &CheckResult, profile: &str) -> String {
    let CheckResult::Compaction(c) = result else {
        return String::new();
    };
    let mut sizes: Vec<i64> = report
        .scan
        .requests
        .iter()
        .filter(|q| q.profiles.iter().any(|p| p == profile) && c.models.contains(&q.model))
        .flat_map(|q| q.parts.iter().filter_map(|p| p.usage.input))
        .collect();
    if sizes.is_empty() {
        return String::new();
    }
    sizes.sort_unstable();
    let at = |q: f64| {
        let index = ((q * sizes.len() as f64).floor() as usize).min(sizes.len() - 1);
        count(js_round(sizes[index] as f64 / 1000.0) as i64 * 1000)
    };
    format!(
        " Its requests carried a median of {} tokens, and 1 in 10 carried more than {}. A smaller window makes each request cheaper, but every compaction pays for a summary and drops detail.",
        at(0.5),
        at(0.9)
    )
}

fn coverage_note(r: &Common<'_>) -> String {
    match r.coverage {
        // Percent, because one request can bill several parts.
        Some(c) if c.priced != c.total => format!(
            ", using the {}% of requests with measured usage",
            percent(c.priced, c.total)
        ),
        _ => String::new(),
    }
}

/// A sum of context costs as a row's figure, and a sentence counting the pieces it leaves out.
fn known_figure(values: &[Option<f64>]) -> (String, String) {
    let known: Vec<f64> = values.iter().flatten().copied().collect();
    if known.is_empty() {
        return ("unknown".into(), String::new());
    }
    let total = about(known.iter().fold(0.0, |n, c| n + c));
    match values.len() - known.len() {
        0 => (total, String::new()),
        1 => (
            format!("at least {total}"),
            "1 piece could not be priced. ".into(),
        ),
        n => (
            format!("at least {total}"),
            format!("{} pieces could not be priced. ", count(n as u64)),
        ),
    }
}

/// The profile a piece of context is filed under: the first of its sorted set.
fn first_profile(c: &ContextCost) -> &str {
    c.profiles.first().map_or("", String::as_str)
}

/// The kind of claim a line's recommendation rests on, in report order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Claim {
    /// Replayed from the user's own transcripts at public API prices.
    Measured,
    /// The provider's own guidance, cited.
    Official,
    /// The maintainer's preference, beside whatever was measured.
    Opinion,
}

impl Claim {
    const ALL: [Claim; 3] = [Claim::Measured, Claim::Official, Claim::Opinion];

    fn name(self) -> &'static str {
        match self {
            Claim::Measured => "Measured",
            Claim::Official => "Official",
            Claim::Opinion => "Opinion",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            Claim::Measured => "MEASURED  replayed from your own sessions",
            Claim::Official => "OFFICIAL  the provider's own guidance",
            Claim::Opinion => "OPINION  the maintainer's preference",
        }
    }

    /// The kind of claim a check's recommendation rests on.
    fn of(result: &CheckResult) -> Claim {
        match result {
            CheckResult::Speed(_) | CheckResult::Memory(_) => Claim::Opinion,
            _ => Claim::Measured,
        }
    }
}

/// A line's verdict, in the order lines are listed within a section.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Mark {
    /// A recommended change, which exits 1.
    Change,
    /// An advisory, such as a feature worth considering off.
    Consider,
    /// A setting or feature already at its recommended value.
    Keep,
    /// Not enough evidence for a verdict.
    Unknown,
    /// Evidence with no verdict to give, such as context no setting turns off.
    Info,
}

impl Mark {
    /// The word that carries the verdict when color cannot.
    fn word(self) -> &'static str {
        match self {
            Mark::Change => "change",
            Mark::Consider => "consider",
            Mark::Keep => "keep",
            Mark::Unknown => "unknown",
            Mark::Info => "info",
        }
    }

    /// The ANSI style that carries the verdict: red, yellow, green, or dim.
    fn style(self) -> &'static str {
        match self {
            Mark::Change => tier(Action::Change),
            Mark::Consider => tier(Action::Advisory),
            Mark::Keep => tier(Action::Keep),
            Mark::Unknown | Mark::Info => DIM,
        }
    }
}

/// A dim line's ANSI style.
const DIM: &str = "2";
/// Cells the plain-text verdict word takes, with its gap.
const WORD_CELLS: usize = 9;
/// Names and values wider than this are not padded, so one long name doesn't widen every line.
const NAME_CELLS: usize = 40;
const VALUE_CELLS: usize = 18;
/// Dollar figures wider than this, such as "at least $1.41", are not right-aligned.
const FIGURE_CELLS: usize = 8;

/// One setting or feature in the default report: a heading line and a line of evidence.
struct Row {
    claim: Claim,
    mark: Mark,
    name: String,
    /// The current value, and the recommended one after an arrow for a change.
    value: String,
    /// What it saves per 30 days or costs over the window, as a verb and a figure.
    amount: Option<(&'static str, String)>,
    /// The profile it belongs to, when the report covers several.
    profile: String,
    evidence: String,
    /// The provider guidance an Official line cites.
    sources: Vec<&'static str>,
}

impl Row {
    fn new(claim: Claim, mark: Mark, name: String) -> Self {
        Row {
            claim,
            mark,
            name,
            value: String::new(),
            amount: None,
            profile: String::new(),
            evidence: String::new(),
            sources: vec![],
        }
    }
}

/// `text` padded with spaces to `width` cells.
fn pad(text: &str, width: usize) -> String {
    let cells = cells(text);
    format!("{text}{}", " ".repeat(width.saturating_sub(cells)))
}

/// The widest of `values` that fits in `cap` cells.
fn column(values: impl Iterator<Item = usize>, cap: usize) -> usize {
    values.filter(|&n| n <= cap).max().unwrap_or(0)
}

/// Lower a sentence's first letter for use after a colon, unless it starts a name such as T3 or Claude.
fn decapitalize(text: &str) -> String {
    let word = text.split(' ').next().unwrap_or_default();
    let mut chars = word.chars();
    let lower = chars.next().is_some_and(char::is_uppercase)
        && chars.all(char::is_lowercase)
        && !matches!(word, "Claude" | "Codex" | "Anthropic" | "OpenAI");
    if !lower {
        return text.to_string();
    }
    let mut chars = text.chars();
    chars
        .next()
        .into_iter()
        .flat_map(char::to_lowercase)
        .chain(chars)
        .collect()
}

/// A row's evidence when there is not enough of it for a verdict.
fn not_enough(reason: &str) -> String {
    format!("Not enough evidence: {}", decapitalize(reason.trim()))
}

/// Every row, one section per kind of claim, red lines first.
fn rows_section(rows: &[Row], out: &mut Out<'_>) {
    let color = out.options.color;
    let (dot, hollow) = if out.options.unicode {
        ("●", "○")
    } else {
        ("*", "o")
    };
    let indent = if color { 2 } else { WORD_CELLS };
    for claim in Claim::ALL {
        let mut section: Vec<&Row> = rows.iter().filter(|r| r.claim == claim).collect();
        if section.is_empty() {
            continue;
        }
        section.sort_by_key(|r| r.mark);
        let names: Vec<String> = section.iter().map(|r| terminal_text(&r.name)).collect();
        let values: Vec<String> = section.iter().map(|r| terminal_text(&r.value)).collect();
        let value_cells = column(values.iter().map(|v| cells(v)), VALUE_CELLS);
        let figure_cells = column(
            section
                .iter()
                .filter_map(|r| r.amount.as_ref().map(|(_, f)| cells(f))),
            FIGURE_CELLS,
        );
        let amounts: Vec<String> = section
            .iter()
            .map(|r| {
                r.amount.as_ref().map_or_else(String::new, |(verb, f)| {
                    format!(
                        "{verb} {}{f}",
                        " ".repeat(figure_cells.saturating_sub(cells(f)))
                    )
                })
            })
            .collect();
        let amount_cells = column(amounts.iter().map(|a| cells(a)), FIGURE_CELLS + 6);
        let tag_cells = section.iter().map(|r| cells(&r.profile)).max().unwrap_or(0);
        // On a narrow terminal, the name column gives way so the others still fit on one row.
        let mut name_cells = column(names.iter().map(|n| cells(n)), NAME_CELLS);
        if let Some(width) = out.options.width {
            let rest = indent + value_cells + amount_cells + tag_cells + 6;
            name_cells = name_cells.min(width.saturating_sub(rest));
        }
        out.section(claim.heading());
        for (i, row) in section.iter().enumerate() {
            let marker = match (color, row.mark) {
                (true, Mark::Unknown | Mark::Info) => format!("{hollow} "),
                (true, _) => format!("{dot} "),
                (false, mark) => pad(mark.word(), WORD_CELLS),
            };
            let mut columns = vec![pad(&names[i], name_cells)];
            if values.iter().any(|v| !v.is_empty()) {
                columns.push(pad(&values[i], value_cells));
            }
            if amounts.iter().any(|a| !a.is_empty()) {
                columns.push(pad(&amounts[i], amount_cells));
            }
            if tag_cells > 0 {
                columns.push(row.profile.clone());
            }
            let head = format!("{marker}{}", columns.join("  ").trim_end());
            out.hang(&head, indent, row.mark.style());
            out.line(&row.evidence, indent, PLAIN);
        }
        let sources: IndexSet<&str> = section.iter().flat_map(|r| r.sources.clone()).collect();
        if !sources.is_empty() {
            let sources: Vec<&str> = sources.into_iter().collect();
            out.line(&format!("Sources: {}", sources.join(", ")), 0, PLAIN);
        }
    }
}

/// What a context piece costs over the window, as a row's amount.
fn cost_amount(usd: Option<f64>) -> Option<(&'static str, String)> {
    Some(("costs", usd.map_or_else(|| "unknown".into(), about)))
}

/// A setting's value in tokens, shortened to thousands or millions when exact.
fn tokens(n: i64) -> String {
    match n {
        0 => "0".into(),
        n if n % 1_000_000 == 0 => format!("{}M", n / 1_000_000),
        n if n % 1000 == 0 => format!("{}k", n / 1000),
        n => count(n),
    }
}

/// The documented page behind Claude Code's CLAUDE.md length guidance.
const CLAUDE_MEMORY_DOCS: &str =
    "https://code.claude.com/docs/en/memory#write-effective-instructions";
/// The documented page behind Codex's AGENTS.md byte budget.
const CODEX_AGENTS_DOCS: &str = "https://learn.chatgpt.com/docs/agent-configuration/agents-md";

/// An instruction file's row: Claude's line guidance or Codex's byte budget, where one applies.
fn instruction_row(c: &ContextCost, profiles: &[Profile], out: &Out<'_>) -> Row {
    let name = out.home(&c.name);
    let claude_file = c
        .name
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|n| n == "CLAUDE.md" || n == "CLAUDE.local.md");
    let advice = "Shorten it, or move parts into path-scoped rules or skills.";
    if c.harness == Harness::Claude && !claude_file {
        let mut row = Row::new(Claim::Measured, Mark::Info, name);
        row.amount = cost_amount(c.estimated_usd);
        row.evidence =
            format!("No provider size guidance covers it, and no setting turns it off. {advice}");
        return row;
    }
    let Some(size) = c.instruction_size else {
        let mut row = Row::new(Claim::Official, Mark::Unknown, name);
        row.amount = cost_amount(c.estimated_usd);
        row.evidence = not_enough(
            "This record has no rendered instruction text to measure against the provider's guidance.",
        );
        return row;
    };
    let mut row = match c.harness {
        Harness::Claude => {
            let over = size.lines >= 200;
            let mut row = Row::new(
                Claim::Official,
                if over { Mark::Consider } else { Mark::Keep },
                name,
            );
            row.value = format!(
                "{} line{}",
                count(size.lines as u64),
                if size.lines == 1 { "" } else { "s" }
            );
            row.sources = vec![CLAUDE_MEMORY_DOCS];
            row.evidence = if over {
                format!(
                    "Claude Code suggests under 200 lines per CLAUDE.md; length alone is not a quality score. {advice} Imports still load together."
                )
            } else {
                "Under the 200 lines Claude Code suggests per CLAUDE.md.".into()
            };
            row
        }
        Harness::Codex => {
            // The system and user configuration are read, and projects or named
            // profiles that set it leave it unresolved; command-line overrides are unseen.
            // A resolved budget is shared by the selected profiles; name every source of it.
            let mut sources: Vec<String> = vec![];
            for p in profiles.iter().filter(|p| c.profiles.contains(&p.id)) {
                let source = match &p.instruction_budget_file {
                    Some(file) => format!("{file} sets"),
                    None => "Codex loads by default".into(),
                };
                if !sources.contains(&source) {
                    sources.push(source);
                }
            }
            let limit = |budget: usize| {
                format!(
                    "the {} bytes {} (project_doc_max_bytes), counting headers and wrappers",
                    count(budget as u64),
                    sources.join(" and ")
                )
            };
            let unseen = "A command-line override is not recorded.";
            let mut row = match agents_budget(c, profiles) {
                Err(reason) => {
                    let mut row = Row::new(Claim::Official, Mark::Unknown, name);
                    row.evidence = not_enough(&reason);
                    row
                }
                // Rendered bytes include headers and wrappers, so only reaching the
                // budget can mean Codex truncated the source files.
                Ok(budget) if size.bytes >= budget => {
                    let mut row = Row::new(Claim::Official, Mark::Consider, name);
                    row.evidence = format!(
                        "At or above {}, so Codex may have cut it short. Trim or scope it to keep the important instructions. {unseen}",
                        limit(budget)
                    );
                    row
                }
                Ok(budget) => {
                    let mut row = Row::new(Claim::Official, Mark::Keep, name);
                    row.evidence = format!("Under {}. {unseen}", limit(budget));
                    row
                }
            };
            row.value = format!("{} bytes", count(size.bytes as u64));
            row.sources = vec![CODEX_AGENTS_DOCS];
            row
        }
    };
    row.amount = cost_amount(c.estimated_usd);
    row
}

/// The default report: every setting and feature under the kind of claim it rests on.
/// A check's verdict before its line is drawn.
///
/// A comparison with no requests is not a verdict, whatever the tie-break said. Window
/// coverage counts only changed parts, so zero there can still be a keep. Fast mode warns
/// from current settings alone, so it never needs requests.
fn check_action(result: &CheckResult, r: &Common<'_>) -> Action {
    let empty =
        matches!(result, CheckResult::Ttl(_) | CheckResult::Compaction(_)) && r.requests == 0;
    if empty { Action::Advisory } else { r.action }
}

/// A check line's mark. A measured background generation over the limit is advisory only
/// because its uses are not recorded, and auto memory is advised off on the maintainer's
/// opinion whatever it cost, so both are worth considering off. Other advisories lack evidence.
fn check_mark(result: &CheckResult, r: &Common<'_>) -> Mark {
    match check_action(result, r) {
        Action::Change => Mark::Change,
        Action::Keep => Mark::Keep,
        Action::Advisory => match result {
            CheckResult::Background(_) if r.savings_usd.is_some() => Mark::Consider,
            CheckResult::Memory(_) => Mark::Consider,
            _ => Mark::Unknown,
        },
    }
}

/// A line the report asks the person to act on.
pub(crate) struct Recommendation<'a> {
    /// The kind of claim it rests on: Measured, Official or Opinion.
    pub claim: &'static str,
    /// Worth considering, rather than a recommended change.
    pub suggested: bool,
    /// The line's name, such as "Main-agent prompt cache TTL".
    pub name: String,
    /// The current value and the one to change to, such as "5m -> 1h".
    pub value: String,
    pub source: Source<'a>,
}

/// What a recommendation's line reports on.
pub(crate) enum Source<'a> {
    Check(&'a CheckResult),
    Piece(&'a ContextCost),
}

/// Every change the summary recommends or suggests, in its order: by kind of claim,
/// recommended changes before suggestions.
pub(crate) fn recommendations(report: &Report, unicode: bool) -> Vec<Recommendation<'_>> {
    let out = Out::new(
        report,
        ReportOptions {
            unicode,
            ..ReportOptions::default()
        },
    );
    let arrow = out.arrow;
    let mut lines = vec![];
    for result in &report.checks {
        let r = common(result);
        let mark = check_mark(result, &r);
        if !matches!(mark, Mark::Change | Mark::Consider) {
            continue;
        }
        let value = |s: Option<Setting<'_>>| match s {
            Some(Setting::Window(n)) => tokens(n),
            other => setting_value(other),
        };
        // A suggestion is always to turn a feature off.
        let target = match mark {
            Mark::Change => value(r.recommended),
            _ => "off".into(),
        };
        lines.push((
            Claim::of(result),
            mark,
            Recommendation {
                claim: Claim::of(result).name(),
                suggested: mark == Mark::Consider,
                name: setting_name(result),
                value: format!("{} {arrow} {target}", value(r.current)),
                source: Source::Check(result),
            },
        ));
    }
    for c in setting_rows(&report.context) {
        let mark = match c.verdict {
            Some(Verdict::Off) => Mark::Change,
            Some(Verdict::Consider) => Mark::Consider,
            _ => continue,
        };
        let target = if typed_only(c.control.as_ref()) {
            "typed only"
        } else {
            "off"
        };
        lines.push((
            Claim::Measured,
            mark,
            Recommendation {
                claim: Claim::Measured.name(),
                suggested: mark == Mark::Consider,
                name: short_label(c, &out),
                value: format!("on {arrow} {target}"),
                source: Source::Piece(c),
            },
        ));
    }
    // A stable sort keeps the report's order within each claim and mark.
    lines.sort_by_key(|(claim, mark, _)| (Claim::ALL.iter().position(|c| c == claim), *mark));
    lines.into_iter().map(|(_, _, line)| line).collect()
}

fn summary(report: &Report, out: &mut Out<'_>) {
    let arrow = out.arrow;
    let coverage = &report.scan.coverage;
    let requests = &report.scan.requests;
    let since = report.scan.since.or(requests.first().map(|r| r.time));
    let until = generated_at(report);
    let many = report.scan.profiles.len() > 1;
    out.title();
    t3_coverage(report, out);
    out.line(
        "Estimated with public API prices, not your subscription bill. Nothing was changed.",
        0,
        PLAIN,
    );
    out.blank();
    out.line(
        &match since {
            Some(since) if coverage.requests > 0 => format!(
                "Read {} requests from {} to {} ({}):",
                count(coverage.requests),
                date(&iso(since)),
                date(&report.generated_at),
                days(Some(since), Some(until))
            ),
            _ => format!(
                "No requests found{} in:",
                report
                    .scan
                    .since
                    .map_or_else(String::new, |s| format!(" since {}", date(&iso(s))))
            ),
        },
        0,
        PLAIN,
    );
    for p in &report.scan.profiles {
        out.line(&out.short_profile(&p.id), 2, PLAIN);
    }
    for group in &report.historical {
        if !group.tier_sensitivity.is_empty() {
            out.line(&format!(
                "Conditional Codex pricing [{}]: {} of {} requests priced; {} use assumed service tiers (supported historical requests, otherwise standard for omitted/auto). {}",
                group.profiles.iter().map(|p| out.short_profile(p)).collect::<Vec<_>>().join(", "),
                group.priced_requests, group.requests, group.conditional_requests, TIER_SENSITIVITY
            ), 0, PLAIN);
        }
    }
    if coverage.missing_usage > 0 {
        out.line(
            &format!(
                "{} requests have no recorded usage and are left out of comparisons.",
                count(coverage.missing_usage)
            ),
            0,
            PLAIN,
        );
    }
    if coverage.skipped_lines > 0 {
        out.line(
            &format!(
                "{} unreadable transcript lines in {} files were skipped.",
                count(coverage.skipped_lines),
                count(coverage.skipped_files)
            ),
            0,
            PLAIN,
        );
    }
    if report.scan.snapshots.is_empty() {
        out.line("No transcript histories found. Select a profile with --claude-dir or --codex-dir after using that agent.", 0, PLAIN);
    } else if coverage.requests == 0 {
        out.line(
            "Try an earlier --since or select another profile.",
            0,
            PLAIN,
        );
    }

    let checks: Vec<(&CheckResult, Common<'_>)> =
        report.checks.iter().map(|c| (c, common(c))).collect();
    // A check for a working directory that no longer exists has no setting left
    // to change; show it only when it is the sole result for its setting.
    let key = |(result, r): &(&CheckResult, Common<'_>)| {
        let models = match result {
            CheckResult::Compaction(c) => c
                .models
                .iter()
                .map(|m| m.as_deref().unwrap_or(""))
                .collect::<Vec<_>>()
                .join(","),
            _ => String::new(),
        };
        format!("{}:{}:{}:{models}", r.check, r.bucket, r.profile)
    };
    let action = |(result, r): &(&CheckResult, Common<'_>)| check_action(result, r);
    let decided: crate::collections::HashSet<String> = checks
        .iter()
        .filter(|c| action(c) != Action::Advisory)
        .map(key)
        .collect();
    // Only a vanished directory is proven irrelevant; any other unresolved owner,
    // such as a project override, stays visible beside the resolved verdict.
    let checks: Vec<(&CheckResult, Common<'_>)> = checks
        .into_iter()
        .filter(|c| {
            !(action(c) == Action::Advisory
                && matches!(c.0, CheckResult::Speed(s) if s.reason.contains("working directory no longer exists"))
                && decided.contains(&key(c)))
        })
        .collect();
    let mark = |(result, r): &(&CheckResult, Common<'_>)| check_mark(result, r);
    // Lines name a profile briefly, by T3 instance or root, unless two would read the same.
    let mut tags: IndexMap<&str, String> = report
        .scan
        .profiles
        .iter()
        .map(|p| {
            let tag = match &p.t3 {
                Some(t) => format!("T3 {}", t.instance),
                None => out.home(&p.root),
            };
            (p.id.as_str(), tag)
        })
        .collect();
    let mut seen = crate::collections::HashMap::<String, usize>::default();
    for tag in tags.values() {
        *seen.entry(tag.clone()).or_default() += 1;
    }
    for (id, tag) in &mut tags {
        if seen[tag.as_str()] > 1 {
            *tag = out.short_profile(id);
        }
    }
    let tag = |profile: &str, out: &Out<'_>| {
        if !many {
            return String::new();
        }
        let tag = tags
            .get(profile)
            .cloned()
            .unwrap_or_else(|| out.short_profile(profile));
        format!("[{tag}]")
    };
    let pieces: Vec<&ContextCost> = setting_rows(&report.context)
        .filter(|c| c.verdict.is_some())
        .collect();
    let off: Vec<&ContextCost> = pieces
        .iter()
        .copied()
        .filter(|c| c.verdict == Some(Verdict::Off))
        .collect();
    let total = checks.iter().filter(|c| mark(c) == Mark::Change).count() + off.len();
    out.blank();
    out.line(
        &if total > 0 {
            format!(
                "{total} setting change{} recommended.",
                if total == 1 { "" } else { "s" }
            )
        } else {
            "No setting changes recommended.".into()
        },
        0,
        BOLD,
    );
    out.line(
        &format!(
            "Savings are per 30 days at this window's rate; costs and uses are totals over the whole window. A tool, skill or MCP server that was never used is recommended off, and one that cost more than {} per use (--max-cost-per-use) is worth considering off.",
            about(report.max_cost_per_use_usd)
        ),
        0,
        PLAIN,
    );
    if report
        .checks
        .iter()
        .any(|c| matches!(c, CheckResult::Background(_)))
    {
        out.line("Background estimates apply current files to history; historical settings and unrecorded launch overrides remain unknown.", 0, PLAIN);
    }
    // Parent and detail coverage overlap; do not combine them into a percentage.
    if pieces.iter().any(|c| c.priced < c.requests) {
        out.line("Some feature costs are scaled from their priced requests; --details shows each row's coverage.", 0, PLAIN);
    }
    if out.options.color {
        out.legend();
    }
    let incomplete = incomplete_profiles(&report.scan.snapshots);
    // A server's own loading and exposure controls, after its line's evidence.
    let server_notes = |c: &ContextCost| {
        if c.kind != ContextKind::McpServer {
            return String::new();
        }
        context_guidance(c, &report.scan.profiles, &c.name)
            .iter()
            .map(|n| format!(" {n}"))
            .collect::<String>()
    };
    // What long foreground Bash waits cost: a total over the window, like their count.
    let waits = |b: &BashCapCheck| {
        let w = &b.waits;
        if w.long == 0 {
            return "No foreground Bash wait outlasted the cache.".to_string();
        }
        let cost = &w.miss_cost;
        let cost = match cost.total_usd {
            Some(usd) => about(usd.to_f64()),
            None if cost.known_usd > Usd::ZERO => {
                format!("at least {}", about(cost.known_usd.to_f64()))
            }
            None => "an unknown amount".into(),
        };
        format!(
            "{} foreground Bash wait{} outlasted the cache ({} hit the timeout, {} polled in a loop); the cache misses cost {cost} over the window.",
            count(w.long),
            if w.long == 1 { "" } else { "s" },
            count(w.timed_out),
            count(w.polling)
        )
    };
    let check_row = |c: &(&CheckResult, Common<'_>), out: &Out<'_>| {
        let (result, r) = c;
        let mark = mark(c);
        let mut row = Row::new(Claim::of(result), mark, setting_name(result));
        row.profile = tag(r.profile, out);
        let current = match r.current {
            Some(Setting::Window(n)) => tokens(n),
            other => setting_value(other),
        };
        row.value = current.clone();
        if let CheckResult::Compaction(c) = result
            && c.unset
        {
            row.value += " (default)";
        }
        if mark == Mark::Change {
            let recommended = match r.recommended {
                Some(Setting::Window(n)) => tokens(n),
                other => setting_value(other),
            };
            row.value = format!("{} {arrow} {recommended}", row.value);
        }
        let saves = |usd: f64| {
            Some((
                "saves",
                match r.monthly_savings_usd {
                    None => per_month(usd, r.since, r.until),
                    Some(m) => about(m),
                },
            ))
        };
        if mark == Mark::Change {
            row.amount = r.savings_usd.and_then(saves);
        }
        let set_in = format!("Set in {}.", out.home(r.owner));
        let unknown = || {
            format!(
                "{}{}{} {set_in}",
                not_enough(&unresolved(result, r)),
                context_sizes(report, result, r.profile),
                context_rot(report, result)
            )
        };
        row.evidence = match result {
            CheckResult::Ttl(t) => match mark {
                Mark::Unknown => unknown(),
                Mark::Keep if t.one_hour_savings_usd == 0.0 => {
                    format!("5m and 1h cost the same on replay. {set_in}")
                }
                _ => {
                    if mark == Mark::Keep {
                        row.amount = Some((
                            "saves",
                            per_month(t.one_hour_savings_usd.abs(), r.since, r.until),
                        ));
                    }
                    let other = if r.current == Some(Setting::Ttl(Ttl::OneHour)) {
                        "5m"
                    } else {
                        "1h"
                    };
                    let better = if mark == Mark::Keep {
                        format!("{} costs less than {other}", setting_value(r.current))
                    } else {
                        format!("{} costs less", setting_value(r.recommended))
                    };
                    format!(
                        "Replayed at 5m and 1h, assuming the same sessions{}; {better}. {set_in}",
                        coverage_note(r)
                    )
                }
            },
            CheckResult::Compaction(c) => {
                let mut text = match c.setting {
                    Some(setting) => format!(
                        "Set to {} tokens, which the model's context window caps at {current}. ",
                        count(setting),
                    ),
                    None => String::new(),
                };
                text += &match mark {
                    Mark::Change => {
                        let added = c
                            .scenarios
                            .iter()
                            .find(|s| r.recommended == Some(Setting::Window(s.window)))
                            .map_or(0, |s| s.added.len());
                        let net = match added {
                            0 => String::new(),
                            n => format!(
                                ", net of {} added summar{} and their resumed prefixes",
                                count(n as u64),
                                if n == 1 { "y" } else { "ies" }
                            ),
                        };
                        let smaller = matches!(
                            (r.current, r.recommended),
                            (Some(Setting::Window(a)), Some(Setting::Window(b))) if b < a
                        );
                        format!(
                            "Replayed assuming the same sessions{}{net}. {}{} {set_in}",
                            coverage_note(r),
                            if smaller {
                                "Detail lost to summaries is not measured."
                            } else {
                                "How work changes at longer contexts is not measured."
                            },
                            context_rot(report, result)
                        )
                    }
                    Mark::Keep => format!(
                        "No other window was cheaper, replayed assuming the same sessions.{} {set_in}",
                        context_rot(report, result)
                    ),
                    _ => unknown(),
                };
                text
            }
            CheckResult::Idle(_) => {
                let on = |s: Option<Setting<'_>>| s == Some(Setting::Idle(true));
                let loss = " A summary drops detail, which is not measured.";
                match mark {
                    Mark::Change => format!(
                        "Replayed on and off, assuming the same sessions{}.{} {set_in}",
                        coverage_note(r),
                        if on(r.recommended) { loss } else { "" }
                    ),
                    Mark::Keep if on(r.current) => format!(
                        "Summarizing idle sessions cost less than re-reading them.{loss} {set_in}"
                    ),
                    Mark::Keep => {
                        format!("Summarizing idle sessions would not have saved money. {set_in}")
                    }
                    _ => unknown(),
                }
            }
            CheckResult::T3Idle(_) => match mark {
                Mark::Change => format!(
                    "Replayed under each mode, assuming the same sessions{}.{} {set_in}",
                    coverage_note(r),
                    if r.recommended == Some(Setting::T3Idle(crate::model::T3IdleMode::Off)) {
                        ""
                    } else {
                        " A summary drops detail, which is not measured."
                    }
                ),
                Mark::Keep => {
                    format!("Replayed under each mode, the current one costs least. {set_in}")
                }
                _ => unknown(),
            },
            CheckResult::BashCap(b) => {
                let ttl = b.ttl.map_or("unknown", Ttl::as_str);
                match mark {
                    Mark::Change => format!(
                        "A wait that long outlasts the {ttl} cache. {} Commands past the cap keep running in the background; what checking on them costs is not measured.{} {set_in}",
                        waits(b),
                        if b.replacement.contains("BASH_DEFAULT_TIMEOUT_MS") {
                            " Lower BASH_DEFAULT_TIMEOUT_MS to match."
                        } else {
                            ""
                        }
                    ),
                    Mark::Keep => format!("Under the {ttl} cache TTL. {} {set_in}", waits(b)),
                    _ => unknown(),
                }
            }
            CheckResult::Keepalive(k) => match mark {
                Mark::Keep => keepalive_keep(k, r),
                // A priced result is no verdict only because no setting caps pings.
                _ if r.recommended.is_some() => {
                    row.mark = Mark::Info;
                    keepalive_unresolved(k, r)
                }
                _ => not_enough(&keepalive_unresolved(k, r)),
            },
            CheckResult::Background(b) => {
                let directories: Vec<String> = b
                    .directories
                    .iter()
                    .map(|d| {
                        d.as_ref()
                            .map_or_else(|| "directory absent".into(), |d| out.home(d))
                    })
                    .collect();
                let mut text = if directories.is_empty() {
                    String::new()
                } else {
                    format!("In {}. ", directories.join(", "))
                };
                if r.current == Some(Setting::Toggle(true)) {
                    row.amount = r
                        .savings_usd
                        .filter(|usd| *usd > 0.0)
                        .and_then(|usd| cost_amount(Some(usd)));
                }
                let off = background_off(b, out);
                text += &match mark {
                    Mark::Change => format!(
                        "Estimated where it would have run{}; its uses are not recorded. {off}.",
                        coverage_note(r)
                    ),
                    Mark::Consider => format!(
                        "Estimated where it would have run{}; its uses are not recorded, so it may still cost less than {} per use. {off} to turn it off.",
                        coverage_note(r),
                        about(report.max_cost_per_use_usd)
                    ),
                    Mark::Keep if r.current != Some(Setting::Toggle(true)) => {
                        format!("Costs nothing. {set_in}")
                    }
                    Mark::Keep if b.generations == Some(0) => format!(
                        "No recorded interactive terminal turn would have run it, so it cost nothing. {off} to turn it off."
                    ),
                    Mark::Keep => format!(
                        "Estimated where it would have run{}; its whole cost is under {}, so it costs no more than that per use. {off} to turn it off.",
                        coverage_note(r),
                        about(report.max_cost_per_use_usd)
                    ),
                    _ if r.current.is_none() => format!(
                        "{} {} Candidate owner: {}. Verify ownership before using: {off}.",
                        not_enough(background_unknown(b)),
                        b.reason,
                        out.home(r.owner)
                    ),
                    _ => format!(
                        "{} {off} to turn it off.",
                        not_enough(background_unknown(b))
                    ),
                };
                text
            }
            CheckResult::Memory(m) => {
                // Codex extraction and consolidation are never measured, so a total from
                // a window where memories may have run is a lower bound.
                let unmeasured = codex_unmeasured(m);
                let at_least = r.coverage.is_some() || unmeasured;
                let mut notes = vec![];
                row.amount = match r.savings_usd {
                    Some(0.0) => None,
                    Some(usd) => Some((
                        "costs",
                        format!("{}{}", if at_least { "at least " } else { "" }, about(usd)),
                    )),
                    None => cost_amount(None),
                };
                match r.coverage {
                    Some(c) if r.savings_usd.is_some() && c.priced == 0 => notes.push(
                        "from memory's reads and writes alone; no request recorded its context"
                            .to_string(),
                    ),
                    Some(c) if r.savings_usd.is_some() => notes.push(format!(
                        "from the {}% of requests that recorded their context",
                        percent(c.priced, c.total)
                    )),
                    _ => {}
                }
                if unmeasured {
                    notes.push(if r.savings_usd == Some(0.0) && r.coverage.is_none() {
                        "no measured cost, but extraction and consolidation are not measured".into()
                    } else {
                        "extraction and consolidation are not measured".into()
                    });
                }
                let measured = if mark == Mark::Keep && memory_free(m) && !unmeasured {
                    "costs nothing".to_string()
                } else {
                    memory_uses(m)
                };
                notes.insert(0, measured);
                let mut text = format!("{MEMORY_ADVICE}. Measured: {}.", notes.join("; "));
                if mark != Mark::Keep {
                    if m.unresolved.is_some() {
                        row.value = "may be on".into();
                        text = format!("{}. {text}", memory_unresolved(m));
                    }
                    let _ = write!(text, " {}.", memory_off(m, out));
                }
                text
            }
            CheckResult::Speed(s) => {
                let measured = match mark {
                    Mark::Change if r.savings_usd.is_some() => {
                        "The premium is measured at API rates for the same work.".to_string()
                    }
                    Mark::Change => capitalize(&s.reason) + ".",
                    Mark::Keep => String::new(),
                    _ => not_enough(&unresolved(result, r)),
                };
                [format!("{FAST_ADVICE}."), measured, set_in]
                    .into_iter()
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        };
        row
    };
    let piece_row = |c: &ContextCost, out: &Out<'_>| {
        let typed = typed_only(c.control.as_ref());
        let verdict = c.verdict.expect("pieces have verdicts");
        let mark = match verdict {
            Verdict::Off => Mark::Change,
            Verdict::Consider => Mark::Consider,
            Verdict::Keep => Mark::Keep,
        };
        let mut row = Row::new(Claim::Measured, mark, short_label(c, out));
        row.profile = tag(first_profile(c), out);
        row.amount = cost_amount(c.estimated_usd);
        row.value = match verdict {
            Verdict::Off if typed => format!("on {arrow} typed only"),
            Verdict::Off => format!("on {arrow} off"),
            _ => "on".into(),
        };
        let each = c
            .cost_per_use_usd
            .map_or_else(String::new, |usd| format!(", {} each", about(usd)));
        let mut text = format!("{}{each}", capitalize(&uses(c)));
        if c.skill.is_some() {
            let _ = write!(text, "; {}", catalog_evidence(c));
        }
        text.push('.');
        // The two reasons context verdicts leave nonuse unproven outside the Codex catalog.
        if verdict == Verdict::Consider && c.skill.is_none() && c.calls == Some(0) {
            text += if c.profiles.iter().any(|p| incomplete.contains(p)) {
                " An unreadable transcript line could hold a use."
            } else {
                " It was first sent during the window, so the window cannot rule out a use."
            };
        }
        if c.kind == ContextKind::McpTool {
            text += " Included in its server's cost.";
        }
        let control = match (&c.control, c.skill.is_some()) {
            (_, true) => catalog_control(c),
            (Some(control), _) if typed => format!(
                "Hide it from the model: {}",
                control_text(control, c.harness)
            ),
            (Some(control), _) => format!("Turn off: {}", control_text(control, c.harness)),
            (None, _) => String::new(),
        };
        if !control.is_empty() {
            let _ = write!(text, " {control}.");
        }
        text += &server_notes(c);
        row.evidence = text;
        row
    };
    let mut rows: Vec<Row> = checks.iter().map(|c| check_row(c, out)).collect();
    rows.extend(pieces.iter().map(|c| piece_row(c, out)));
    // Switchable context still sent today without enough pricing or use evidence for a
    // verdict. Auto memory has its own line.
    for c in setting_rows(&report.context).filter(|c| {
        c.current && c.control.is_some() && c.verdict.is_none() && c.kind != ContextKind::Memory
    }) {
        let mut row = Row::new(Claim::Measured, Mark::Unknown, short_label(c, out));
        row.profile = tag(first_profile(c), out);
        row.value = "on".into();
        row.amount = c.estimated_usd.and_then(|usd| cost_amount(Some(usd)));
        let uses = uses(c);
        let mut text = not_enough(&match c.estimated_usd {
            None => format!("None of its requests could be priced; {uses}."),
            Some(_) if c.tier_unknown_requests > 0 => format!(
                "{} requests have unresolved tier evidence ({} priced using assumed tiers), so tier sensitivity could reverse keep or off; {uses}.",
                c.tier_unknown_requests, c.conditional_requests
            ),
            Some(_) => "Its recorded uses are unknown, so keep or off is unresolved.".into(),
        });
        if c.kind == ContextKind::McpTool {
            text += " Included in its server's cost.";
        }
        if c.skill.is_some() {
            let _ = write!(
                text,
                " {}. {}.",
                capitalize(&catalog_evidence(c)),
                catalog_control(c)
            );
        } else {
            let _ = write!(
                text,
                " Turn off: {}.",
                control_text(c.control.as_ref().expect("switchable context"), c.harness)
            );
        }
        text += &server_notes(c);
        row.evidence = text;
        rows.push(row);
    }
    // Tools inside a server that the lines above leave out: those its server's off
    // covers, and those without a switch of their own.
    for (server, c) in report
        .context
        .iter()
        .filter(|c| c.current)
        .flat_map(|server| {
            server
                .details
                .iter()
                .filter(|d| d.current)
                .map(move |d| (server, d))
        })
    {
        let covered = server.verdict == Some(Verdict::Off);
        if !covered && (c.verdict.is_some() || c.control.is_some()) {
            continue;
        }
        let mut row = Row::new(Claim::Measured, Mark::Info, short_label(c, out));
        row.profile = tag(first_profile(c), out);
        row.amount = cost_amount(c.estimated_usd);
        let reason = if covered {
            " Turning off its server covers it."
        } else if server.harness == Harness::Codex && server.name == "codex_apps" {
            " The app id mapping is unavailable, so no per-app tool switch is generated."
        } else if c.harness == Harness::Codex && c.kind == ContextKind::McpTool {
            " Its raw tool identity or owning server is unresolved, so no per-tool switch is generated."
        } else {
            ""
        };
        row.evidence = format!(
            "{}. Included in its server's cost.{reason}",
            capitalize(&uses(c))
        );
        rows.push(row);
    }
    for g in report
        .scan
        .t3
        .iter()
        .filter_map(|s| s.text_generation.as_ref())
    {
        let mut row = Row::new(Claim::Measured, Mark::Unknown, "T3 text generation".into());
        row.value = format!("{} {}", g.selection.instance, g.selection.model);
        let mut text = format!(
            "{} ({}). Set in {}.",
            not_enough(
                "T3 writes thread titles, branch names, PR text and other generated text in ephemeral sessions that keep no native records"
            ),
            g.resolution,
            out.home(&g.owner)
        );
        for (key, s) in &g.overrides {
            let _ = write!(text, " {key} replaces it with {} {}.", s.instance, s.model);
        }
        if g.project_overrides > 0 {
            let _ = write!(
                text,
                " {} project{} replace these selections in projectSettingsOverrides.",
                count(g.project_overrides),
                if g.project_overrides == 1 { "" } else { "s" }
            );
        }
        row.evidence = text;
        rows.push(row);
    }
    for c in report
        .context
        .iter()
        .filter(|c| c.current && c.kind == ContextKind::Instructions)
    {
        let mut row = instruction_row(c, &report.scan.profiles, out);
        row.profile = tag(first_profile(c), out);
        rows.push(row);
    }
    let loading = mcp_guidance(report.context.iter().filter(|c| c.current));
    if !loading.is_empty() {
        let mut row = Row::new(Claim::Official, Mark::Info, "MCP tool loading".into());
        let notes: IndexSet<&str> = loading.iter().flat_map(|g| g.notes.clone()).collect();
        row.evidence = notes.into_iter().collect::<Vec<_>>().join(" ");
        row.sources = loading.iter().filter_map(|g| g.source).collect();
        rows.push(row);
    }
    // Tool output, one line per cap setting, with tools no cap governs together.
    let mut outputs: IndexMap<(&str, Option<Cap>), Vec<&ContextCost>> = IndexMap::default();
    for c in report
        .context
        .iter()
        .filter(|c| c.kind == ContextKind::ToolOutput)
    {
        outputs
            .entry((first_profile(c), output_cap(report, c)))
            .or_default()
            .push(c);
    }
    // Capped settings first, in the order their most expensive output appeared.
    let order: Vec<_> = outputs
        .keys()
        .filter(|k| k.1.is_some())
        .chain(outputs.keys().filter(|k| k.1.is_none()))
        .cloned()
        .collect();
    for key in order {
        let group = &outputs[&key];
        let usd: Vec<Option<f64>> = group.iter().map(|c| c.estimated_usd).collect();
        let (figure, unpriced) = known_figure(&usd);
        let largest = group.iter().filter_map(|c| c.largest_characters).max();
        let largest = largest.map_or_else(String::new, |n| {
            format!("Its largest result was {} characters. ", count(n))
        });
        let mut row = match &key.1 {
            // A cap is a setting whose cost transcripts cannot weigh.
            Some(cap) => {
                let mut row = Row::new(Claim::Measured, Mark::Unknown, cap.label.into());
                row.value = cap.value.clone();
                row.evidence = format!(
                    "{} {largest}{unpriced}{}.",
                    not_enough(
                        "A lower cap keeps less output in context but leaves the model to read the rest back, which transcripts cannot price."
                    ),
                    cap.text
                );
                row
            }
            None => {
                let names: Vec<&str> = group.iter().take(4).map(|c| c.name.as_str()).collect();
                let more = group.len() - names.len();
                let more = if more > 0 {
                    format!(" and {} more", count(more as u64))
                } else {
                    String::new()
                };
                let mut row = Row::new(Claim::Measured, Mark::Info, "Other tool output".into());
                row.evidence = format!(
                    "From {}{more}. {largest}{unpriced}No cap is checked for it.",
                    names.join(", ")
                );
                row
            }
        };
        row.amount = Some(("costs", figure));
        row.profile = tag(key.0, out);
        rows.push(row);
    }
    for c in &report.subagents {
        let mut row = Row::new(Claim::Measured, Mark::Info, subagent_name(c));
        row.profile = tag(&c.profiles[0], out);
        row.amount = Some(("costs", scaled(&c.cost, &about)));
        row.evidence = subagent_line(c, &about, out);
        rows.push(row);
    }
    // Context with no switch: worth seeing, but not a setting to change.
    let fixed: Vec<&ContextCost> = report
        .context
        .iter()
        .filter(|c| {
            c.current
                && c.control.is_none()
                && !matches!(c.kind, ContextKind::ToolOutput | ContextKind::Instructions)
                && c.estimated_usd != Some(0.0)
        })
        .collect();
    for c in fixed
        .iter()
        .filter(|c| matches!(c.kind, ContextKind::Skill | ContextKind::McpServer))
    {
        let mut row = Row::new(Claim::Measured, Mark::Info, short_label(c, out));
        row.profile = tag(first_profile(c), out);
        row.amount = cost_amount(c.estimated_usd);
        row.evidence = match c.kind {
            ContextKind::Skill if c.skill.is_some() => {
                format!(
                    "{}. {}.",
                    capitalize(&catalog_evidence(c)),
                    catalog_control(c)
                )
            }
            ContextKind::Skill => format!(
                "{}. No setting turns it off: {}.",
                capitalize(&uses(c)),
                decapitalize(PLUGIN_SKILL)
            ),
            _ if c.harness == Harness::Codex && c.name == "codex_apps" => {
                server_notes(c).trim_start().to_string()
            }
            _ => format!(
                "Configure the plugin or launcher that supplies it; no unique local server switch was resolved.{}",
                server_notes(c)
            ),
        };
        rows.push(row);
    }
    let core: Vec<Option<f64>> = fixed
        .iter()
        .filter(|c| !matches!(c.kind, ContextKind::Skill | ContextKind::McpServer))
        .map(|c| c.estimated_usd)
        .collect();
    if !core.is_empty() {
        let (figure, unpriced) = known_figure(&core);
        let mut row = Row::new(
            Claim::Measured,
            Mark::Info,
            "System prompt and core tools".into(),
        );
        row.amount = Some(("costs", figure));
        row.evidence = format!(
            "{unpriced}Includes core tools such as Bash and Read, and the skill and subagent lists. No setting turns these off."
        );
        rows.push(row);
    }
    rows_section(&rows, out);
    if !report.subagents.is_empty() {
        out.blank();
        out.line(SUBAGENT_EVIDENCE, 0, PLAIN);
    }
    catalog_guidance(report, out);
    if since.is_some_and(|s| until > s) && !pieces.is_empty() {
        out.line(
            "Context costs assume nothing else changes when a piece is removed; cache writes and compaction can shift.",
            0,
            PLAIN,
        );
    }
    out.blank();
    let fetched = report
        .prices
        .fetched_at
        .as_deref()
        .filter(|f| !f.is_empty())
        .map_or_else(String::new, |f| format!(" ({})", date(f)));
    out.line(
        &format!(
            "Prices: {} from {}{fetched}.",
            if report.prices.offline {
                "cached"
            } else {
                "fetched"
            },
            out.home(&report.prices.source)
        ),
        0,
        PLAIN,
    );
    out.line(
        "Each line compares one setting with everything else unchanged; savings do not add up.",
        0,
        PLAIN,
    );
    out.line(
        "--details shows the evidence and assumptions behind each line.",
        0,
        PLAIN,
    );
}

/// What every piece of context cost, including pieces no longer sent.
fn context_section(report: &Report, out: &mut Out<'_>) {
    if report.context.is_empty() {
        return;
    }
    let since = report
        .scan
        .since
        .or(report.scan.requests.first().map(|r| r.time));
    let until = generated_at(report);
    let span = since.map(|since| ((until - since) as f64).max(DAY));
    let many = report.scan.profiles.len() > 1;
    out.section("What each piece of context costs");
    out.line(
        &format!(
            "Priced over the requests that carried it in the same window. Tool and discovery detail costs are included in their server total. Recommended off when never used, and worth considering off when it cost more than {} per use.",
            about(report.max_cost_per_use_usd)
        ),
        0,
        PLAIN,
    );
    let guidance = mcp_guidance(report.context.iter());
    let notes: IndexSet<&str> = guidance
        .iter()
        .flat_map(|g| g.notes.iter().copied().chain(g.source))
        .collect();
    for note in notes {
        out.line(note, 2, PLAIN);
    }
    for (c, covered) in report.context.iter().flat_map(|parent| {
        std::iter::once((parent, false)).chain(
            parent
                .details
                .iter()
                .map(|d| (d, parent.verdict == Some(Verdict::Off))),
        )
    }) {
        let first = first_profile(c);
        // Unpriced requests are scaled from the priced ones, never counted as free.
        let cost = match c.estimated_usd {
            None => "cost unknown".to_string(),
            Some(usd) => format!(
                "{}{}{}",
                about(usd),
                span.map_or_else(String::new, |span| format!(
                    " (about {} per 30 days)",
                    about((usd * 30.0 * DAY) / span)
                )),
                if c.priced < c.requests {
                    format!(
                        ", estimated from the {} of {} requests that could be priced",
                        count(c.priced),
                        count(c.requests)
                    )
                } else {
                    String::new()
                }
            ),
        };
        let used = match c.calls {
            None => String::new(),
            Some(results) if c.kind == ContextKind::ToolOutput => format!(
                "; {} result{} in the window, the largest {} characters",
                count(results),
                if results == 1 { "" } else { "s" },
                count(c.largest_characters.unwrap_or(0))
            ),
            _ if c.skill.is_some() => format!(
                "; {} observed skill turns; {}",
                count(c.calls.unwrap_or(0)),
                catalog_evidence(c)
            ),
            Some(0) => "; no recorded use".into(),
            Some(calls) => format!(
                "; used {} time{}, last {}",
                count(calls),
                if calls == 1 { "" } else { "s" },
                date(&iso(c.last_used.unwrap_or(0)))
            ),
        };
        let profile = if many {
            format!(" [{}]", out.profile_name(first))
        } else {
            String::new()
        };
        let text = format!("{}{profile}: {cost}{used}", piece_label(c, out));
        out.line(&text, 2, feature_tier(c, covered));
        for note in context_guidance(c, &report.scan.profiles, &out.home(&c.name)) {
            out.line(&note, 6, PLAIN);
        }
        out.notes("Pricing assumption", &c.assumptions);
        let size = c.tokens.map_or_else(String::new, |tokens| {
            format!("{} tokens in {} requests", count(tokens), count(c.requests))
        });
        let unpriced = if c.priced < c.requests {
            format!(
                "; {} requests could not be priced",
                count(c.requests - c.priced)
            )
        } else {
            String::new()
        };
        out.line(
            &format!(
                "{size}{unpriced}{}",
                if c.current { "" } else { "; no longer sent" }
            ),
            6,
            PLAIN,
        );
        // Auto memory's row names the setting its check resolved, whatever owns it.
        let memory = report
            .scan
            .profiles
            .iter()
            .rev()
            .find(|p| p.id == first)
            .and_then(|p| p.memory.as_ref())
            .filter(|_| c.kind == ContextKind::Memory);
        let text = match &c.control {
            _ if covered => "Covered by the server-off recommendation".into(),
            Some(_) if memory.is_some() => {
                let m = memory.expect("checked above");
                match &m.off_file {
                    Some(file) => format!("Turn off: {} in {}", m.off, out.home(file)),
                    None => format!("Turn off: {}", m.off),
                }
            }
            Some(control) => {
                let file = report
                    .scan
                    .profiles
                    .iter()
                    .rev()
                    .find(|p| p.id == first)
                    .map_or_else(|| "settings.json".into(), |p| out.home(&p.settings_file));
                format!(
                    "{}: {} in {file}",
                    match c.verdict {
                        Some(Verdict::Off) => "Recommended off",
                        Some(Verdict::Consider) => "Consider turning off",
                        _ => "Turn off",
                    },
                    control_text(control, c.harness)
                )
            }
            None if c.kind == ContextKind::Instructions => {
                "Turn off: trim or scope this file".into()
            }
            None if c.skill.is_some() => catalog_control(c),
            None if c.kind == ContextKind::Skill => PLUGIN_SKILL.into(),
            None if c.kind == ContextKind::ToolOutput => match output_cap(report, c) {
                Some(cap) => format!("Capped by {}", cap.text),
                None => "No cap is checked for it".into(),
            },
            None if c.kind == ContextKind::McpTool && c.harness == Harness::Codex => {
                "No per-tool block is generated; see the server's control-mapping limits".into()
            }
            None if c.kind == ContextKind::McpServer && c.harness == Harness::Codex => {
                "Configure the plugin or launcher that supplies it; no unique local server switch was resolved".into()
            }
            None => "No setting turns this off".into(),
        };
        out.line(&text, 6, PLAIN);
    }
    catalog_guidance(report, out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use crate::reader::scan;
    use crate::replay::{AnalyzeOptions, analyze};
    use crate::test_support::{
        Workspace, make_profile, make_request, prices, report_line, report_lines,
    };

    const NOV1: Millis = 1_793_491_200_000;
    const DAY_MS: Millis = 86_400_000;
    const ROOT: &str = "/synthetic/home/.claude";

    fn cost(total: Option<f64>, known: f64) -> Cost {
        Cost {
            known_usd: Usd::from_f64(known),
            total_usd: total.map(Usd::from_f64),
            unknown: if total.is_none() {
                vec!["synthetic unknown charge".into()]
            } else {
                vec![]
            },
            assumptions: vec!["synthetic price assumption".into()],
        }
    }

    fn check_base<V>(action: Action, current: Option<V>, recommended: Option<V>) -> CheckBase<V> {
        CheckBase {
            profile: "profile".into(),
            history_profiles: vec!["profile".into()],
            evidence: Evidence::Measured,
            action,
            current,
            recommended,
            owner: format!("{ROOT}/settings.json:promptCacheTtl"),
            since: Some(NOV1),
            until: Some(NOV1 + 30 * DAY_MS),
            window_basis: "explicit --since".into(),
            baseline: "current settings applied to history".into(),
            requests: 2000,
            coverage: Some(Coverage {
                priced: 1990,
                total: 2000,
            }),
            savings_usd: Some(150.0),
            monthly_savings_usd: Some(150.0),
            break_even: None,
            assumptions: vec!["synthetic assumption".into()],
            uncertainty: vec!["synthetic uncertainty".into()],
            sources: vec!["https://example.invalid/source".into()],
        }
    }

    fn ttl_check(bucket: Bucket, base: CheckBase<Ttl>, one_hour: f64) -> CheckResult {
        CheckResult::Ttl(TtlCheck {
            base,
            window_basis_kind: WindowBasisKind::Explicit,
            bucket,
            one_hour_savings_usd: one_hour,
            cost: PerTtl {
                five_minutes: cost(Some(300.0), 300.0),
                one_hour: cost(Some(150.0), 150.0),
            },
            summary_cost: PerTtl::default(),
            comparison_cost: PerTtl {
                five_minutes: cost(Some(200.125), 200.125),
                one_hour: cost(None, 9.87),
            },
            fixed_cost: cost(None, 0.0),
            usage_estimates: vec![UsageEstimate {
                request: "synthetic-attempt".into(),
                evidence: EstimateEvidence::Unknown,
                basis: vec![],
                part: None,
                max_output_tokens: Some(2048),
                reason: "no supported input estimate",
                assumptions: vec!["synthetic attempt assumption".into()],
            }],
            cache_evidence: vec![CacheReplayEvidence {
                request: "synthetic-cache-request".into(),
                part: 0,
                ttl: Ttl::OneHour,
                state: KnownState::Unknown,
                predecessors: vec![CachePredecessor {
                    request: "synthetic-cache-predecessor".into(),
                    part: 0,
                    time: 0,
                    state: KnownState::Unknown,
                    prefix_evidence: PrefixEvidence::Unknown,
                    cohort_known: false,
                    reason: "synthetic witness reason",
                }],
                outcome: ReplayOutcome::Unknown,
                reason: "synthetic unresolved predecessor",
            }],
            sensitivity: TtlSensitivity {
                cold_prefix1h_usd: Some(1.5),
                cold_prefix1h_comparison_usd: None,
                warm_reuse_break_even_fraction: None,
                unlinked: Some(UnlinkedSensitivity {
                    no_billed_attempt: PerTtl::default(),
                    cold_cache: PerTtl {
                        five_minutes: cost(Some(0.004), 0.004),
                        one_hour: cost(Some(-0.004), -0.004),
                    },
                }),
            },
        })
    }

    fn compaction_check(base: CheckBase<i64>) -> CheckResult {
        CheckResult::Compaction(CompactionCheck {
            base,
            bucket: "all",
            model: Some("claude-opus-5-5".into()),
            models: vec![Some("claude-opus-5-5".into())],
            unset: false,
            setting: None,
            baseline_cost: cost(Some(1234567.891), 1234567.891),
            observed_summary_cost: cost(None, 0.125),
            ttls: PerBucket {
                main: Some(Ttl::OneHour),
                subagent: None,
            },
            scenarios: vec![WindowScenario {
                window: 150_000,
                cost: cost(Some(10.5), 10.5),
                baseline_comparison_cost: cost(Some(-10.5), -10.5),
                comparison_cost: Cost {
                    unknown: vec!["no matching compaction summary".into()],
                    ..cost(None, 2.675)
                },
                savings_usd: Some(0.375),
                coverage: Coverage {
                    priced: 3,
                    total: 4,
                },
                added: vec![AddedCompaction {
                    request: "synthetic-added".into(),
                    input_tokens: 150_000,
                    summary_tokens: 2000,
                    prefix_tokens: 1,
                    basis: vec![CompactionBasis {
                        event: "synthetic-boundary".into(),
                        request: "synthetic-resumed".into(),
                    }],
                    summary_cost: cost(Some(0.625), 0.625),
                    prefix_cost: cost(None, 0.0),
                }],
                affected_requests: 12,
                cache_evidence: vec![CacheReplayEvidence {
                    request: "synthetic-window-request".into(),
                    part: 1,
                    ttl: Ttl::FiveMinutes,
                    state: KnownState::Estimated,
                    predecessors: vec![],
                    outcome: ReplayOutcome::Estimated,
                    reason: "synthetic geometry",
                }],
                sensitivity: WindowSensitivity {
                    larger_summary_savings_usd: Some(0.015),
                    cold_summary_savings_usd: None,
                    reread_break_even_usd: Some(1.005),
                },
            }],
            usage_estimates: vec![],
            cache_evidence: vec![],
        })
    }

    fn period(status: PeriodStatus, resume: Option<&str>) -> IdlePeriod {
        IdlePeriod {
            request: "synthetic-idle-request".into(),
            resume: resume.map(Text::from),
            session: "session".into(),
            context_tokens: Some(180_000),
            gap_ms: 3_700_000,
            status,
            reason: "synthetic gap",
            confirmed: status == PeriodStatus::Eligible,
            return_timing: ReturnTiming::AfterExpiry,
            outcome: PeriodOutcome::Unobserved,
            event: None,
            version: Some("2.1.314".into()),
            client: None,
            flag_state: Some("on"),
            activation: Some(1.0 / 3.0),
            activation_basis: "synthetic cohort".into(),
            summary_cost: cost(Some(0.875), 0.875),
            calibration: Some(PeriodCalibration {
                events: vec!["synthetic-boundary".into(), "second".into()],
                summary_tokens: 1500,
                prefix_tokens: 30000,
            }),
        }
    }

    fn idle_scenario(periods: Vec<IdlePeriod>, delta: Option<f64>) -> IdleScenario {
        IdleScenario {
            baseline: "current settings".into(),
            applicable: true,
            ttls: PerBucket {
                main: Some(Ttl::OneHour),
                subagent: Some(Ttl::FiveMinutes),
            },
            windows: vec![ScenarioWindow {
                owner: "default".into(),
                models: vec![Some("claude-opus-5-5".into()), None],
                value: Some(200_000),
            }],
            periods,
            cost: OnOff {
                on: cost(Some(12.0), 12.0),
                off: cost(None, 3.0),
            },
            adjustment: OnOff::default(),
            delta_usd: delta,
            monthly_delta_usd: delta,
            by_context: vec![ContextDelta {
                tokens: Some(100_000),
                periods: 2,
                delta_usd: Some(-0.5),
            }],
            activation_cohorts: vec![ActivationCohort {
                version: "2.1.314".into(),
                client: "cli".into(),
                flag_state: "on",
                applied: 1,
                eligible: 3,
                fraction: 1.0 / 3.0,
            }],
            sensitivity: IdleSensitivity {
                closed_delta_usd: Some(0.1),
                open_delta_usd: None,
                larger_summary_delta_usd: Some(-2.5),
                late_trigger_delta_usd: Some(0.3),
                lifetime_break_even_fraction: None,
                reread_break_even_usd_per_resumed_summary: Some(0.02),
            },
            unknown: vec!["synthetic idle unknown".into()],
        }
    }

    fn idle_check(
        base: CheckBase<bool>,
        periods: Vec<IdlePeriod>,
        delta: Option<f64>,
    ) -> CheckResult {
        CheckResult::Idle(IdleCheck {
            base,
            bucket: "main",
            threshold: Some(200_000),
            current_settings: idle_scenario(periods.clone(), delta),
            after_recommendations: idle_scenario(periods, delta),
            usage_estimates: vec![],
        })
    }

    fn speed_check(base: CheckBase<String>, reason: &str) -> CheckResult {
        CheckResult::Speed(SpeedCheck {
            base: CheckBase {
                evidence: Evidence::Guidance,
                coverage: None,
                ..base
            },
            bucket: "all",
            replacement: "fastMode: false".into(),
            reason: reason.into(),
            observed: ObservedPremium {
                premium_parts: 1,
                standard_parts: 2,
                unknown_parts: 3,
                premium: cost(None, 0.25),
                monthly_premium_usd: None,
            },
            current_estimate: CurrentPremium {
                premium: cost(Some(4.5), 4.5),
                priced_parts: 2,
                unknown_parts: 0,
            },
            usage_estimates: vec![],
        })
    }

    /// A context row with its derived per-use cost and verdict at a $1 limit.
    fn context_piece(
        kind: ContextKind,
        name: &str,
        control: Option<Control>,
        estimated: Option<f64>,
        calls: Option<u64>,
    ) -> ContextCost {
        let cost_per_use = calls
            .filter(|&c| c > 0)
            .and_then(|c| estimated.map(|e| e / c as f64));
        ContextCost {
            harness: Harness::Claude,
            largest_characters: None,
            skill: None,
            details: vec![],
            instruction_size: None,
            profiles: vec!["profile".into()],
            kind,
            name: name.into(),
            requests: 100,
            priced: if estimated.is_some() { 90 } else { 0 },
            conditional_requests: 0,
            tier_unknown_requests: 0,
            assumptions: vec![],
            tokens: estimated.map(|_| 12_345),
            cost_usd: Usd::from_f64(estimated.map_or(0.0, |e| e * 0.9)),
            estimated_usd: estimated,
            calls,
            last_used: calls.filter(|&c| c > 0).map(|_| NOV1 + 5 * DAY_MS),
            current: true,
            cost_per_use_usd: cost_per_use,
            verdict: match (&control, estimated, calls) {
                (Some(_), Some(_), Some(0)) => Some(Verdict::Off),
                (Some(_), Some(_), Some(_)) if cost_per_use > Some(1.0) => Some(Verdict::Consider),
                (Some(_), Some(_), Some(_)) => Some(Verdict::Keep),
                _ => None,
            },
            control,
        }
    }

    fn deny(rule: &str) -> Option<Control> {
        Some(Control {
            path: vec!["permissions".into(), "deny".into()],
            value: ControlValue::Text(rule.into()),
            append: true,
        })
    }

    fn switch(path: &[&str], value: ControlValue) -> Option<Control> {
        Some(Control {
            path: path.iter().map(|p| p.to_string()).collect(),
            value,
            append: false,
        })
    }

    /// A report exercising every check kind and context verdict, with one profile.
    fn synthetic() -> Report {
        let profile = make_profile("profile", ROOT);
        let requests: Vec<Request> = [(0, 12_345), (1, 150_000), (2, 180_499), (3, 499)]
            .into_iter()
            .map(|(i, input)| {
                let mut r = make_request();
                r.id = format!("request-{i}").into();
                r.time = NOV1 + i * 1000;
                r.profiles = vec!["profile".into()].into();
                r.parts[0].usage.input = Some(input);
                r
            })
            .collect();
        let off = ControlValue::Bool(false);
        let hidden = ControlValue::Text("user-invocable-only".into());
        let context = vec![
            context_piece(
                ContextKind::System,
                "Claude Code system prompt",
                None,
                Some(210.0),
                None,
            ),
            context_piece(
                ContextKind::Tool,
                "Artifact",
                switch(&["enableArtifact"], off.clone()),
                Some(90.0),
                Some(0),
            ),
            context_piece(
                ContextKind::Tool,
                "Agent",
                deny("Agent"),
                Some(40.0),
                Some(310),
            ),
            context_piece(
                ContextKind::Skill,
                "example",
                switch(&["skillOverrides", "example"], hidden),
                Some(12.0),
                Some(2),
            ),
            context_piece(
                ContextKind::Skill,
                SYNCED_SKILLS,
                switch(&["syncClaudeAiSkills"], off.clone()),
                Some(30.0),
                Some(2),
            ),
            context_piece(ContextKind::Skill, "tools:deploy", None, Some(6.0), Some(0)),
            context_piece(
                ContextKind::McpServer,
                "docs",
                deny("mcp__docs"),
                Some(7.5),
                Some(1),
            ),
            context_piece(
                ContextKind::Instructions,
                &format!("{ROOT}/CLAUDE.md"),
                None,
                Some(5.0),
                None,
            ),
            context_piece(ContextKind::Tool, "Bash", None, Some(3.0), Some(40)),
            context_piece(
                ContextKind::Other,
                "Skill list framing",
                None,
                Some(0.0),
                None,
            ),
            context_piece(
                ContextKind::Tool,
                "Workflow",
                switch(&["enableWorkflows"], off),
                None,
                Some(0),
            ),
            // No longer sent, so it gets no verdict.
            ContextCost {
                current: false,
                verdict: None,
                ..context_piece(
                    ContextKind::Tool,
                    "Retired",
                    deny("Retired"),
                    Some(0.5),
                    Some(0),
                )
            },
            context_piece(
                ContextKind::Instructions,
                "/repo/CLAUDE.md",
                None,
                None,
                None,
            ),
        ];
        let mut ttl_main = check_base(Action::Change, Some(Ttl::FiveMinutes), Some(Ttl::OneHour));
        ttl_main.savings_usd = Some(12.34);
        ttl_main.monthly_savings_usd = Some(24.68);
        let mut ttl_sub = check_base(Action::Keep, Some(Ttl::OneHour), Some(Ttl::OneHour));
        ttl_sub.coverage = Some(Coverage {
            priced: 2000,
            total: 2000,
        });
        let mut window = check_base(Action::Advisory, Some(200_000), None);
        window.owner = format!("{ROOT}/settings.json:autoCompactWindow");
        window.evidence = Evidence::Estimated;
        let mut window_keep = check_base(Action::Keep, Some(1_000_000), Some(1_000_000));
        window_keep.owner = window.owner.clone();
        let mut idle = check_base(Action::Advisory, Some(true), None);
        idle.owner = format!("{ROOT}/settings.json:idleCompaction");
        let mut fast = check_base(
            Action::Keep,
            Some("standard".to_string()),
            Some("standard".to_string()),
        );
        fast.owner = format!("{ROOT}/settings.json:fastMode");
        fast.savings_usd = None;
        fast.monthly_savings_usd = None;
        let checks = vec![
            ttl_check(Bucket::Main, ttl_main, 12.34),
            ttl_check(Bucket::Subagent, ttl_sub, 4.2),
            compaction_check(window),
            compaction_check(window_keep),
            idle_check(
                idle,
                vec![
                    period(PeriodStatus::Eligible, Some("synthetic-resume")),
                    period(PeriodStatus::Censored, None),
                ],
                Some(0.0),
            ),
            speed_check(fast, "explicit fastMode false"),
            {
                let mut base = check_base(Action::Advisory, Some(true), Some(false));
                base.owner = format!("{ROOT}/settings.json:autoMemoryEnabled (default)");
                base.evidence = Evidence::Estimated;
                base.coverage = None;
                CheckResult::Memory(MemoryCheck {
                    base,
                    harness: Harness::Claude,
                    off: "\"autoMemoryEnabled\": false".into(),
                    off_file: Some(format!("{ROOT}/settings.json")),
                    context_usd: Some(140.0),
                    writes_usd: Some(10.0),
                    reads: 1,
                    writes: 2,
                    unresolved: None,
                    overrides: vec![],
                    unmeasured_generation: false,
                })
            },
        ];
        Report {
            subagents: vec![],
            generated_at: "2026-12-01T00:00:00.000Z".into(),
            scan: Scan {
                subagents: vec![],
                t3: vec![],
                sessions: Default::default(),
                snapshot_started_at: "2026-12-01T00:00:00.000Z".into(),
                snapshot_finished_at: "2026-12-01T00:00:01.000Z".into(),
                since: Some(NOV1),
                roots: vec![RootStatus {
                    harness: Harness::Claude,
                    path: ROOT.into(),
                    status: RootState::Selected,
                    origin: RootOrigin::Default,
                }],
                profiles: vec![profile],
                snapshots: vec![Snapshot {
                    file: format!("{ROOT}/projects/project/main.jsonl"),
                    harness: Harness::Claude,
                    profiles: vec!["profile".into()],
                    bytes: 100,
                    device: 1,
                    inode: 2,
                    deferred_bytes: 0,
                    skipped_lines: 2,
                    lines: 10,
                }],
                requests,
                events: vec![],
                exposure: ExposureScan::default(),
                cache_context: CacheContext::default(),
                coverage: ScanCoverage {
                    requests: 2000,
                    source_known: 1990,
                    tier_known: 1980,
                    missing_usage: 1234,
                    deferred_files: 1,
                    skipped_lines: 2,
                    skipped_files: 1,
                    issues: vec!["synthetic coverage limit".into()],
                },
            },
            prices: PriceSource {
                source: "https://models.dev/api.json".into(),
                fetched_at: Some("2026-11-30T08:00:00.000Z".into()),
                offline: false,
            },
            historical: vec![HistoricalCost {
                bucket: "profile".into(),
                profiles: vec!["profile".into()],
                requests: 2000,
                priced_requests: 1000,
                conditional_requests: 0,
                tier_sensitivity: vec![],
                cost: cost(None, 1500.0),
            }],
            compactions: vec![CompactionCharge {
                event: "synthetic-boundary".into(),
                request: None,
                evidence: Evidence::Estimated,
                cost: cost(Some(0.3), 0.3),
                ttl_cost: PerTtl::default(),
                assumptions: vec!["synthetic summary proxy".into()],
            }],
            exposure: ExposureSummary {
                calibration: vec![],
                assumptions: vec!["synthetic exposure assumption".into()],
            },
            context,
            max_cost_per_use_usd: 1.0,
            checks,
            assumptions: vec!["synthetic report assumption".into()],
        }
    }

    /// The synthetic report with a second Codex profile owning a fast-mode change.
    fn two_profiles() -> Report {
        let mut report = synthetic();
        let mut other = make_profile("another-profile", "/synthetic/second-profile");
        other.harness = Harness::Codex;
        report.scan.profiles.push(other);
        let mut fast = check_base(
            Action::Change,
            Some("fast".to_string()),
            Some("standard".to_string()),
        );
        fast.profile = "another-profile".into();
        fast.owner = "/synthetic/second-profile/config.toml:service_tier".into();
        fast.savings_usd = None;
        fast.monthly_savings_usd = None;
        report
            .checks
            .push(speed_check(fast, "user service_tier selects fast"));
        let mut piece = report.context[1].clone();
        piece.profiles = vec!["another-profile".into()];
        report.context.push(piece);
        report
    }

    fn changes(report: &Report, width: Option<usize>) -> String {
        format_changes(
            report,
            ReportOptions {
                width,
                ..Default::default()
            },
        )
    }

    fn text(report: &Report, width: Option<usize>, details: bool) -> String {
        format_report(
            report,
            ReportOptions {
                width,
                details,
                ..Default::default()
            },
        )
    }

    fn compact(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(start) = rest.find('\x1b') {
            out.push_str(&rest[..start]);
            let end = rest[start..].find('m').expect("SGR ends in m");
            rest = &rest[start + end + 1..];
        }
        out + rest
    }

    #[test]
    fn removed_t3_owners_are_named_in_the_bill() {
        let mut report = synthetic();
        report.scan.t3.push(T3State {
            path: "/state".into(),
            status: "selected".into(),
            exclusions: vec![],
            metadata: String::new(),
            snapshot_started_at: None,
            snapshot_finished_at: None,
            joined_requests: 0,
            ambiguous_requests: 0,
            unresolved: IndexMap::default(),
            removed: IndexMap::from_iter([("gone".into(), 3)]),
            instances: Default::default(),
            text_generation: None,
        });
        let mut group = report.historical[0].clone();
        group.profiles = vec![crate::t3::profile_key("/state", "gone")];
        report.historical.push(group);
        let rendered = compact(&text(&report, Some(400), true));
        assert!(rendered.contains(&compact(
            "T3 gone (removed from T3 settings); 2000 requests"
        )));
        assert!(rendered.contains(&compact("T3 removed: 3 requests of instance gone")));
    }

    #[test]
    fn a_capped_window_names_the_setting_and_the_effective_window() {
        let mut report = synthetic();
        let mut base = check_base(Action::Keep, Some(200_000), Some(200_000));
        base.owner =
            format!("{ROOT}/settings.json:modelSettings.claude-opus-5-5.autoCompactWindow");
        let mut check = compaction_check(base);
        if let CheckResult::Compaction(c) = &mut check {
            c.setting = Some(400_000);
        }
        report.checks = vec![check];
        let summary = text(&report, None, false);
        let line = report_line(&summary, "Auto-compact window");
        assert_eq!(line.head, "Auto-compact window (claude-opus-5-5) 200k");
        assert!(
            line.evidence.starts_with(
                "Set to 400,000 tokens, which the model's context window caps at 200k. "
            ),
            "{line:?}"
        );
        let details = compact(&text(&report, Some(400), true));
        assert!(details.contains(&compact(
            "400000 tokens, capped at the model's context window"
        )));
        // A change names the effective window, not the arrow to the new one.
        let mut base = check_base(Action::Change, Some(200_000), Some(100_000));
        base.owner =
            format!("{ROOT}/settings.json:modelSettings.claude-opus-5-5.autoCompactWindow");
        base.savings_usd = Some(1.0);
        let mut check = compaction_check(base);
        if let CheckResult::Compaction(c) = &mut check {
            c.setting = Some(400_000);
        }
        report.checks = vec![check];
        let line = report_line(&text(&report, None, false), "Auto-compact window");
        assert!(line.head.contains("200k -> 100k"), "{line:?}");
        assert!(
            line.evidence.starts_with(
                "Set to 400,000 tokens, which the model's context window caps at 200k. "
            ),
            "{line:?}"
        );
    }

    #[test]
    fn numbers_format_like_javascript() {
        // toFixed rounds exact binary halves up; Rust's formatter rounds them to even.
        assert_eq!(usd(0.125), "$0.13");
        assert_eq!(usd(0.625), "$0.63");
        assert_eq!(usd(2.675), "$2.67");
        assert_eq!(usd(1.005), "$1.00");
        assert_eq!(usd(0.004), "<$0.01");
        assert_eq!(usd(-0.004), "-<$0.01");
        assert_eq!(usd(-0.0), "$0.00");
        assert_eq!(about(10.5), "$11");
        assert_eq!(about(-10.5), "-$10");
        assert_eq!(about(12.0), "$12");
        assert_eq!(about(7.5), "$7.50");
        assert_eq!(count(1_234_567u64), "1,234,567");
        assert_eq!(count(-1000i64), "-1,000");
        assert_eq!(count(999u64), "999");
        assert_eq!(percent(1990, 2000), "99.5");
        assert_eq!(percent(2, 3), "66.6");
        assert_eq!(js_number(1.0 / 3.0), "0.3333333333333333");
        assert_eq!(js_number(1e-7), "1e-7");
        assert_eq!(js_number(150_000.5), "150000.5");
        assert_eq!(days(Some(0), Some(DAY as Millis / 3)), "1 day");
    }

    #[test]
    fn the_summary_gives_one_line_per_setting() {
        let report = synthetic();
        let text = text(&report, None, false);
        let rows = report_lines(&text);
        let line = |head: &str| report_line(&text, head);
        // Sections follow the kind of claim, and red lines lead each section.
        let claims: Vec<&str> = rows.iter().map(|r| r.claim.as_str()).collect();
        let mut sorted = claims.clone();
        sorted.dedup();
        assert_eq!(sorted, ["MEASURED", "OFFICIAL", "OPINION"]);
        for claim in ["MEASURED", "OFFICIAL", "OPINION"] {
            let marks: Vec<&str> = rows
                .iter()
                .filter(|r| r.claim == claim)
                .map(|r| r.mark.as_str())
                .collect();
            let order = |m: &&str| {
                ["change", "consider", "keep", "unknown", "info"]
                    .iter()
                    .position(|w| w == m)
            };
            assert!(marks.is_sorted_by_key(order), "{claim}: {marks:?}");
        }
        let has = |head: &str, mark: &str, claim: &str, evidence: &str| {
            let row = line(head);
            assert_eq!(
                (
                    row.head.as_str(),
                    row.mark.as_str(),
                    row.claim.as_str(),
                    row.evidence.as_str()
                ),
                (head, mark, claim, evidence)
            );
        };
        assert!(text.contains("\n2 setting changes recommended.\n"));
        has(
            "Main-agent prompt cache TTL 5m -> 1h saves $25",
            "change",
            "MEASURED",
            "Replayed at 5m and 1h, assuming the same sessions, using the 99.5% of requests with measured usage; 1h costs less. Set in /synthetic/home/.claude/settings.json:promptCacheTtl.",
        );
        has(
            "Artifact tool on -> off costs $90",
            "change",
            "MEASURED",
            "Never used. Turn off: \"enableArtifact\": false.",
        );
        // Used, but above the limit per use: worth considering off, with the setting inline.
        has(
            "Skill example on costs $12",
            "consider",
            "MEASURED",
            "The model used it 2 times, $6 each. Hide it from the model: \"skillOverrides\": {\"example\": \"user-invocable-only\"}.",
        );
        // Turning syncing off removes typed uses too, so every use counts.
        has(
            "Skills synced from claude.ai on costs $30",
            "consider",
            "MEASURED",
            "Used 2 times, $15 each. Turn off: \"syncClaudeAiSkills\": false.",
        );
        has(
            "Skill tools:deploy costs $6",
            "info",
            "MEASURED",
            "Never used. No setting turns it off: turn off its plugin with /plugin, or remove the skill from the added directory.",
        );
        has(
            "Agent tool on costs $40",
            "keep",
            "MEASURED",
            "Used 310 times, $0.13 each. Turn off: \"permissions\": {\"deny\": [\"Agent\"]}.",
        );
        has(
            "Subagent prompt cache TTL 1h saves $4.20",
            "keep",
            "MEASURED",
            "Replayed at 5m and 1h, assuming the same sessions; 1h costs less than 5m. Set in /synthetic/home/.claude/settings.json:promptCacheTtl.",
        );
        has(
            "Workflow tool on",
            "unknown",
            "MEASURED",
            "Not enough evidence: none of its requests could be priced; never used. Turn off: \"enableWorkflows\": false.",
        );
        has(
            "Idle compaction on",
            "unknown",
            "MEASURED",
            "Not enough evidence: on and off cost the same on recorded idle gaps, so neither is cheaper. Set in /synthetic/home/.claude/settings.json:idleCompaction.",
        );
        assert!(text.contains(
            "Not enough evidence: too few recorded compactions to model a different window. Its requests carried a median of 150,000 tokens, and 1 in 10 carried more than 180,000."
        ));
        has(
            "/repo/CLAUDE.md costs unknown",
            "unknown",
            "OFFICIAL",
            "Not enough evidence: this record has no rendered instruction text to measure against the provider's guidance.",
        );
        // A zero-cost fixed piece is hidden; the system prompt and Bash are summed.
        has(
            "System prompt and core tools costs $213",
            "info",
            "MEASURED",
            "Includes core tools such as Bash and Read, and the skill and subagent lists. No setting turns these off.",
        );
        // Auto memory and fast mode rest on the maintainer's preference; memory is
        // advised off whatever it cost, without changing the exit code.
        has(
            "Auto memory on costs $150",
            "consider",
            "OPINION",
            "Opinion: notes the agent writes for itself skip review and encourage slop, so keep durable guidance in reviewed, checked-in CLAUDE.md or AGENTS.md. Measured: used 3 times (1 read, 2 writes). Set \"autoMemoryEnabled\": false in /synthetic/home/.claude/settings.json.",
        );
        has(
            "Fast mode off",
            "keep",
            "OPINION",
            "Opinion: standard speed is enough, and fast mode bills a premium for the same work. Set in /synthetic/home/.claude/settings.json:fastMode.",
        );
        // Pieces no longer sent and those without a verdict get no line of their own.
        assert!(!text.contains("Retired") && !text.contains("Skill list framing"));
        // The changes list each edit after the report, in report order.
        let listed = changes(&report, None);
        assert!(!text.contains("Changes to make"));
        let edits: Vec<&str> = listed
            .lines()
            .filter(|l| l.starts_with("      "))
            .map(str::trim)
            .collect();
        assert_eq!(
            edits,
            [
                r#""promptCacheTtl": "1h""#,
                r#""enableArtifact": false"#,
                r#""skillOverrides": {"example": "user-invocable-only"}"#,
                r#""syncClaudeAiSkills": false"#,
                r#""permissions": {"deny": ["mcp__docs"]} (added to the list)"#,
                r#""autoMemoryEnabled": false"#,
            ],
            "{listed}"
        );
    }

    #[test]
    fn doctor_edits_the_owning_setting_and_leaves_other_owners_by_hand() {
        use crate::doctor::{Edit, How, Patch, fixes};
        use serde_json::json;
        let file = format!("{ROOT}/settings.json");
        let set = |path: &[&str], value: serde_json::Value| {
            How::Edit(Patch {
                file: file.clone(),
                harness: Harness::Claude,
                edits: vec![Edit {
                    path: path.iter().map(|p| p.to_string()).collect(),
                    value,
                    append: false,
                }],
            })
        };
        let how = |report: &Report, name: &str| {
            fixes(report, false)
                .into_iter()
                .find(|f| f.name.starts_with(name))
                .unwrap_or_else(|| panic!("no {name} fix"))
                .how
        };
        let by_hand = |how: How| match how {
            How::Manual { reason, .. } => reason,
            How::Edit(patch) => panic!("expected a step by hand, got {patch:?}"),
        };
        let ttl = "Main-agent prompt cache TTL";
        let mut report = synthetic();
        assert_eq!(how(&report, ttl), set(&["promptCacheTtl"], json!("1h")));
        // The variable in the settings env is edited there; the setting outranks the 1h flag.
        for (owner, path) in [
            (
                "env.CLAUDE_CODE_PROMPT_CACHE_TTL",
                &["env", "CLAUDE_CODE_PROMPT_CACHE_TTL"][..],
            ),
            ("env.ENABLE_PROMPT_CACHING_1H", &["promptCacheTtl"][..]),
        ] {
            ttl_mut(&mut report).base.owner = format!("{file}:{owner}");
            assert_eq!(how(&report, ttl), set(path, json!("1h")), "{owner}");
        }
        ttl_mut(&mut report).base.owner = "CLAUDE_CODE_PROMPT_CACHE_TTL".into();
        assert_eq!(
            by_hand(how(&report, ttl)),
            format!(
                "the CLAUDE_CODE_PROMPT_CACHE_TTL environment variable sets it and outranks {file}; change it where it is exported"
            )
        );
        let project = "/synthetic/project/.claude/settings.json:promptCacheTtl";
        ttl_mut(&mut report).base.owner = project.into();
        assert_eq!(
            by_hand(how(&report, ttl)),
            format!("{project} sets it; token-max edits only {file}")
        );

        // A model's own row, whose ID may hold dots, under a sizing note.
        let mut window = check_base(Action::Change, Some(967_000), Some(400_000));
        window.owner =
            format!("{file}:modelSettings.claude-opus-4.5.autoCompactWindow (capped at 200000)");
        report.checks.push(compaction_check(window));
        assert_eq!(
            how(&report, "Auto-compact window"),
            set(
                &["modelSettings", "claude-opus-4.5", "autoCompactWindow"],
                json!(400_000)
            )
        );

        // Two lines that want one setting at different values leave it to the person.
        let mut on = check_base(Action::Change, Some(false), Some(true));
        on.owner = format!("{file}:idleCompaction");
        let off = CheckBase {
            current: Some(true),
            recommended: Some(false),
            ..on.clone()
        };
        report.checks = vec![idle_check(on, vec![], None), idle_check(off, vec![], None)];
        report.context.clear();
        let idle = fixes(&report, false);
        assert_eq!(idle[0].how, set(&["idleCompaction"], json!(true)));
        assert_eq!(
            by_hand(idle[1].how.clone()),
            "Idle compaction wants a different value for the same setting"
        );
    }

    #[test]
    fn tool_blocks_are_covered_by_a_server_block() {
        let mut report = synthetic();
        report.checks.clear();
        report.scan.profiles[0].harness = Harness::Codex;
        report.scan.profiles[0].settings_file = "/synthetic/.codex/config.toml".into();
        let mut server = context_piece(
            ContextKind::McpServer,
            "docs",
            switch(
                &["mcp_servers", "docs", "enabled"],
                ControlValue::Bool(false),
            ),
            Some(20.0),
            Some(100),
        );
        server.harness = Harness::Codex;
        let mut tool = context_piece(
            ContextKind::McpTool,
            "mcp__docs__write",
            Some(Control {
                path: vec!["mcp_servers".into(), "docs".into(), "disabled_tools".into()],
                value: ControlValue::Text("write".into()),
                append: true,
            }),
            Some(5.0),
            Some(0),
        );
        tool.harness = Harness::Codex;
        server.details.push(tool);
        server.details.push(context_piece(
            ContextKind::McpTool,
            "mcp__docs__search",
            switch(
                &["mcp_servers", "docs", "disabled_tools"],
                ControlValue::Text("search".into()),
            ),
            None,
            Some(0),
        ));
        report.context = vec![server];
        let rendered = text(&report, None, false);
        let line = report_line(&rendered, "MCP tool mcp__docs__write");
        assert_eq!(
            (line.mark.as_str(), line.head.as_str()),
            ("change", "MCP tool mcp__docs__write on -> off costs $5")
        );
        assert!(line.evidence.contains("Included in its server's cost."));
        let patch = changes(&report, None);
        assert!(patch.contains("disabled_tools = [\"write\"]"));
        assert!(!patch.contains("enabled = false"));
        for details in [false, true] {
            let rendered = text(&report, None, details);
            assert_eq!(rendered.matches("Exposure:").count(), 1);
            if details {
                assert!(rendered.contains("1 recommended change"));
                assert!(!rendered.contains("Available checks favor current settings"));
                assert!(
                    rendered.find("MCP server docs:").unwrap()
                        < rendered.find("Exposure:").unwrap()
                );
            }
        }

        let mut conditional = report.clone();
        let server = &mut conditional.context[0];
        let mark_conditional = |row: &mut ContextCost| {
            row.verdict = None;
            row.tier_unknown_requests = 100;
            row.conditional_requests = row.priced;
            row.assumptions = vec!["conditional tool-tier estimate".into()];
        };
        mark_conditional(server);
        for row in &mut server.details {
            mark_conditional(row);
        }
        let rendered = text(&conditional, None, false);
        let advisory: Vec<_> = report_lines(&rendered)
            .into_iter()
            .filter(|r| r.mark == "unknown")
            .collect();
        assert_eq!(advisory[0].head, "MCP server docs on costs $20");
        assert_eq!(advisory[1].head, "MCP tool mcp__docs__write on costs $5");
        assert!(advisory[..2].iter().all(|r| {
            r.evidence
                .contains("tier sensitivity could reverse keep or off")
        }));
        assert!(changes(&conditional, None).is_empty());
        let detailed = text(&conditional, None, true);
        assert_eq!(
            detailed.matches("conditional tool-tier estimate").count(),
            3
        );

        report.context[0].verdict = Some(Verdict::Off);
        let rendered = text(&report, None, false);
        let patch = changes(&report, None);
        assert!(patch.contains("enabled = false"));
        assert!(!patch.contains("disabled_tools"));
        // The server's off covers its tools, which stay listed without a verdict.
        let covered = report_line(&rendered, "MCP tool mcp__docs__write");
        assert_eq!(
            (covered.mark.as_str(), covered.head.as_str()),
            ("info", "MCP tool mcp__docs__write costs $5")
        );
        assert!(
            covered
                .evidence
                .contains("Turning off its server covers it.")
        );
        assert!(!rendered.contains("Not enough evidence"));
        assert!(rendered.contains("Some feature costs are scaled from their priced requests"));
        let detailed = text(&report, None, true);
        assert_eq!(detailed.matches("Recommended off:").count(), 1);
        assert_eq!(
            detailed
                .matches("Covered by the server-off recommendation")
                .count(),
            2
        );
        assert!(!detailed.contains("Recommended off: mcp_servers.docs.disabled_tools"));
        // The server keeps the tool's cost inside it, without a second top-level row.
        assert_eq!(report.context.len(), 1);
        assert_eq!(report.context[0].details[0].estimated_usd, Some(5.0));
    }

    #[test]
    fn tools_without_their_own_switch_keep_a_line() {
        let mut report = synthetic();
        report.checks.clear();
        let mut server = context_piece(
            ContextKind::McpServer,
            "docs",
            switch(
                &["mcp_servers", "docs", "enabled"],
                ControlValue::Bool(false),
            ),
            Some(20.0),
            Some(3),
        );
        server.harness = Harness::Codex;
        server.verdict = Some(Verdict::Keep);
        let mut tool = context_piece(
            ContextKind::McpTool,
            "mcp__docs__read",
            None,
            Some(5.0),
            Some(2),
        );
        tool.harness = Harness::Codex;
        server.details.push(tool);
        let mut conditional = context_piece(
            ContextKind::McpTool,
            "mcp__docs__write",
            switch(
                &["mcp_servers", "docs", "disabled_tools"],
                ControlValue::Text("write".into()),
            ),
            Some(4.0),
            None,
        );
        conditional.harness = Harness::Codex;
        server.details.push(conditional);
        report.context = vec![server];
        let rendered = text(&report, None, false);
        let read = report_line(&rendered, "MCP tool mcp__docs__read");
        assert_eq!(
            (read.mark.as_str(), read.head.as_str()),
            ("info", "MCP tool mcp__docs__read costs $5")
        );
        assert!(read.evidence.contains("Included in its server's cost."));
        assert!(
            read.evidence
                .contains("raw tool identity or owning server is unresolved")
        );
        let write = report_line(&rendered, "MCP tool mcp__docs__write");
        assert_eq!(write.mark, "unknown");
        assert!(write.evidence.contains("Included in its server's cost."));
    }

    #[test]
    fn loading_guidance_is_shared_and_control_limits_stay_with_their_owner() {
        let mut report = synthetic();
        report.checks.clear();
        report.context = ["docs", "search"]
            .map(|name| {
                context_piece(
                    ContextKind::McpServer,
                    name,
                    switch(
                        &["permissions", "deny"],
                        ControlValue::Text(format!("mcp__{name}")),
                    ),
                    Some(2.0),
                    Some(10),
                )
            })
            .into();
        let mut plugin = context_piece(ContextKind::McpServer, "plugin", None, Some(2.0), Some(10));
        plugin.harness = Harness::Codex;
        report.context.push(plugin);
        let mut apps = context_piece(
            ContextKind::McpServer,
            "codex_apps",
            switch(&["features", "apps"], ControlValue::Bool(false)),
            Some(2.0),
            Some(10),
        );
        apps.harness = Harness::Codex;
        let mut tool = context_piece(
            ContextKind::McpTool,
            "mcp__codex_apps__query",
            None,
            Some(1.0),
            Some(0),
        );
        tool.harness = Harness::Codex;
        apps.details.push(tool);
        report.context.push(apps);
        report.context.push(context_piece(
            ContextKind::Instructions,
            "/repo/.claude/rules/foo.md",
            None,
            None,
            None,
        ));
        for details in [false, true] {
            let rendered = text(&report, None, details);
            assert_eq!(rendered.matches("Claude MCP loading:").count(), 1);
            assert_eq!(rendered.matches("Claude tool search needs").count(), 1);
            assert_eq!(
                rendered.matches("net deferral savings are unknown").count(),
                1
            );
            for name in ["docs", "search"] {
                assert_eq!(
                    rendered
                        .matches(&format!("mcpServers.{name}.alwaysLoad"))
                        .count(),
                    1
                );
            }
            assert_eq!(
                rendered.matches("Configure the plugin or launcher").count(),
                1
            );
            assert!(!rendered.contains("Instruction budget unknown"));
            assert!(!rendered.contains("raw tool identity or owning server is unresolved"));
            if !details {
                assert!(
                    report_line(&rendered, "ChatGPT apps")
                        .evidence
                        .contains("These transcripts do not establish that id mapping")
                );
            }
        }
    }

    #[test]
    fn instruction_files_are_official_against_the_providers_size_guidance() {
        let out = Out {
            text: String::new(),
            options: ReportOptions::default(),
            arrow: "->",
            home: None,
            profiles: IndexMap::default(),
            removed: Default::default(),
        };
        let file = |harness, name: &str, bytes, lines| {
            let mut c = context_piece(ContextKind::Instructions, name, None, Some(1.0), None);
            c.harness = harness;
            c.instruction_size = Some(crate::model::InstructionSize { bytes, lines });
            c
        };
        let verdict = |c: &ContextCost, profiles: &[Profile]| {
            let row = instruction_row(c, profiles, &out);
            (row.claim, row.mark, row.value)
        };
        let claude = |lines| file(Harness::Claude, "/repo/CLAUDE.md", 100, lines);
        assert_eq!(
            verdict(&claude(200), &[]),
            (Claim::Official, Mark::Consider, "200 lines".into())
        );
        assert_eq!(
            verdict(&claude(199), &[]),
            (Claim::Official, Mark::Keep, "199 lines".into())
        );
        // Claude's guidance is per CLAUDE.md; other files it loads have none.
        let other = file(Harness::Claude, "/repo/AGENTS.md", 100, 900);
        assert_eq!(instruction_row(&other, &[], &out).claim, Claim::Measured);
        assert_eq!(instruction_row(&other, &[], &out).mark, Mark::Info);
        // Rendered bytes count headers and wrappers, so only reaching the budget can mean truncation.
        let mut profile = crate::test_support::make_profile("profile", "/codex");
        let agents = |bytes| file(Harness::Codex, "AGENTS.md", bytes, 1);
        let profiles = std::slice::from_ref(&profile);
        assert_eq!(verdict(&agents(32_767), profiles).1, Mark::Keep);
        assert_eq!(verdict(&agents(32_768), profiles).1, Mark::Consider);
        let mut other = profile.clone();
        other.id = "other".into();
        other
            .settings
            .insert("project_doc_max_bytes".into(), Scalar::Number(65536.0));
        let mut both = agents(100);
        both.profiles.push("other".into());
        assert_eq!(
            verdict(&both, &[profile.clone(), other.clone()]).1,
            Mark::Unknown
        );
        // The same budget from different sources names each of them.
        other
            .settings
            .insert("project_doc_max_bytes".into(), Scalar::Number(32768.0));
        other.instruction_budget_file = Some("/other/config.toml".into());
        let mixed = instruction_row(&both, &[profile.clone(), other], &out).evidence;
        assert!(
            mixed.contains("Codex loads by default and /other/config.toml sets"),
            "{mixed}"
        );
        let evidence =
            |c: &ContextCost, profiles: &[Profile]| instruction_row(c, profiles, &out).evidence;
        let default = evidence(&agents(32_768), std::slice::from_ref(&profile));
        assert!(
            default.contains("32,768 bytes Codex loads by default"),
            "{default}"
        );
        assert!(
            default.contains("command-line override is not recorded"),
            "{default}"
        );
        profile
            .settings
            .insert("project_doc_max_bytes".into(), Scalar::Number(65536.0));
        profile.instruction_budget_file = Some("/etc/codex/config.toml".into());
        let profiles = std::slice::from_ref(&profile);
        assert_eq!(verdict(&agents(40_000), profiles).1, Mark::Keep);
        // A configured budget names the file that sets it, not the default.
        let set = evidence(&agents(70_000), profiles);
        assert!(
            set.contains("65,536 bytes /etc/codex/config.toml sets"),
            "{set}"
        );
        assert!(!set.contains("default"), "{set}");
        // A project or named profile that can set it leaves no verdict.
        profile.instruction_budget_unresolved =
            Some("/repo/.codex/config.toml can set project_doc_max_bytes for some runs".into());
        let profiles = std::slice::from_ref(&profile);
        assert_eq!(verdict(&agents(100), profiles).1, Mark::Unknown);
        assert!(
            evidence(&agents(100), profiles)
                .starts_with("Not enough evidence: /repo/.codex/config.toml can set")
        );
    }

    #[test]
    fn budget_guidance_distinguishes_source_files_from_rendered_instructions() {
        let mut c = context_piece(
            ContextKind::Instructions,
            "/repo/CLAUDE.md",
            None,
            None,
            None,
        );
        c.instruction_size = Some(crate::model::InstructionSize {
            bytes: 401,
            lines: 200,
        });
        let notes = context_guidance(&c, &[], "~/CLAUDE.md");
        assert!(notes[0].contains("~/CLAUDE.md: 200 recorded lines, 401 UTF-8 bytes; at or above"));
        assert!(!notes[0].contains("/repo/CLAUDE.md"));
        assert!(notes[0].contains("imports still load together"));
        c.harness = Harness::Codex;
        c.instruction_size = Some(crate::model::InstructionSize {
            bytes: 32_769,
            lines: 1,
        });
        let mut profile = crate::test_support::make_profile("profile", "/codex");
        let notes = context_guidance(&c, std::slice::from_ref(&profile), "AGENTS.md");
        assert!(notes[0].contains("including any headers or wrappers"));
        assert!(notes[0].contains(
            "Rendered bytes cannot establish whether that source chain exceeded its budget"
        ));
        assert!(notes[0].contains("Selected profile budget: 32768 bytes"));
        profile
            .settings
            .insert("project_doc_max_bytes".into(), Scalar::Number(65536.0));
        let notes = context_guidance(&c, &[profile], "AGENTS.md");
        assert!(notes[0].contains("Selected profile budget: 65536 bytes"));
    }

    #[test]
    fn tool_output_and_codex_switches() {
        let mut report = synthetic();
        report.scan.profiles[0]
            .settings
            .insert("MAX_MCP_OUTPUT_TOKENS".into(), Scalar::Number(50_000.0));
        let mut codex = make_profile("codex", "/synthetic/.codex");
        codex.harness = Harness::Codex;
        codex.settings_file = "/synthetic/.codex/config.toml".into();
        report.scan.profiles.push(codex);
        let output = |harness, profile: &str, name: &str, usd: f64, largest: i64| ContextCost {
            harness,
            profiles: vec![profile.into()],
            largest_characters: Some(largest),
            ..context_piece(ContextKind::ToolOutput, name, None, Some(usd), Some(4))
        };
        let apps = ContextCost {
            harness: Harness::Codex,
            profiles: vec!["codex".into()],
            ..context_piece(
                ContextKind::McpServer,
                "codex_apps",
                switch(&["features", "apps"], ControlValue::Bool(false)),
                Some(30.0),
                Some(0),
            )
        };
        report.context.extend([
            output(Harness::Claude, "profile", "Bash", 20.0, 30_500),
            output(Harness::Claude, "profile", "mcp__docs", 6.0, 900),
            output(Harness::Claude, "profile", "Read", 8.0, 2_000),
            output(Harness::Codex, "codex", "exec", 50.0, 112_000),
            apps,
        ]);
        let text = text(&report, None, false);
        let line = |head: &str| report_line(&text, head);
        assert_eq!(
            line("ChatGPT apps").head,
            "ChatGPT apps on -> off costs $30 [/synthetic/.codex]"
        );
        assert!(
            line("ChatGPT apps")
                .evidence
                .starts_with("Never used. Turn off: features.apps = false. Per-app controls:")
        );
        // One line per cap setting, capped ones first, then everything else together.
        let outputs: Vec<(String, String, String)> = report_lines(&text)
            .into_iter()
            .filter(|r| r.head.contains(" output "))
            .map(|r| (r.mark, r.head, r.evidence))
            .collect();
        let cap = "Not enough evidence: a lower cap keeps less output in context but leaves the model to read the rest back, which transcripts cannot price.";
        assert_eq!(
            outputs,
            [
                (
                    "unknown".to_string(),
                    "Bash output unset costs $20 [/synthetic/home/.claude]".to_string(),
                    format!(
                        "{cap} Its largest result was 30,500 characters. bashOutputMaxChars is unset, so about 30,000 characters."
                    )
                ),
                (
                    "unknown".into(),
                    "MCP output 50,000 tokens costs $6 [/synthetic/home/.claude]".into(),
                    format!(
                        "{cap} Its largest result was 900 characters. MAX_MCP_OUTPUT_TOKENS is 50,000 tokens."
                    )
                ),
                (
                    "unknown".into(),
                    "Tool output unset costs $50 [/synthetic/.codex]".into(),
                    format!(
                        "{cap} Its largest result was 112,000 characters. tool_output_token_limit is unset."
                    )
                ),
                (
                    "info".into(),
                    "Other tool output costs $8 [/synthetic/home/.claude]".into(),
                    "From Read. Its largest result was 2,000 characters. No cap is checked for it."
                        .into()
                ),
            ]
        );
        // Codex switches go to config.toml as TOML.
        let snippet = changes(&report, None);
        assert!(
            snippet.contains("In /synthetic/.codex/config.toml:\n      features.apps = false\n"),
            "{snippet}"
        );
    }

    #[test]
    fn every_check_kind_wraps_completely_at_narrow_widths() {
        let mut report = two_profiles();
        report.scan.profiles[0].root = format!(
            "/synthetic/e\u{301}-東京-💻/{}",
            "long-profile-name-".repeat(8)
        );
        let values = [
            report.scan.profiles[0].root.clone(),
            "/synthetic/second-profile".into(),
            format!("{ROOT}/settings.json:promptCacheTtl"),
            "synthetic assumption".into(),
            "synthetic uncertainty".into(),
            "synthetic attempt assumption".into(),
            "synthetic-attempt: unknown".into(),
            "2048 (not observed output)".into(),
            "unknown ($9.87 known subtotal)".into(),
            "synthetic-cache-predecessor".into(),
            "Current: 5m".into(),
            "Suggested: 1h".into(),
            "[guidance]".into(),
        ];
        for width in [28, 40, 80] {
            let text = text(&report, Some(width), true);
            let at = |s: &str| text.find(s).expect(s);
            assert!(at("Recommended changes") < at("Advisory findings"));
            assert!(at("Advisory findings") < at("Keep current settings"));
            for line in text.lines() {
                assert!(
                    unicode_width::UnicodeWidthStr::width(line) <= width,
                    "{line}"
                );
            }
            let compact = compact(&text);
            for value in &values {
                assert!(compact.contains(&super::tests::compact(value)), "{value}");
            }
            assert!(compact.contains("$12.34") && compact.contains("$24.68"));
            assert!(!text.contains('\x1b') && !text.contains('…'));
            assert!(
                text.contains("e\u{301}"),
                "decomposed names are not normalized"
            );
        }
    }

    #[test]
    fn color_carries_the_verdict_and_words_carry_it_without_color() {
        let report = synthetic();
        // Details only add styles to the same words.
        let plain = text(&report, Some(120), true);
        let styled = format_report(
            &report,
            ReportOptions {
                width: Some(120),
                color: true,
                details: true,
                ..Default::default()
            },
        );
        for style in ["1", "31", "33", "32"] {
            assert!(styled.contains(&format!("\x1b[{style}m")), "{style}");
        }
        assert_eq!(strip_ansi(&styled), plain);
        // The summary's color replaces each verdict word with a dot in that color,
        // dim and hollow when there is no verdict, and adds a legend.
        let styled = format_report(
            &report,
            ReportOptions {
                color: true,
                unicode: true,
                ..Default::default()
            },
        );
        let plain = text(&report, None, false);
        for (style, dot, line) in [
            ("31", "●", "Artifact tool"),
            ("33", "●", "Skill example"),
            ("32", "●", "Agent tool"),
            ("2", "○", "Workflow tool"),
            ("2", "○", "System prompt and core tools"),
            ("32", "●", "Fast mode"),
        ] {
            assert!(
                styled.contains(&format!("\x1b[{style}m{dot} {line} ")),
                "{line}"
            );
        }
        assert!(styled.contains("\x1b[31mRed: change.\x1b[0m"));
        // The legend wraps between its parts on a narrow terminal.
        let narrow = format_report(
            &report,
            ReportOptions {
                width: Some(40),
                color: true,
                ..Default::default()
            },
        );
        let legend = &narrow[narrow.find("\x1b[31mRed: change.").unwrap()..];
        let legend: Vec<&str> = legend.lines().take(2).collect();
        assert!(
            legend.iter().all(|l| cells(&strip_ansi(l)) <= 40),
            "{legend:?}"
        );
        assert!(legend[1].contains("Dim: no verdict."), "{legend:?}");
        let words = |text: &str| {
            text.lines()
                .filter(|l| !l.starts_with("Red: change."))
                .map(|l| {
                    let l = l.replace('→', "->");
                    let l = l.trim_start_matches(['●', '○']).trim_start();
                    ["change", "consider", "keep", "unknown", "info"]
                        .iter()
                        .find_map(|w| l.strip_prefix(w).filter(|r| r.starts_with(' ')))
                        .unwrap_or(l)
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(words(&strip_ansi(&styled)), words(&plain));
        assert_eq!(
            text(&report, None, true)
                .matches("Identical fixed costs remain unknown")
                .count(),
            2
        );
    }

    #[test]
    fn subagent_lines_are_evidence_that_never_count_as_changes() {
        let mut report = synthetic();
        let before = text(&report, None, false);
        let cost = |usd: f64| Cost {
            known_usd: Usd::from_f64(usd),
            total_usd: Some(Usd::from_f64(usd)),
            unknown: vec![],
            assumptions: vec![],
        };
        report.subagents.push(SubagentCost {
            harness: Harness::Claude,
            profiles: vec!["profile".into()],
            kind: "Explore".into(),
            native: false,
            runs: 3,
            requests: 10,
            priced: 9,
            cost: Cost {
                total_usd: None,
                ..cost(2.0)
            },
            models: vec![
                Share {
                    name: "claude-opus-5-5".into(),
                    count: 9,
                },
                Share {
                    name: "claude-haiku-4-5".into(),
                    count: 1,
                },
            ],
            efforts: vec![Share {
                name: "medium".into(),
                count: 10,
            }],
            sources: vec![SourceShare {
                source: ModelSource::Unknown,
                detail: "the Claude Code version is not recorded".into(),
                runs: 3,
            }],
            cheapest: Some("claude-haiku-5-5".into()),
            reprice: Some(cost(0.5)),
            model_setting: "model: in /synthetic/.claude/agents/Explore.md".into(),
            effort_setting: "effort: in /synthetic/.claude/agents/Explore.md".into(),
        });
        let summary = text(&report, None, false);
        let first = |text: &str| {
            text.lines()
                .find(|l| l.contains("recommended"))
                .map(str::to_string)
        };
        assert_eq!(first(&summary), first(&before));
        let row = report_line(&summary, "Explore subagent");
        assert_eq!(
            (row.mark.as_str(), row.head.as_str()),
            ("info", "Explore subagent costs at least $2")
        );
        let line = row.evidence;
        for part in [
            "3 runs (9 of 10 requests priced) on",
            "claude-opus-5-5 (90%), claude-haiku-4-5 (10%) at medium effort",
            "Model source: unknown, the Claude Code version is not recorded.",
            "On claude-haiku-5-5 the same recorded tokens would cost",
            "Model setting: model: in /synthetic/.claude/agents/Explore.md.",
        ] {
            assert!(line.contains(part), "{part} in {line}");
        }
        assert!(summary.contains(SUBAGENT_EVIDENCE));
        let details = text(&report, None, true);
        assert!(
            details.contains("Same tokens on claude-haiku-5-5: $0.50"),
            "{details}"
        );
        assert!(
            details.contains(
                "Model source: unknown: the Claude Code version is not recorded (3 runs)"
            )
        );
    }

    #[test]
    fn empty_histories_give_a_next_step() {
        let mut report = synthetic();
        report.scan.snapshots.clear();
        report.scan.requests.clear();
        report.scan.coverage = ScanCoverage::default();
        report.checks.clear();
        report.context.clear();
        let summary = text(&report, None, false);
        assert!(summary.contains("No requests found since 2026-11-01 in:"));
        assert!(summary.contains("No transcript histories found."));
        assert!(summary.contains("No setting changes recommended."));
        let details = text(&report, None, true);
        assert!(details.contains("No applicable setting changes"));
        assert!(!details.contains("What each piece of context costs"));
    }

    /// The report the CLI builds from the synthetic Claude fixture with an explicit 5m TTL.
    fn synthetic_report() -> Report {
        let w = Workspace::new();
        w.fixture(w.claude.join("projects/project/main.jsonl"), "claude.jsonl");
        std::fs::write(w.claude.join("settings.json"), r#"{"promptCacheTtl":"5m"}"#).unwrap();
        let mut options = w.scan_options();
        options.discovery.claude_dirs = vec![w.claude.to_string_lossy().into_owned()];
        analyze(AnalyzeOptions {
            scan: scan(&options).unwrap(),
            catalog: &prices(),
            since: None,
            now: instant("2026-11-02T00:00:00Z"),
            max_cost_per_use_usd: 1.0,
        })
        .unwrap()
    }

    fn instant(text: &str) -> Millis {
        text.parse::<jiff::Timestamp>().unwrap().as_millisecond()
    }

    fn ttl_mut(report: &mut Report) -> &mut TtlCheck {
        report
            .checks
            .iter_mut()
            .find_map(|c| match c {
                CheckResult::Ttl(t) => Some(t),
                _ => None,
            })
            .expect("a claude-cache-ttl check")
    }

    #[test]
    fn recommendations_lead_advisory_and_keep_findings_with_separate_baselines_and_complete_narrow_output()
     {
        let mut report = synthetic_report();
        let change = ttl_mut(&mut report);
        change.base.savings_usd = Some(12.34);
        change.base.monthly_savings_usd = Some(24.68);
        let mut advisory = change.clone();
        advisory.base.action = Action::Advisory;
        advisory.base.evidence = Evidence::Guidance;
        advisory.base.current = None;
        advisory.base.recommended = None;
        advisory.base.savings_usd = None;
        advisory.base.monthly_savings_usd = None;
        advisory.base.baseline =
            "synthetic recorded profile, holding other settings constant".into();
        advisory.base.assumptions =
            vec!["Synthetic provider guidance; review low-use components manually.".into()];
        advisory.base.uncertainty =
            vec!["Synthetic incomplete invocation visibility; absence of use is unknown.".into()];
        let mut keep = change.clone();
        keep.base.action = Action::Keep;
        keep.base.current = keep.base.recommended;
        let mut change = change.clone();
        let root = format!(
            "/synthetic/e\u{301}-東京-👩\u{200d}💻/{}",
            "long-profile-name-".repeat(8)
        );
        report.scan.profiles[0].root = root.clone();
        change.base.owner = format!("{root}/settings.json (env.CLAUDE_CODE_PROMPT_CACHE_TTL)");
        let other = Profile {
            id: "another-profile".into(),
            root: "/synthetic/second-profile".into(),
            ..report.scan.profiles[0].clone()
        };
        report.scan.profiles.push(other.clone());
        advisory.base.profile = other.id.clone();
        let values = [
            root,
            change.base.owner.clone(),
            other.root,
            advisory.base.baseline.clone(),
        ]
        .into_iter()
        .chain(advisory.base.assumptions.clone())
        .chain(advisory.base.uncertainty.clone());
        let values: Vec<String> = values.collect();
        report.checks = vec![
            CheckResult::Ttl(advisory),
            CheckResult::Ttl(keep),
            CheckResult::Ttl(change),
        ];
        for width in [28, 40, 80] {
            let text = text(&report, Some(width), true);
            let at = |s: &str| text.find(s).expect(s);
            assert!(at("Recommended changes") < at("Advisory findings"));
            assert!(at("Advisory findings") < at("Keep current settings"));
            for line in text.lines() {
                assert!(
                    unicode_width::UnicodeWidthStr::width(line) <= width,
                    "{line}"
                );
            }
            let compact = compact(&text);
            for value in &values {
                assert!(compact.contains(&super::tests::compact(value)), "{value}");
            }
            assert!(text.contains("$12.34") && text.contains("$24.68"));
            assert!(compact.contains("Current:5m"));
            assert!(compact.contains("Suggested:1h"));
            assert!(compact.contains("[guidance]"));
            assert!(
                compact.contains("Estimatednetsavingvsbaseline:unknownoverwindow;unknown/30days")
            );
            let lower = text.to_lowercase();
            for forbidden in ["total sav", "synthetic_private", "\x1b", "…"] {
                assert!(!lower.contains(forbidden), "{forbidden}");
            }
            assert!(
                text.contains("e\u{301}"),
                "decomposed filename must not be NFC-normalized"
            );
        }
    }

    #[test]
    fn repeated_idle_evidence_is_grouped() {
        let mut report = synthetic();
        let idle = report
            .checks
            .iter_mut()
            .find_map(|c| match c {
                CheckResult::Idle(i) => Some(i),
                _ => None,
            })
            .unwrap();
        let eligible = period(PeriodStatus::Eligible, Some("return"));
        let mut unknown = period(PeriodStatus::Unknown, None);
        unknown.reason = "unresolved synthetic setting";
        let mut periods = vec![eligible; 100];
        periods.push(unknown);
        idle.after_recommendations.periods = periods;
        idle.current_settings.periods.clear();
        let rendered = text(&report, None, true);
        assert!(
            rendered.contains("100 periods; eligible; after-expiry; unobserved; synthetic gap")
        );
        assert!(rendered.contains(
            "1 periods; unknown; after-expiry; unobserved; unresolved synthetic setting"
        ));
        assert!(!rendered.contains("Idle period:"));
        assert!(!rendered.contains("Idle by context:"));
        assert!(rendered.contains("Repeated evidence is grouped"));
    }

    #[test]
    fn details_retain_every_assumption_unknown_cost_witness_and_finding_without_a_top_n_cutoff() {
        let mut report = synthetic_report();
        let result = ttl_mut(&mut report);
        result.cost.one_hour = Cost {
            known_usd: Usd::from_f64(9.87),
            total_usd: None,
            unknown: vec!["synthetic unknown fixed charge".into()],
            assumptions: vec!["synthetic price assumption".into()],
        };
        result.fixed_cost = Cost {
            known_usd: Usd::from_f64(0.0),
            total_usd: None,
            unknown: vec!["synthetic unknown fixed charge".into()],
            assumptions: vec![],
        };
        result.base.assumptions = (0..15)
            .map(|i| format!("synthetic assumption {i}"))
            .collect();
        result
            .base
            .uncertainty
            .push("synthetic final uncertainty".into());
        result.usage_estimates.push(UsageEstimate {
            request: "synthetic-attempt".into(),
            evidence: EstimateEvidence::Unknown,
            basis: vec![],
            part: None,
            max_output_tokens: Some(2048),
            reason: "no supported input estimate",
            assumptions: vec!["synthetic attempt assumption".into()],
        });
        result.cache_evidence.push(CacheReplayEvidence {
            request: "synthetic-cache-request".into(),
            part: 0,
            ttl: Ttl::OneHour,
            state: KnownState::Unknown,
            outcome: ReplayOutcome::Unknown,
            reason: "synthetic unresolved predecessor",
            predecessors: vec![CachePredecessor {
                request: "synthetic-cache-predecessor".into(),
                part: 0,
                time: 0,
                state: KnownState::Unknown,
                prefix_evidence: PrefixEvidence::Unknown,
                cohort_known: false,
                reason: "synthetic witness reason",
            }],
        });
        let result = result.clone();
        for i in 0..12 {
            let mut copy = result.clone();
            copy.base.owner = format!("synthetic-advisory-{i}");
            copy.base.action = Action::Advisory;
            copy.base.evidence = Evidence::Unknown;
            copy.base.current = None;
            copy.base.recommended = None;
            copy.base.savings_usd = None;
            copy.base.monthly_savings_usd = None;
            report.checks.push(CheckResult::Ttl(copy));
        }
        let text = text(&report, Some(120), true);
        let compact = compact(&text);
        let values = result
            .base
            .assumptions
            .iter()
            .chain(&result.base.uncertainty)
            .chain(&result.base.sources)
            .map(String::as_str)
            .chain([
                "synthetic price assumption",
                "synthetic attempt assumption",
                "synthetic-advisory-11",
                "synthetic-attempt: unknown",
                "2048 (not observed output)",
                "unknown ($9.87 known subtotal)",
            ]);
        for value in values {
            assert!(compact.contains(&super::tests::compact(value)), "{value}");
        }
        assert!(text.contains(&*result.cache_evidence[0].request));
        assert!(text.contains("Predecessor:"));
        let unknown_fixed = report
            .checks
            .iter()
            .filter(|c| matches!(c, CheckResult::Ttl(t) if t.fixed_cost.total_usd.is_none()))
            .count();
        assert_eq!(
            text.matches("Identical fixed costs remain unknown").count(),
            unknown_fixed
        );
        assert!(format_report(&report, ReportOptions::default()).contains("--details"));
        let styled = format_report(
            &report,
            ReportOptions {
                details: true,
                width: Some(120),
                color: true,
                ..Default::default()
            },
        );
        assert!(styled.contains("\x1b["));
        assert_eq!(strip_ansi(&styled), text);
    }

    #[test]
    fn terminal_controls_in_all_metadata_and_prose_are_escaped() {
        let mut report = synthetic_report();
        let hostile = "synthetic\x1b]8;;https://invalid\x07\r\n\t\u{9b}31m\u{202e}reversed\u{2066}\u{200e}\u{200f}\u{61c}";
        let result = ttl_mut(&mut report);
        result.base.owner = hostile.into();
        result.base.baseline = hostile.into();
        result.base.assumptions.push(hostile.into());
        result.base.uncertainty.push(hostile.into());
        report.scan.profiles[0].root = hostile.into();
        report.prices.source = hostile.into();
        let text = text(&report, None, true);
        let unsafe_char = |c: char| {
            let code = c as u32;
            (code <= 0x1f && c != '\n')
                || (0x7f..=0x9f).contains(&code)
                || matches!(
                    c,
                    '\u{202e}' | '\u{2066}' | '\u{200e}' | '\u{200f}' | '\u{61c}'
                )
        };
        assert!(!text.chars().any(unsafe_char), "{text:?}");
        assert!(compact(&text).contains("\\u009b31m\\u202ereversed\\u2066"));
    }

    #[test]
    fn an_unresolved_fast_mode_owner_stays_beside_a_resolved_one_and_only_vanished_directories_are_hidden()
     {
        let mut report = synthetic_report();
        let resolved = report
            .checks
            .iter()
            .find_map(|c| match c {
                CheckResult::Speed(s) => Some(s.clone()),
                _ => None,
            })
            .expect("a fast-mode check");
        assert_eq!(resolved.base.action, Action::Keep);
        let mut project = resolved;
        project.base.action = Action::Advisory;
        project.base.current = None;
        project.base.recommended = None;
        project.base.owner = "/synthetic/project/.codex/config.toml".into();
        project.reason = "synthetic project override ownership is unresolved".into();
        let mut vanished = project.clone();
        vanished.base.owner = "/synthetic/gone/.claude/settings.json".into();
        vanished.reason =
            "recorded working directory no longer exists; current ownership unresolved".into();
        report.checks.push(CheckResult::Speed(project));
        report.checks.push(CheckResult::Speed(vanished));
        let text = text(&report, None, false);
        let fast: Vec<(String, String)> = report_lines(&text)
            .into_iter()
            .filter(|r| r.head.starts_with("Fast mode"))
            .map(|r| (r.mark, r.head))
            .collect();
        assert_eq!(
            fast,
            [
                ("keep".to_string(), "Fast mode off".to_string()),
                ("unknown".into(), "Fast mode unknown".into())
            ],
            "{text}"
        );
        assert!(
            text.to_lowercase()
                .contains("synthetic project override ownership is unresolved"),
            "{text}"
        );
        assert!(!text.contains("working directory no longer exists"));
    }

    #[test]
    fn short_windows_scale_to_30_days_snippets_stay_valid_json_when_narrow_and_details_count_feature_changes()
     {
        let mut report = synthetic_report();
        let check = ttl_mut(&mut report);
        check.base.action = Action::Change;
        check.base.current = Some(Ttl::FiveMinutes);
        check.base.recommended = Some(Ttl::OneHour);
        let since = instant("2026-11-01T00:00:00Z");
        check.base.since = Some(since);
        check.base.until = Some(since + 3_600_000);
        check.base.savings_usd = Some(1.0);
        check.base.monthly_savings_usd = Some(720.0);
        assert_eq!(
            report_line(&text(&report, None, false), "Main-agent prompt cache TTL").head,
            "Main-agent prompt cache TTL 5m -> 1h saves $720"
        );

        let name = "google-drive-comments";
        report.checks.clear();
        let skill = ContextCost {
            harness: Harness::Claude,
            largest_characters: None,
            skill: None,
            details: vec![],
            instruction_size: None,
            profiles: vec![report.scan.profiles[0].id.clone()],
            kind: ContextKind::Skill,
            name: name.into(),
            control: Some(Control {
                path: vec!["skillOverrides".into(), name.into()],
                value: ControlValue::Text("user-invocable-only".into()),
                append: false,
            }),
            requests: 10,
            priced: 10,
            conditional_requests: 0,
            tier_unknown_requests: 0,
            assumptions: vec![],
            tokens: Some(100),
            cost_usd: Usd::from_f64(5.0),
            estimated_usd: Some(5.0),
            calls: Some(0),
            last_used: None,
            current: true,
            cost_per_use_usd: None,
            verdict: Some(Verdict::Off),
        };
        report.context = vec![skill.clone()];
        // An edit is never wrapped, so it can be copied at any width.
        let narrow = changes(&report, Some(40));
        assert!(
            narrow
                .lines()
                .any(|l| l.trim()
                    == format!(r#""skillOverrides": {{"{name}": "user-invocable-only"}}"#)),
            "{narrow}"
        );
        // Feature and context costs are totals over the window, however short.
        let line = report_line(&text(&report, None, false), "Skill google-drive-comments");
        assert_eq!(
            line.head,
            "Skill google-drive-comments on -> typed only costs $5"
        );
        assert!(line.evidence.starts_with("The model never used it."));
        // First sent during the window, so the window cannot rule out a use.
        let mut late = report.clone();
        late.context[0].verdict = Some(Verdict::Consider);
        assert!(text(&late, None, false).contains(
            "The model never used it. It was first sent during the window, so the window cannot rule out a use. Hide it from the model:"
        ));
        late.scan.snapshots[0].skipped_lines = 1;
        late.scan.snapshots[0].profiles = late.context[0].profiles.clone();
        assert!(text(&late, None, false).contains(
            "The model never used it. An unreadable transcript line could hold a use. Hide it from the model:"
        ));
        report.scan.since = Some(instant(&report.generated_at) - 3_600_000);
        report.context.push(ContextCost {
            kind: ContextKind::System,
            name: "Claude Code system prompt".into(),
            control: None,
            cost_usd: Usd::from_f64(1.0),
            estimated_usd: Some(1.0),
            calls: None,
            cost_per_use_usd: None,
            verdict: None,
            ..skill
        });
        assert_eq!(
            report_line(&text(&report, None, false), "System prompt").head,
            "System prompt and core tools costs $1"
        );
        let details = text(&report, Some(200), true);
        assert!(details.contains("1 recommended change"));
        assert!(!details.contains("No applicable"));
    }
}
