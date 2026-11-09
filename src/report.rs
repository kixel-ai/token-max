//! The terminal report: one verdict per setting by default, every piece of evidence in details.

use std::fmt::Write as _;

use indexmap::{IndexMap, IndexSet};
use serde::Serialize;
use serde_json::Value;

use crate::context::{control_text, settings_patch};
use crate::model::{
    Action, BackgroundCheck, BackgroundFeature, BashCapCheck, CacheReplayEvidence, CheckBase,
    CheckResult, ContextCost, ContextKind, Control, Cost, Coverage, EstimateEvidence, Evidence,
    Millis, PeriodStatus, Profile, Report, RequestEvidence, Ttl, UsageEstimate, Verdict,
};
use crate::safe::{display, iso, parse_instant, terminal_text};
use crate::terminal::{WrapOptions, wrap_text};

/// How to lay out the text report; presentation never changes analysis or the JSON report.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReportOptions {
    /// Terminal width to wrap to; None keeps each line whole.
    pub width: Option<usize>,
    /// Style headings and verdicts with ANSI colors.
    pub color: bool,
    /// Use Unicode decorations such as arrows.
    pub unicode: bool,
    /// Show every check's full evidence instead of one line per setting.
    pub details: bool,
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
        None => format!("unknown ({} known subtotal)", usd(cost.known_usd)),
        Some(total) => usd(total),
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

fn about(value: f64) -> String {
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

/// The serialized name of a schema enum, such as "eligible".
fn label<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(name)) => name,
        _ => unreachable!("schema enums serialize as strings"),
    }
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
    Speed(&'a str),
    Toggle(bool),
    Cap(Millis),
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
    }
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
        Some(Setting::Speed(s)) => s.into(),
        Some(Setting::Toggle(b)) => if b { "on" } else { "off" }.into(),
        Some(Setting::Cap(ms)) => format!("{ms} ms"),
    }
}

/// A value as the summary prints it.
fn setting_value(value: Option<Setting<'_>>) -> String {
    match value {
        None => "unknown".into(),
        Some(Setting::Idle(b) | Setting::Toggle(b)) => if b { "on" } else { "off" }.into(),
        Some(Setting::Speed("standard")) => "off".into(),
        Some(Setting::Speed(s)) => s.into(),
        Some(Setting::Window(n)) => format!("{} tokens", count(n)),
        Some(Setting::Ttl(t)) => t.as_str().into(),
        Some(Setting::Cap(ms)) => duration(ms),
    }
}

/// The text being built, one wrapped line at a time.
struct Out<'a> {
    text: String,
    options: ReportOptions,
    arrow: &'static str,
    home: Option<String>,
    profiles: IndexMap<&'a str, &'a Profile>,
}

impl<'a> Out<'a> {
    fn line(&mut self, text: &str, indent: usize, style: u8) {
        // Escape metadata before wrapping or adding our own ANSI. Long paths are
        // hard-wrapped, never shortened; wide characters use terminal cell widths.
        let wrapped = wrap_text(
            &terminal_text(text),
            WrapOptions {
                width: self.options.width,
                indent,
                continuation: None,
            },
        );
        for part in wrapped.split('\n') {
            if self.options.color && style != 0 {
                let _ = write!(self.text, "\x1b[{style}m{part}\x1b[0m");
            } else {
                self.text.push_str(part);
            }
            self.text.push('\n');
        }
    }
    fn blank(&mut self) {
        self.line("", 0, 0);
    }
    /// A line kept whole at any width, such as a JSON snippet.
    fn raw(&mut self, text: &str) {
        self.text.push_str(&terminal_text(text));
        self.text.push('\n');
    }
    fn section(&mut self, title: &str) {
        self.blank();
        self.line(title, 0, 1);
    }
    fn field(&mut self, name: &str, value: &str) {
        self.line(&format!("{name}: {value}"), 2, 0);
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
        for evidence in values {
            self.field(
                "Cache evidence",
                &format!(
                    "{}/{} {}: {}, {}; {}",
                    evidence.request,
                    evidence.part,
                    evidence.ttl.as_str(),
                    label(&evidence.state),
                    label(&evidence.outcome),
                    evidence.reason
                ),
            );
            for p in &evidence.predecessors {
                self.field(
                    "Predecessor",
                    &format!(
                        "{}/{}: {}, prefix {}, cohort {}, {}; {}",
                        p.request,
                        p.part,
                        label(&p.state),
                        label(&p.prefix_evidence),
                        if p.cohort_known {
                            "known"
                        } else {
                            "incomplete"
                        },
                        instant(Some(p.time)),
                        p.reason
                    ),
                );
            }
            if evidence.predecessors.is_empty() {
                self.field("Predecessor", "unrecorded");
            }
        }
    }
    fn home(&self, path: &str) -> String {
        match &self.home {
            Some(dir)
                if path == dir
                    || path
                        .strip_prefix(dir.as_str())
                        .is_some_and(|rest| rest.starts_with('/')) =>
            {
                format!("~{}", &path[dir.len()..])
            }
            _ => path.to_string(),
        }
    }
    /// A profile by harness and quoted root, for evidence cards.
    fn profile_name(&self, id: &str) -> String {
        match self.profiles.get(id) {
            Some(p) => format!("{} {}", label(&p.harness), display(&p.root)),
            None => display(id),
        }
    }
    /// A profile by harness and home-relative root, for the summary.
    fn short_profile(&self, id: &str) -> String {
        match self.profiles.get(id) {
            Some(p) => format!("{} {}", label(&p.harness), self.home(&p.root)),
            None => self.home(id),
        }
    }
}

/// Render all findings in a wrapping flow, with complete evidence available in details.
pub fn format_report(report: &Report, options: ReportOptions) -> String {
    let mut profiles = IndexMap::new();
    for p in &report.scan.profiles {
        profiles.insert(p.id.as_str(), p);
    }
    let mut out = Out {
        text: String::new(),
        options,
        arrow: if options.unicode { "→" } else { "->" },
        home: std::env::home_dir().map(|dir| dir.to_string_lossy().into_owned()),
        profiles,
    };
    if options.details {
        details(report, &mut out);
    } else {
        summary(report, &mut out);
    }
    out.text
}

fn generated_at(report: &Report) -> Millis {
    parse_instant(&report.generated_at).expect("reports record an ISO generation time")
}

fn details(report: &Report, out: &mut Out<'_>) {
    let arrow = out.arrow;
    out.line("token-max", 0, 1);
    out.line("Public API-price estimates, not subscription bills.", 0, 0);
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
        + report
            .context
            .iter()
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
        out.line("No transcript histories found. Select an existing profile with --claude-dir or --codex-dir; run after using that agent.", 0, 0);
    } else if coverage.requests == 0 {
        out.line(
            "No selected requests. Try an earlier --since or select another profile.",
            0,
            0,
        );
    } else if report.checks.is_empty() && !report.context.iter().any(|c| c.verdict.is_some()) {
        out.line("No applicable checks for the selected requests. Usage coverage does not establish that every setting is optimal.", 0, 0);
    } else if changes == 0 {
        out.line(
            if report.checks.iter().any(|c| c.action() == Action::Advisory) {
                "Review advisory findings for unresolved settings or missing evidence."
            } else {
                "Available checks favor current settings. Coverage limits still apply."
            },
            0,
            0,
        );
    }
    if !report.checks.is_empty() {
        out.line(
            "Each check has its own baseline. Do not sum independent savings.",
            0,
            0,
        );
    }

    let groups = [
        (Action::Change, "Recommended changes", 36),
        (Action::Advisory, "Advisory findings", 33),
        (Action::Keep, "Keep current settings", 0),
    ];
    for (action, title, style) in groups {
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
            card(result, action, style, out);
        }
    }

    context_section(report, out);

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
    }
    if report.historical.is_empty() {
        out.line(
            "Recorded-request cost: unknown; no selected request evidence.",
            0,
            0,
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
        "All check evidence shown. --json provides the complete structured report (schema version 1).",
        0,
        0,
    );
}

/// One check's full evidence card.
fn card(result: &CheckResult, action: Action, style: u8, out: &mut Out<'_>) {
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
        style,
    );
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
        if r.check == "fast-mode" {
            "Estimated rate-only saving vs current speed"
        } else {
            "Estimated net saving vs baseline"
        },
        &format!(
            "{} over window; {} / 30 days",
            estimate(r.savings_usd),
            estimate(r.monthly_savings_usd)
        ),
    );
    match result {
        CheckResult::Background(b) => {
            out.field("Turn off", &background_off(b, out));
            out.field(
                "Estimated requests",
                &b.generations
                    .map_or_else(|| "unknown".into(), |n| n.to_string()),
            );
            out.cost("Estimated request cost", &b.cost);
            out.cost("Cache rewrites its reads avoided", &b.lost_warming);
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
            "Review evidence and resolve current ownership/coverage before changing a setting; advisory findings do not affect the exit code.",
        );
    }
    for id in r.history_profiles {
        out.field("History profile", &out.profile_name(id));
    }
    match result {
        CheckResult::Background(_) => {}
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
                for size in &scenario.by_context {
                    out.field(
                        "Idle by context",
                        &format!(
                            "{} tokens; {} periods; on minus off {}",
                            or_unknown(size.tokens),
                            size.periods,
                            estimate(size.delta_usd)
                        ),
                    );
                }
                for p in &scenario.periods {
                    out.field(
                        "Idle period",
                        &format!(
                            "{}; resume {}; context {} tokens; gap {}ms; {}; {}; {}; {}; boundary {}",
                            p.request,
                            p.resume.as_deref().unwrap_or("not observed"),
                            or_unknown(p.context_tokens),
                            p.gap_ms,
                            label(&p.status),
                            label(&p.return_timing),
                            p.reason,
                            label(&p.outcome),
                            p.event.as_deref().unwrap_or("unobserved")
                        ),
                    );
                    out.field(
                        "Activation",
                        &format!(
                            "{}; {}; version {}; client {}; flags {}",
                            or_unknown(p.activation.map(js_number)),
                            p.activation_basis,
                            or_unknown(p.version.as_deref()),
                            or_unknown(p.client.as_deref()),
                            or_unknown(p.flag_state.as_deref())
                        ),
                    );
                    out.cost("Idle summary", &p.summary_cost);
                    if let Some(c) = &p.calibration {
                        out.field(
                            "Idle geometry",
                            &format!(
                                "{} summary output tokens; {} full resumed prefix tokens; boundaries {}",
                                c.summary_tokens,
                                c.prefix_tokens,
                                c.events.join(", ")
                            ),
                        );
                    }
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
                for added in &scenario.added {
                    out.field(
                        "Added compaction",
                        &format!(
                            "at {}; {} summary input tokens; {} summary output tokens; {} resumed-prefix tokens",
                            added.request,
                            js_number(added.input_tokens as f64),
                            js_number(added.summary_tokens as f64),
                            js_number(added.prefix_tokens as f64)
                        ),
                    );
                    for basis in &added.basis {
                        out.field(
                            "Geometry basis",
                            &format!(
                                "boundary {}; resumed request {}",
                                basis.event, basis.request
                            ),
                        );
                    }
                    out.cost("Added summary", &added.summary_cost);
                    out.cost("Resumed prefix", &added.prefix_cost);
                }
                out.witnesses(&scenario.cache_evidence);
            }
        }
    }
    for e in r.usage_estimates {
        let basis = e.basis.join(", ");
        out.field(
            "Attempt",
            &format!(
                "{}: {}; basis {}; {}; output budget {} (not observed output)",
                e.request,
                label(&e.evidence),
                if basis.is_empty() {
                    "unavailable"
                } else {
                    &basis
                },
                e.reason,
                or_unknown(e.max_output_tokens)
            ),
        );
        out.notes("Attempt assumption", &e.assumptions);
    }
    match result {
        CheckResult::Ttl(t) => out.witnesses(&t.cache_evidence),
        CheckResult::Compaction(c) => out.witnesses(&c.cache_evidence),
        CheckResult::Idle(_)
        | CheckResult::Speed(_)
        | CheckResult::Background(_)
        | CheckResult::BashCap(_) => {}
    }
    out.notes("Assumption", r.assumptions);
    out.notes("Uncertainty", r.uncertainty);
    out.notes("Source", r.sources);
}

fn reason(result: &CheckResult, r: &Common<'_>) -> String {
    match result {
        CheckResult::Speed(s) => return s.reason.clone(),
        CheckResult::Background(b) => {
            return match r.action {
                Action::Advisory => background_unknown(b).into(),
                _ if r.current == Some(Setting::Toggle(false)) => "Off; it costs nothing.".into(),
                _ => "Estimated cost against --max-cost-per-use, counting its unrecorded uses as one.".into(),
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
            let models: Vec<&str> = c.models.iter().flatten().map(String::as_str).collect();
            if models.is_empty() {
                "Auto-compact window".into()
            } else {
                format!("Auto-compact window ({})", models.join(", "))
            }
        }
        CheckResult::Idle(_) => "Idle compaction".into(),
        CheckResult::Speed(_) => "Fast mode".into(),
        CheckResult::BashCap(_) => "Bash timeout cap".into(),
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
    match b.feature {
        BackgroundFeature::GoalCheckIns => {
            "Cost unknown: check-in turns are not marked in transcripts."
        }
        BackgroundFeature::AutoRecaps => "Cost unknown: Codex does not record recap requests.",
        _ => "Cost unknown: none of the turns where it would have run have recorded usage.",
    }
}

/// One plain sentence on why a check could not reach a verdict.
fn unresolved(result: &CheckResult, r: &Common<'_>) -> String {
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
                    "No window is set for this model and its default is not documented."
                } else {
                    "The current window setting could not be resolved."
                }
                .into();
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
        CheckResult::Speed(_) | CheckResult::Background(_) | CheckResult::BashCap(_) => {
            unreachable!("handled above")
        }
    }
}

fn piece_label(c: &ContextCost, out: &Out<'_>) -> String {
    match c.kind {
        ContextKind::Tool => format!("{} tool", c.name),
        ContextKind::McpServer => format!("MCP server {}", c.name),
        ContextKind::Skill => format!("Skill {}", c.name),
        ContextKind::Instructions => out.home(&c.name),
        ContextKind::System | ContextKind::Other => c.name.clone(),
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

/// A sum of context costs that never turns an unknown piece into $0.
fn known_total(values: &[Option<f64>]) -> String {
    let known: Vec<f64> = values.iter().flatten().copied().collect();
    if known.is_empty() {
        return "cost unknown".into();
    }
    let total = about(known.iter().fold(0.0, |n, c| n + c));
    let missing = values.len() - known.len();
    if missing > 0 {
        format!(
            "at least {total}, with {} piece{} that could not be priced",
            count(missing as u64),
            if missing == 1 { "" } else { "s" }
        )
    } else {
        total
    }
}

/// The profile a piece of context is filed under: the first of its sorted set.
fn first_profile(c: &ContextCost) -> &str {
    c.profiles.first().map_or("", String::as_str)
}

/// The default report: one verdict per setting, in plain words.
fn summary(report: &Report, out: &mut Out<'_>) {
    let arrow = out.arrow;
    let coverage = &report.scan.coverage;
    let requests = &report.scan.requests;
    let since = report.scan.since.or(requests.first().map(|r| r.time));
    let until = generated_at(report);
    let many = report.scan.profiles.len() > 1;
    out.line("token-max", 0, 1);
    out.line(
        "Estimated with public API prices, not your subscription bill. Nothing was changed.",
        0,
        0,
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
        0,
    );
    for p in &report.scan.profiles {
        out.line(&out.short_profile(&p.id), 2, 0);
    }
    if coverage.missing_usage > 0 {
        out.line(
            &format!(
                "{} requests have no recorded usage and are left out of comparisons.",
                count(coverage.missing_usage)
            ),
            0,
            0,
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
            0,
        );
    }
    if report.scan.snapshots.is_empty() {
        out.line("No transcript histories found. Select a profile with --claude-dir or --codex-dir after using that agent.", 0, 0);
    } else if coverage.requests == 0 {
        out.line("Try an earlier --since or select another profile.", 0, 0);
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
    // A comparison with no requests is not a verdict, whatever the tie-break said.
    // Window coverage counts only changed parts, so zero there can still be a keep.
    // Fast mode warns from current settings alone, so it never needs requests.
    let action = |(result, r): &(&CheckResult, Common<'_>)| {
        let empty =
            matches!(result, CheckResult::Ttl(_) | CheckResult::Compaction(_)) && r.requests == 0;
        if empty { Action::Advisory } else { r.action }
    };
    let decided: std::collections::HashSet<String> = checks
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
    let name = |r: &Common<'_>, result: &CheckResult, out: &Out<'_>| {
        let mut name = setting_name(result);
        if many {
            let _ = write!(name, " [{}]", out.short_profile(r.profile));
        }
        name
    };
    let span = since.filter(|&s| until > s).map(|s| until - s);
    // Everything is shown per 30 days so lines with different windows compare directly.
    // Scaled by the real duration, however short, as monthlySavingsUsd is.
    let monthly = |usd: f64, from: Option<Millis>, to: Option<Millis>| match (from, to) {
        (Some(from), Some(to)) if to > from => about((usd * 30.0 * DAY) / (to - from) as f64),
        _ => about(usd),
    };
    let settings_file: IndexMap<&str, String> = report
        .scan
        .profiles
        .iter()
        .map(|p| (p.id.as_str(), out.home(&p.settings_file)))
        .collect();
    let suffix = |c: &ContextCost, out: &Out<'_>| {
        if many {
            format!(" [{}]", out.short_profile(first_profile(c)))
        } else {
            String::new()
        }
    };
    let pieces: Vec<&ContextCost> = report
        .context
        .iter()
        .filter(|c| c.verdict.is_some())
        .collect();
    let off: Vec<&ContextCost> = pieces
        .iter()
        .copied()
        .filter(|c| c.verdict == Some(Verdict::Off))
        .collect();
    let changes: Vec<&(&CheckResult, Common<'_>)> = checks
        .iter()
        .filter(|c| action(c) == Action::Change)
        .collect();
    let total = changes.len() + off.len();
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
        1,
    );
    out.line(
        &format!(
            "Dollar amounts are per 30 days at this window's rate. A tool, skill or MCP server is worth turning off when it cost more than {} per use (--max-cost-per-use).",
            about(report.max_cost_per_use_usd)
        ),
        0,
        0,
    );
    // One coverage note for every feature line instead of one per line.
    let carried: u64 = pieces.iter().map(|c| c.requests).sum();
    let priced: u64 = pieces.iter().map(|c| c.priced).sum();
    if priced < carried {
        out.line(
            &format!(
                "Feature costs are scaled up from the {}% of their requests that could be priced.",
                percent(priced, carried)
            ),
            0,
            0,
        );
    }
    let piece = |c: &ContextCost, out: &Out<'_>| {
        let label = piece_label(c, out) + &suffix(c, out);
        let cost = format!(
            "Costs {}",
            monthly(c.estimated_usd.unwrap_or(0.0), since, Some(until))
        );
        let calls = c.calls.unwrap_or(0);
        let uses = if calls == 0 {
            if c.kind == ContextKind::Skill {
                "the model never used it".to_string()
            } else {
                "never used".to_string()
            }
        } else {
            format!(
                "{} {} time{} ({} each)",
                if c.kind == ContextKind::Skill {
                    "the model used it"
                } else {
                    "used"
                },
                count(calls),
                if calls == 1 { "" } else { "s" },
                about(c.cost_per_use_usd.unwrap_or(0.0))
            )
        };
        let change = if c.kind == ContextKind::Skill {
            format!("on {arrow} only when you type /{}", c.name)
        } else {
            format!("on {arrow} off")
        };
        if c.verdict == Some(Verdict::Off) {
            format!("{label}: {change}. {cost}; {uses}.")
        } else {
            format!("{label}: on. {cost}; {uses}.")
        }
    };
    // What long foreground Bash waits cost, scaled like every other line.
    let waits = |b: &BashCapCheck, r: &Common<'_>| {
        let w = &b.waits;
        if w.long == 0 {
            return "No foreground Bash wait outlasted the cache.".to_string();
        }
        let cost = &w.miss_cost;
        let cost = match cost.total_usd {
            Some(usd) => monthly(usd, r.since, r.until),
            None if cost.known_usd > 0.0 => {
                format!("at least {}", monthly(cost.known_usd, r.since, r.until))
            }
            None => "an unknown amount".into(),
        };
        format!(
            "{} foreground Bash wait{} outlasted the cache ({} hit the timeout, {} polled in a loop); the cache misses cost {cost}.",
            count(w.long),
            if w.long == 1 { "" } else { "s" },
            count(w.timed_out),
            count(w.polling)
        )
    };
    let check_line = |c: &(&CheckResult, Common<'_>), out: &Out<'_>| {
        let (result, r) = c;
        let head = format!("{}: {}", name(r, result, out), setting_value(r.current));
        match action(c) {
            Action::Change => {
                let saves = match (r.savings_usd, result) {
                    (Some(savings), _) => format!(
                        " Saves {}{}.",
                        match r.monthly_savings_usd {
                            None => monthly(savings, r.since, r.until),
                            Some(m) => about(m),
                        },
                        coverage_note(r)
                    ),
                    (None, CheckResult::Speed(s)) => format!(" {}.", capitalize(&s.reason)),
                    (None, CheckResult::BashCap(b)) => {
                        let mut text = format!(
                            " A wait that long outlasts the {} cache. {}",
                            b.ttl.map_or("unknown", Ttl::as_str),
                            waits(b, r)
                        );
                        if b.replacement.contains("BASH_DEFAULT_TIMEOUT_MS") {
                            text += " Lower BASH_DEFAULT_TIMEOUT_MS to match.";
                        }
                        text
                    }
                    (None, _) => String::new(),
                };
                if let CheckResult::Background(b) = result {
                    return format!(
                        "{head} {arrow} off.{saves} Its uses are not recorded. {}.",
                        background_off(b, out)
                    );
                }
                return format!(
                    "{head} {arrow} {}.{saves} Set in {}.",
                    setting_value(r.recommended),
                    out.home(r.owner)
                );
            }
            Action::Advisory => {
                if let CheckResult::Background(b) = result {
                    return format!(
                        "{head}. {} {} to turn it off.",
                        background_unknown(b),
                        background_off(b, out)
                    );
                }
                return format!(
                    "{head}. {}{}",
                    unresolved(result, r),
                    context_sizes(report, result, r.profile)
                );
            }
            Action::Keep => {}
        }
        match result {
            CheckResult::Background(_) if r.current != Some(Setting::Toggle(true)) => {
                format!("{head}. Costs nothing; set in {}.", out.home(r.owner))
            }
            CheckResult::Background(b) if b.generations == Some(0) => format!(
                "{head}. No recorded interactive terminal turn would have run it, so it cost nothing. {} to turn it off.",
                background_off(b, out)
            ),
            CheckResult::Background(b) => format!(
                "{head}. Costs {}{}; its uses are not recorded. {} to turn it off.",
                match r.monthly_savings_usd {
                    Some(m) => about(m),
                    None => monthly(r.savings_usd.unwrap_or(0.0), r.since, r.until),
                },
                coverage_note(r),
                background_off(b, out)
            ),
            CheckResult::Ttl(t) if t.one_hour_savings_usd != 0.0 => {
                let other = if r.current == Some(Setting::Ttl(Ttl::OneHour)) {
                    "5m"
                } else {
                    "1h"
                };
                format!(
                    "{head}. Saves {} over {other}{}.",
                    monthly(t.one_hour_savings_usd.abs(), r.since, r.until),
                    coverage_note(r)
                )
            }
            CheckResult::Compaction(_) => {
                format!("{head}. No other window was cheaper on recorded sessions.")
            }
            CheckResult::BashCap(b) => format!(
                "{head}. Under the {} cache TTL. {}",
                b.ttl.map_or("unknown", Ttl::as_str),
                waits(b, r)
            ),
            CheckResult::Idle(_) => format!(
                "{head}. {}",
                if r.current == Some(Setting::Idle(true)) {
                    "Summarizing idle sessions cost less than re-reading them."
                } else {
                    "Summarizing idle sessions would not have saved money."
                }
            ),
            _ => format!("{head}."),
        }
    };
    if total > 0 {
        out.section("Change");
        for c in &changes {
            let text = check_line(c, out);
            out.line(&text, 2, 36);
        }
        for c in &off {
            let text = piece(c, out);
            out.line(&text, 2, 36);
        }
    }
    let keeps: Vec<&(&CheckResult, Common<'_>)> = checks
        .iter()
        .filter(|c| action(c) == Action::Keep)
        .collect();
    let kept: Vec<&ContextCost> = pieces
        .iter()
        .copied()
        .filter(|c| c.verdict == Some(Verdict::Keep))
        .collect();
    if !keeps.is_empty() || !kept.is_empty() {
        out.section("Keep");
        for c in keeps {
            let text = check_line(c, out);
            out.line(&text, 2, 0);
        }
        for c in kept {
            let text = piece(c, out);
            out.line(&text, 2, 0);
        }
    }
    let unknown: Vec<&(&CheckResult, Common<'_>)> = checks
        .iter()
        .filter(|c| action(c) == Action::Advisory)
        .collect();
    // Switchable context still sent today whose requests could not be priced at all.
    let unpriced: Vec<&ContextCost> = report
        .context
        .iter()
        .filter(|c| c.current && c.control.is_some() && c.estimated_usd.is_none())
        .collect();
    if !unknown.is_empty() || !unpriced.is_empty() {
        out.section("Not enough evidence");
        for c in unknown {
            let text = check_line(c, out);
            out.line(&text, 2, 0);
        }
        for c in unpriced {
            let text = format!(
                "{}{}: on. Cost unknown: none of its requests could be priced.",
                piece_label(c, out),
                suffix(c, out)
            );
            out.line(&text, 2, 0);
        }
    }
    // Context with no switch: worth seeing, but not a setting to change.
    let fixed: Vec<&ContextCost> = report
        .context
        .iter()
        .filter(|c| c.current && c.control.is_none() && c.estimated_usd != Some(0.0))
        .collect();
    if !fixed.is_empty() {
        out.section("No setting turns these off");
        for c in fixed.iter().filter(|c| c.kind == ContextKind::Instructions) {
            let text = format!(
                "{}: {}. Shorten it, or move parts into path-scoped rules or skills.",
                out.home(&c.name),
                c.estimated_usd.map_or_else(
                    || "cost unknown".into(),
                    |usd| format!("costs {}", monthly(usd, since, Some(until)))
                )
            );
            out.line(&text, 2, 0);
        }
        // Scaled to 30 days like every other line, without counting unknowns as $0.
        let scale = span.map_or(1.0, |span| (30.0 * DAY) / span as f64);
        let core: Vec<Option<f64>> = fixed
            .iter()
            .filter(|c| c.kind != ContextKind::Instructions)
            .map(|c| c.estimated_usd.map(|usd| usd * scale))
            .collect();
        if !core.is_empty() {
            out.line(
                &format!(
                    "System prompt, core tools such as Bash and Read, and the skill and subagent lists: {}.",
                    known_total(&core)
                ),
                2,
                0,
            );
        }
    }
    let mut files: IndexMap<&str, Vec<&Control>> = IndexMap::new();
    for c in &off {
        let file = settings_file
            .get(first_profile(c))
            .map_or("settings.json", String::as_str);
        files.entry(file).or_default().extend(c.control.as_ref());
    }
    for (file, controls) in files {
        out.blank();
        out.line(
            &format!(
                "To turn these off, merge this into {file}, adding to any lists already there:"
            ),
            0,
            0,
        );
        // Never wrapped, so the snippet stays valid JSON at any terminal width.
        let patch =
            serde_json::to_string_pretty(&settings_patch(controls)).expect("JSON values serialize");
        for row in patch.split('\n') {
            out.raw(&format!("  {row}"));
        }
    }
    if span.is_some() && !pieces.is_empty() {
        out.line(
            "Context costs assume nothing else changes when a piece is removed; cache writes and compaction can shift.",
            0,
            0,
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
        0,
    );
    out.line(
        "Each line compares one setting with everything else unchanged; savings do not add up.",
        0,
        0,
    );
    out.line(
        "--details shows the evidence and assumptions behind each line; --json is for scripts.",
        0,
        0,
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
            "Sent with every request whether or not it is used, priced over the same window. Recommended off when it cost more than {} per use.",
            about(report.max_cost_per_use_usd)
        ),
        0,
        0,
    );
    for c in &report.context {
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
        out.line(&text, 2, 0);
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
            0,
        );
        let text = match &c.control {
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
                    if c.verdict == Some(Verdict::Off) {
                        "Recommended off"
                    } else {
                        "Turn off"
                    },
                    control_text(control)
                )
            }
            None if c.kind == ContextKind::Instructions => {
                "Turn off: trim or scope this file".into()
            }
            None => "No setting turns this off".into(),
        };
        out.line(&text, 6, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use crate::reader::scan;
    use crate::replay::{AnalyzeOptions, analyze};
    use crate::test_support::{Workspace, make_profile, make_request, prices};

    const NOV1: Millis = 1_793_491_200_000;
    const DAY_MS: Millis = 86_400_000;
    const ROOT: &str = "/synthetic/home/.claude";

    fn cost(total: Option<f64>, known: f64) -> Cost {
        Cost {
            known_usd: known,
            total_usd: total,
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
                reason: "no supported input estimate".into(),
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
                    reason: "synthetic witness reason".into(),
                }],
                outcome: ReplayOutcome::Unknown,
                reason: "synthetic unresolved predecessor".into(),
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
                    reason: "synthetic geometry".into(),
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
            resume: resume.map(String::from),
            session: "session".into(),
            context_tokens: Some(180_000),
            gap_ms: 3_700_000,
            status,
            reason: "synthetic gap".into(),
            confirmed: status == PeriodStatus::Eligible,
            return_timing: ReturnTiming::AfterExpiry,
            outcome: PeriodOutcome::Unobserved,
            event: None,
            version: Some("2.1.314".into()),
            client: None,
            flag_state: Some("on".into()),
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
                flag_state: "on".into(),
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
        let cost_per_use = calls.and_then(|c| estimated.map(|e| e / c.max(1) as f64));
        ContextCost {
            profiles: vec!["profile".into()],
            kind,
            name: name.into(),
            requests: 100,
            priced: if estimated.is_some() { 90 } else { 0 },
            tokens: estimated.map(|_| 12_345),
            cost_usd: estimated.map_or(0.0, |e| e * 0.9),
            estimated_usd: estimated,
            calls,
            last_used: calls.filter(|&c| c > 0).map(|_| NOV1 + 5 * DAY_MS),
            current: true,
            cost_per_use_usd: cost_per_use,
            verdict: match (&control, cost_per_use) {
                (Some(_), Some(c)) => Some(if c > 1.0 { Verdict::Off } else { Verdict::Keep }),
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
                r.id = format!("request-{i}");
                r.time = NOV1 + i * 1000;
                r.profiles = vec!["profile".into()];
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
        ];
        Report {
            schema_version: 1,
            generated_at: "2026-12-01T00:00:00.000Z".into(),
            scan: Scan {
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
                evidence: "estimated",
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
        let lines: Vec<&str> = text.lines().collect();
        let section = |title: &str| lines.iter().position(|l| *l == title).expect(title);
        let (change, keep, unknown, fixed) = (
            section("Change"),
            section("Keep"),
            section("Not enough evidence"),
            section("No setting turns these off"),
        );
        assert!(change < keep && keep < unknown && unknown < fixed);
        let has = |line: &str| lines.contains(&line);
        assert!(has("4 setting changes recommended."));
        assert!(has(
            "  Main-agent prompt cache TTL: 5m -> 1h. Saves $25, using the 99.5% of requests with measured usage. Set in /synthetic/home/.claude/settings.json:promptCacheTtl."
        ));
        assert!(has("  Artifact tool: on -> off. Costs $90; never used."));
        assert!(has(
            "  Skill example: on -> only when you type /example. Costs $12; the model used it 2 times ($6 each)."
        ));
        assert!(has(
            "  Agent tool: on. Costs $40; used 310 times ($0.13 each)."
        ));
        assert!(has("  Fast mode: off."));
        assert!(has("  Subagent prompt cache TTL: 1h. Saves $4.20 over 5m."));
        assert!(has(
            "  Workflow tool: on. Cost unknown: none of its requests could be priced."
        ));
        assert!(has(
            "  Idle compaction: on. On and off cost the same on recorded idle gaps, so neither is cheaper."
        ));
        assert!(text.contains(
            "Too few recorded compactions to model a different window. Its requests carried a median of 150,000 tokens, and 1 in 10 carried more than 180,000."
        ));
        assert!(has(
            "  /repo/CLAUDE.md: cost unknown. Shorten it, or move parts into path-scoped rules or skills."
        ));
        // A zero-cost fixed piece is hidden; the system prompt and Bash are summed.
        assert!(has(
            "  System prompt, core tools such as Bash and Read, and the skill and subagent lists: $213."
        ));
        // Pieces no longer sent and those without a verdict get no line of their own.
        assert!(!text.contains("Retired") && !text.contains("Skill list framing"));
        let snippet = &text[text.find("To turn these off").unwrap()..];
        let json = &snippet[snippet.find('{').unwrap()..snippet.find("\n  }\n").unwrap() + 4];
        assert_eq!(
            serde_json::from_str::<Value>(json).unwrap(),
            serde_json::json!({
                "enableArtifact": false,
                "skillOverrides": {"example": "user-invocable-only"},
                "permissions": {"deny": ["mcp__docs"]}
            })
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
    fn color_only_adds_styles() {
        let report = synthetic();
        for details in [false, true] {
            let plain = text(&report, Some(120), details);
            let styled = format_report(
                &report,
                ReportOptions {
                    width: Some(120),
                    color: true,
                    details,
                    ..Default::default()
                },
            );
            assert!(styled.contains("\x1b[1m") && styled.contains("\x1b[36m"));
            assert_eq!(strip_ansi(&styled), plain);
        }
        assert_eq!(
            text(&report, None, true)
                .matches("Identical fixed costs remain unknown")
                .count(),
            2
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

    fn json(report: &Report) -> String {
        serde_json::to_string(report).unwrap()
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
        let before = json(&report);
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
        assert_eq!(
            json(&report),
            before,
            "rendering must not alter JSON or analysis"
        );
    }

    #[test]
    fn details_retain_every_assumption_unknown_cost_witness_and_finding_without_a_top_n_cutoff() {
        let mut report = synthetic_report();
        let result = ttl_mut(&mut report);
        result.cost.one_hour = Cost {
            known_usd: 9.87,
            total_usd: None,
            unknown: vec!["synthetic unknown fixed charge".into()],
            assumptions: vec!["synthetic price assumption".into()],
        };
        result.fixed_cost = Cost {
            known_usd: 0.0,
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
            reason: "no supported input estimate".into(),
            assumptions: vec!["synthetic attempt assumption".into()],
        });
        result.cache_evidence.push(CacheReplayEvidence {
            request: "synthetic-cache-request".into(),
            part: 0,
            ttl: Ttl::OneHour,
            state: KnownState::Unknown,
            outcome: ReplayOutcome::Unknown,
            reason: "synthetic unresolved predecessor".into(),
            predecessors: vec![CachePredecessor {
                request: "synthetic-cache-predecessor".into(),
                part: 0,
                time: 0,
                state: KnownState::Unknown,
                prefix_evidence: PrefixEvidence::Unknown,
                cohort_known: false,
                reason: "synthetic witness reason".into(),
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
        assert!(text.contains(&result.cache_evidence[0].request));
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
    fn terminal_controls_in_all_metadata_and_prose_are_escaped_while_json_retains_exact_metadata() {
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
        let parsed: Value = serde_json::from_str(&json(&report)).unwrap();
        assert_eq!(parsed["checks"][0]["owner"], hostile);
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
        let text = text(&report, Some(200), false);
        let keep = text.find("Keep").expect("a Keep section");
        assert!(text[keep..].contains("Fast mode: off"), "{text}");
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
        assert!(text(&report, Some(200), false).contains("Saves $720."));

        let name = "google-drive:google-drive-comments";
        report.checks.clear();
        let skill = ContextCost {
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
            tokens: Some(100),
            cost_usd: 5.0,
            estimated_usd: Some(5.0),
            calls: Some(0),
            last_used: None,
            current: true,
            cost_per_use_usd: Some(5.0),
            verdict: Some(Verdict::Off),
        };
        report.context = vec![skill.clone()];
        let narrow = text(&report, Some(40), false);
        let lines: Vec<&str> = narrow.lines().collect();
        let start = lines.iter().position(|l| l.trim() == "{").unwrap();
        let end = start + lines[start..].iter().position(|l| *l == "  }").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&lines[start..=end].join("\n")).unwrap(),
            serde_json::json!({"skillOverrides": {name: "user-invocable-only"}})
        );
        // The always-sent total uses the same exact scale as every other line.
        report.scan.since = Some(instant(&report.generated_at) - 3_600_000);
        report.context.push(ContextCost {
            kind: ContextKind::System,
            name: "Claude Code system prompt".into(),
            control: None,
            cost_usd: 1.0,
            estimated_usd: Some(1.0),
            calls: None,
            cost_per_use_usd: None,
            verdict: None,
            ..skill
        });
        assert!(text(&report, Some(200), false).contains("skill and subagent lists: $720."));
        let details = text(&report, Some(200), true);
        assert!(details.contains("1 recommended change"));
        assert!(!details.contains("No applicable"));
    }
}
