//! The terminal report: one verdict per setting by default, summarized evidence in details.

use std::fmt::Write as _;

use crate::collections::{HashMap, HashSet, IndexMap, IndexSet};

use crate::context::{
    CLAUDE_MEMORY, CODEX_MEMORY, Hidden, SYNCED_SKILLS, control_text, hidden_use, setting_rows,
    typed_only, unreadable_lines,
};
use crate::controls::CAPPED_SHARED;
use crate::discovery::{TTL_API_DEFAULT, TTL_BILLING_UNKNOWN, TTL_PLAN_DEFAULT};
use crate::doctor::How;
use crate::keepalive::duration as keepalive_duration;
use crate::model::{
    Action, AdvisorControl, AdvisorLine, AdvisorUse, BackgroundCheck, BackgroundFeature, BashCap,
    Billing, CacheReplayEvidence, CheckBase, CheckResult, CompactionCheck, ContextCost,
    ContextKind, ControlValue, Cost, Coverage, EffortCheck, EffortRun, Estimate, EstimateEvidence,
    Evidence, FableUse, FableWeek, Harness, InboundControl, InboundUse, KeepaliveCheck, Margin,
    MemoryCheck, Millis, ModelSource, PeriodStatus, Profile, Report, RequestEvidence,
    RetentionControl, ReturnTiming, Scalar, Share, SubagentCost, T3IdleCheck, T3TextGeneration,
    Ttl, UsageEstimate, Verdict,
};
use crate::prices::{TIER_SENSITIVITY, TIER_SIGNS, add_costs};
use crate::safe::{display, iso, parse_instant, terminal_text};
use crate::terminal::{BOLD, DIM, GREEN, RED, WrapOptions, YELLOW, cells, wrap_spans};
use crate::vibes::Item;

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

/// Shortest round-trip digits, with `NaN`, `Infinity` and `-Infinity` for non-finite values.
fn js_number(value: f64) -> String {
    match serde_json::Number::from_f64(value) {
        Some(n) => crate::exposure::number(&n),
        None if value.is_nan() => "NaN".into(),
        None if value > 0.0 => "Infinity".into(),
        None => "-Infinity".into(),
    }
}

/// Round to the nearest integer, halves toward positive infinity.
fn js_round(value: f64) -> f64 {
    let floor = value.floor();
    if value - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

/// Two decimal places for a non-negative value, rounding exact binary halves up.
fn to_fixed2(value: f64) -> String {
    if !value.is_finite() || value >= 1e21 {
        return js_number(value);
    }
    // A value whose exact binary expansion ends in a half cent is an odd multiple
    // of 1/8. Rust rounds those to even; this picks the larger.
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

/// [`money`], then what the whole came to extrapolated from the priced requests when some
/// were not.
fn extrapolated(cost: &Cost, estimate: Option<Estimate>) -> String {
    match (cost.total_usd, estimate) {
        (None, Some(e)) => format!(
            "{}; extrapolated, {}",
            money(cost),
            approx_usd(e.usd, e.margin)
        ),
        _ => money(cost),
    }
}

fn instant(value: Option<Millis>) -> String {
    value.map_or_else(|| "unknown".into(), iso)
}

/// `value` as [`usd`] writes it in details; when extrapolated, after a `~` and before its
/// margin of error.
fn approx_usd(value: f64, margin: Option<Margin>) -> String {
    match margin {
        None => usd(value),
        Some(Margin::Usd(m)) => format!("~{} ± {}", usd(value), usd(m)),
        Some(Margin::Unmeasured) => format!("~{}, margin unknown", usd(value)),
    }
}

/// Whether `text` quotes a figure [`approx_usd`] marked as extrapolated.
pub(crate) fn sampled(text: &str) -> bool {
    text.match_indices('~').any(|(at, _)| {
        let rest = &text[at + 1..];
        let rest = rest.strip_prefix('-').unwrap_or(rest);
        rest.strip_prefix('<').unwrap_or(rest).starts_with('$')
    })
}

fn estimate(value: Option<f64>) -> String {
    value.map_or_else(|| "unknown".into(), usd)
}

/// [`estimate`] for a figure that may be extrapolated, with its `~` and margin.
fn approx_estimate(value: Option<Estimate>) -> String {
    value.map_or_else(|| "unknown".into(), |e| approx_usd(e.usd, e.margin))
}

/// Bytes in decimal units, with one decimal below ten, such as `8.1 GB` or `512 kB`.
pub(crate) fn size(bytes: u64) -> String {
    scaled(bytes, [" B", " kB", " MB", " GB", " TB"])
}

/// A count with a decimal suffix, with one decimal below ten, such as `1.9B` or `345M`.
pub(crate) fn short(value: u64) -> String {
    scaled(value, ["", "k", "M", "B", "T"])
}

/// `value` scaled by thousands to the largest of `units` that keeps it under 1,000 once
/// rounded, with one decimal below ten.
fn scaled(value: u64, units: [&str; 5]) -> String {
    let mut scaled = value as f64;
    let mut unit = 0;
    while scaled >= 999.5 && unit < units.len() - 1 {
        scaled /= 1000.0;
        unit += 1;
    }
    match unit {
        0 => format!("{value}{}", units[0]),
        _ if scaled < 9.95 => format!("{scaled:.1}{}", units[unit]),
        _ => format!("{scaled:.0}{}", units[unit]),
    }
}

/// A whole number with commas between groups of three digits.
pub(crate) fn count(value: impl Into<i128>) -> String {
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
    let text = text.strip_suffix(".00").unwrap_or(&text);
    // Group thousands, so "$1,177" doesn't read as a larger number than it is.
    match text.split_once('$') {
        Some((sign, digits)) if digits.len() > 3 => match digits.parse::<i128>() {
            Ok(n) => format!("{sign}${}", count(n)),
            Err(_) => text.to_string(),
        },
        _ => text.to_string(),
    }
}

/// `usd` as [`about`] writes it, then `unit` such as "/mo"; when extrapolated over unpriced
/// requests, after a `~` and before its margin of error, such as "~$3,270/mo ± $410".
pub(crate) fn approx(usd: f64, margin: Option<Margin>, unit: &str) -> String {
    match margin {
        None => format!("{}{unit}", about(usd)),
        Some(Margin::Usd(m)) => format!("~{}{unit} ± {}", about(usd), about(m)),
        Some(Margin::Unmeasured) => format!("~{}{unit}, margin unknown", about(usd)),
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
pub(crate) fn percent(part: u64, whole: u64) -> String {
    js_number(((part as f64 / whole as f64) * 1000.0).floor() / 10.0)
}

/// `text` with its first letter capitalized.
pub(crate) fn capitalize(text: &str) -> String {
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
    /// A percentage of the context window, zero when off.
    Percent(i64),
    Idle(bool),
    T3Idle(crate::model::T3IdleMode),
    Speed(&'a str),
    Toggle(bool),
    Keepalive(&'a str),
    Effort(&'a str),
}

/// The fields every check shares, whatever its value type.
struct Common<'a> {
    check: &'static str,
    bucket: &'static str,
    profile: &'a str,
    profiles: &'a [String],
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
    margin: Option<Margin>,
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
        profiles: &b.profiles,
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
        margin: b.margin,
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
        CheckResult::TurnEnd(c) => base(
            "turn-end-compaction",
            "all",
            &c.base,
            &c.usage_estimates,
            |n| Setting::Percent(*n),
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
        CheckResult::Effort(c) => base("main-effort", "main", &c.base, &[], |s: &String| {
            Setting::Effort(s)
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

/// Anthropic's guidance on raising effort past `high`.
const PAST_HIGH: &str = "Anthropic's model guide says that past high, one more level can cost as much as a bigger model, so test the next model up at a lower effort";
/// Anthropic's model guide, which the effort and subagent warnings cite.
const CLAUDE_MODEL_GUIDE: &str =
    "https://claude.dev/blog/which-claude-model-should-you-build-with/";
/// Claude Code's page on choosing an effort level.
const CLAUDE_EFFORT_DOCS: &str =
    "https://code.claude.com/docs/en/model-config#choose-an-effort-level";
/// OpenAI's reasoning guide, which the Codex effort warnings cite.
const OPENAI_REASONING: &str = "https://developers.openai.com/api/docs/guides/reasoning";

/// The provider's own guidance on a default effort above `high`, which no transcript can
/// price; None at or below `high`.
fn past_high(level: &str, harness: Harness) -> Option<String> {
    Some(match (level, harness) {
        ("max", Harness::Claude) => format!(
            "{PAST_HIGH}, and Claude Code's docs call max prone to overthinking and say to test it before adopting it broadly."
        ),
        ("xhigh", Harness::Claude) => format!(
            "{PAST_HIGH}; on SWE-bench Pro, Opus 5.5 at xhigh scored about 1.4 points above high for 2.5 times the cost."
        ),
        ("max", Harness::Codex) => "OpenAI's reasoning guide keeps max for your most complex tasks, evaluated against xhigh.".into(),
        ("xhigh", Harness::Codex) => "OpenAI's reasoning guide says to use xhigh only when your evals show a clear benefit that justifies the extra latency and cost.".into(),
        _ => return None,
    })
}

/// A default as a card names it, such as `claude-opus-5-5 at high`.
fn default_pair(model: Option<&str>, level: Option<&str>) -> String {
    let model = model.unwrap_or("an unnamed model");
    match level {
        Some("auto") => format!("{model} with no level sent"),
        Some(level) => format!("{model} at {level}"),
        None => format!("{model}, which takes no effort level"),
    }
}

/// The main-session default as `vibes` shows it: the maintainer's pick beside the default
/// the settings give, the provider's guidance past `high`, and what each recorded model and
/// effort pair cost. None when the default is unresolved.
fn default_item(e: &EffortCheck, profile: &str, how: Option<How>, out: &Out<'_>) -> Option<Item> {
    let c = &e.control;
    // A Codex level with no model to weigh it on names no default either.
    if e.base.action != Action::Keep || c.model.is_none() {
        return None;
    }
    let ((model, effort), others) = crate::effort::pick(e.harness);
    let pick = default_pair(Some(model), Some(effort));
    let fine: Vec<String> = others
        .iter()
        .map(|(m, l)| default_pair(Some(m), Some(l)))
        .collect();
    let current = default_pair(c.model.as_deref(), c.effective.as_deref());
    let quiet = crate::effort::accepted(c, e.harness);
    let mut text = if current == pick {
        format!("Defaults to {current}, the maintainer's pick.")
    } else if quiet {
        format!("Defaults to {current}, which the maintainer accepts beside their pick, {pick}.")
    } else {
        let mut text = format!("Defaults to {current}. The maintainer's pick is {pick}");
        if !fine.is_empty() {
            let _ = write!(text, ", and {} is fine too", fine.join(" or "));
        }
        text + "."
    };
    // An accepted default is fine as it is, so no caution is raised against it.
    if let Some(guidance) = c
        .effective
        .as_deref()
        .filter(|_| !quiet)
        .and_then(|l| past_high(l, e.harness))
    {
        let _ = write!(text, " {guidance}");
    }
    let _ = write!(text, " {}", effort_runs(e));
    if !quiet {
        text += " A different model or effort changes the output tokens, so what the pick would cost is not measured.";
    }
    let (lead, guides): (&str, &[&str]) = match e.harness {
        Harness::Claude => (
            "The maintainer's pick of a default. Anthropic's guidance:",
            &[CLAUDE_MODEL_GUIDE, CLAUDE_EFFORT_DOCS],
        ),
        Harness::Codex => (
            "The maintainer's pick of a default. OpenAI's guidance:",
            &[OPENAI_REASONING],
        ),
    };
    Some(Item {
        name: tagged("Main-session default".into(), profile),
        value: if quiet {
            String::new()
        } else {
            format!("{current} {} {pick}", out.arrow)
        },
        what: "The model and reasoning effort a main session starts with.".into(),
        text,
        sources: std::iter::once(lead)
            .chain(guides.iter().copied())
            .map(String::from)
            .collect(),
        how,
        quiet,
        // doctor keeps this line, so its walk never writes the default the card reads.
        after: vec![],
    })
}

/// The first Claude Code that reads `CLAUDE_CODE_SUBAGENT_EFFORT`, as `(minor, patch)` of 2.x.
const SUBAGENT_EFFORT_SINCE: (u64, u64) = (1, 316);

/// The level the maintainer picks for a default subagent effort past `high`, which every
/// model that takes effort accepts.
const SUBAGENT_PICK: &str = "high";

/// Claude Code's changelog, which dates when a setting arrived.
const CLAUDE_CHANGELOG: &str = "https://github.com/anthropics/claude-code/blob/main/CHANGELOG.md";

/// One line of profiles whose default subagent effort the same setting decides.
struct SubagentLine<'a> {
    lead: &'a Profile,
    /// The profile tag the card's name carries.
    profile: String,
    /// The Claude Code each of the line's profiles last ran, by the name a card gives the
    /// profile, empty when the line covers one; for those with a recorded request.
    versions: Vec<(String, Option<&'a str>)>,
}

/// The default subagent effort as `vibes` shows it: the level and the model it pairs with,
/// past `high` the provider's guidance and the maintainer's pick of `high`, and an upgrade
/// when the latest Claude Code ignores the variable. None when no setting names one, as
/// subagents then follow the main session, which its own card covers.
fn subagent_item(line: &SubagentLine<'_>, out: &Out<'_>) -> Option<Item> {
    let p = line.lead;
    let effort = p.agents.effort.as_ref()?;
    let level = effort.value.as_str();
    // Unresolved settings, such as a secret model, may name a model other than the main one.
    let model = match (&p.agents.model, &p.agents.unresolved) {
        (Some(m), _) => m.value.as_str(),
        (None, None) => "the main session's model",
        (None, Some(_)) => "a model token-max cannot resolve",
    };
    let mut text = format!(
        "Subagents default to {level} on {model} when neither the call nor the agent's definition names an effort. Set in {}.",
        out.home(&effort.owner)
    );
    if let Some(why) = &p.agents.unresolved {
        let _ = write!(
            text,
            " {}, so a run may not have used these settings.",
            capitalize(why)
        );
    }
    let (minor, patch) = SUBAGENT_EFFORT_SINCE;
    // A profile's latest Claude Code before the variable arrived runs subagents at the
    // main session's effort, whatever it says.
    let ignored: Vec<(&str, &str)> = match p.harness {
        Harness::Claude => line
            .versions
            .iter()
            .filter_map(|(name, v)| Some((name.as_str(), (*v)?)))
            .filter(|(_, v)| !crate::version::claude_at_least(Some(v), minor, patch))
            .collect(),
        Harness::Codex => vec![],
    };
    if !ignored.is_empty() {
        let ran: Vec<String> = ignored
            .iter()
            .map(|(name, version)| match name {
                &"" => format!("The latest session ran Claude Code {version}"),
                name => format!("{name}'s latest session ran Claude Code {version}"),
            })
            .collect();
        let _ = write!(
            text,
            " {}, which ignores CLAUDE_CODE_SUBAGENT_EFFORT: it arrived in 2.{minor}.{patch}, so its subagents run at the main session's effort. Upgrade Claude Code to 2.{minor}.{patch} or later for the setting to apply.",
            ran.join("; ")
        );
    }
    let guidance = past_high(level, p.harness);
    if let Some(guidance) = &guidance {
        let _ = write!(
            text,
            " {guidance} The maintainer's pick for subagents is {SUBAGENT_PICK}. A different effort changes the output tokens, so what {SUBAGENT_PICK} would cost is not measured."
        );
    }
    // The edit stays on offer beside an upgrade, which token-max does not make itself.
    let how = guidance
        .is_some()
        .then(|| crate::doctor::subagent_effort_how(&effort.owner, p, SUBAGENT_PICK));
    let mut sources: Vec<String> = match p.harness {
        Harness::Claude => vec![
            "The maintainer's pick of a subagent default. Anthropic's guidance:".into(),
            CLAUDE_MODEL_GUIDE.into(),
            CLAUDE_EFFORT_DOCS.into(),
        ],
        Harness::Codex => vec![
            "The maintainer's pick of a subagent default. OpenAI's guidance:".into(),
            OPENAI_REASONING.into(),
        ],
    };
    if !ignored.is_empty() {
        sources.push(CLAUDE_CHANGELOG.into());
    }
    let quiet = how.is_none() && ignored.is_empty();
    Some(Item {
        name: tagged("Subagent default".into(), &line.profile),
        value: if guidance.is_some() {
            format!("{level} {} {SUBAGENT_PICK}", out.arrow)
        } else {
            String::new()
        },
        what: "The reasoning effort a subagent starts with when its call and definition name none."
            .into(),
        text,
        sources,
        how,
        quiet,
        after: vec![],
    })
}

/// What each recorded main-session model and effort pair cost, the most-run first.
fn effort_runs(e: &EffortCheck) -> String {
    if e.runs.is_empty() {
        return "No main-session requests in the window.".into();
    }
    let mut runs: Vec<String> = e
        .runs
        .iter()
        .take(EFFORT_PAIRS)
        .map(|run| {
            let cost = match run.estimate {
                Some(e) => approx(e.usd, e.margin, ""),
                None => "unpriced".into(),
            };
            format!(
                "{} {}, {cost}",
                effort_pair(run),
                plural(run.requests, "time")
            )
        })
        .collect();
    if e.runs.len() > EFFORT_PAIRS {
        let more = e.runs.len() - EFFORT_PAIRS;
        runs.push(format!(
            "{more} more pair{}",
            if more == 1 { "" } else { "s" }
        ));
    }
    format!("Ran {}.", runs.join("; "))
}

/// A model and effort pair as a line names it, such as `claude-opus-5-5 at high`.
fn effort_pair(run: &EffortRun) -> String {
    format!(
        "{} at {}",
        run.model.as_deref().unwrap_or("an unrecorded model"),
        run.effort.as_deref().unwrap_or("an unrecorded effort")
    )
}

/// Why an effort line has no verdict.
const EFFORT_UNPRICED: &str = "A different effort changes the output tokens, so its cost is not repriced and there is no verdict.";

/// The pairs shown on a summary line before the rest are counted.
const EFFORT_PAIRS: usize = 3;

/// An effort line's evidence: its guidance, the default and the level it runs at, what each
/// model and effort pair cost, and the setting that changes it.
fn effort_evidence(e: &EffortCheck, mark: Mark, out: &Out<'_>) -> String {
    let c = &e.control;
    let mut parts = vec![];
    if mark == Mark::Unknown {
        parts.push(not_enough(&e.control.unresolved.clone().unwrap_or_default()) + ".");
    }
    if let (Some(model), Some(level)) = (&c.model, &c.level) {
        let mut text = if level == "auto" {
            format!("No level is sent to {model}")
        } else if c.owner.ends_with(crate::effort::MODEL_DEFAULT) {
            format!("{model} defaults to {level}")
        } else {
            format!("The default is set to {level} on {model}")
        };
        if crate::effort::recorded(&c.model_owner) {
            text += ", the model the latest session ran";
        }
        if let Some(why) = &c.lowered {
            let effective = c.effective.as_deref().unwrap_or("unknown");
            let _ = write!(text, ", and runs at {effective}: {}", out.home_in(why));
        }
        parts.push(text + ".");
    }
    parts.push(effort_runs(e));
    parts.push(EFFORT_UNPRICED.into());
    if e.enterprise && c.level.is_some() {
        parts.push(
            "A Claude Enterprise per-role cap, which no local file records, could lower it.".into(),
        );
    }
    parts.push(format!("Set in {}.", out.home(&e.base.owner)));
    parts.join(" ")
}

/// What the report's dollars mean, for a reader on a Claude or ChatGPT plan or paying for API use.
const PRICES_MEAN: &str = "Dollars are public API prices, not an invoice: the closest public measure of what a request uses of a Claude or ChatGPT plan's limits, and what Claude's usage credits bill past them.";

/// What a check's line means on a Claude or ChatGPT plan, where the provider documents a
/// rule that API prices alone don't show, as a sentence and the page that documents it.
/// A plan is stated conditionally: token-max never reads a profile's plan.
fn plan_note(
    result: &CheckResult,
    r: &Common<'_>,
    out: &Out<'_>,
) -> Option<(&'static str, &'static str)> {
    let profile = out.profiles.get(r.profile);
    let harness = profile.map(|p| p.harness);
    match result {
        CheckResult::Ttl(_) if unset_main_ttl(result, r, out) => Some((
            "while it is unset, Claude Code gives the main conversation 1h on a Claude plan within its included usage, and 5m once usage credits or an API key pay for it.",
            TTL_SOURCE,
        )),
        CheckResult::Ttl(t) if t.bucket == crate::model::Bucket::Main => match profile?.billing? {
            Billing::Plan => Some((
                "while it is unset, Claude Code gives the main conversation 1h on a Claude plan, which you answered pays for this profile; requests past the plan's included usage are paid with usage credits and get 5m.",
                TTL_SOURCE,
            )),
            Billing::Api => Some((
                "while it is unset, Claude Code gives the main conversation 5m on an API key or usage credits, which you answered pay for this profile.",
                TTL_SOURCE,
            )),
        },
        CheckResult::Speed(_) if harness == Some(Harness::Claude) => Some((
            "on a Claude plan, fast mode is paid only from usage credits, at its API rates, even with plan usage left.",
            "https://code.claude.com/docs/en/fast-mode#requirements",
        )),
        CheckResult::Speed(_) if harness == Some(Harness::Codex) => Some((
            "on a ChatGPT plan, Fast uses included usage 2.5 times as fast as Standard and Ultrafast 8 times, which can exceed the API premium priced here.",
            "https://learn.chatgpt.com/docs/pricing",
        )),
        CheckResult::Background(b)
            if b.feature == BackgroundFeature::PromptSuggestions
                && r.current == Some(Setting::Toggle(true)) =>
        {
            Some((
                "Claude Code skips suggestions while a plan is near its usage limit, which this estimate cannot see.",
                "https://code.claude.com/docs/en/costs#background-token-usage",
            ))
        }
        CheckResult::Compaction(_) | CheckResult::Keepalive(_) | CheckResult::TurnEnd(_)
            if harness == Some(Harness::Codex) =>
        {
            Some((
                "ChatGPT credits charge cache writes as ordinary input, below the API write rate priced here on GPT-5.6 and later.",
                "https://learn.chatgpt.com/docs/pricing",
            ))
        }
        _ => None,
    }
}

/// Where Claude Code documents which TTL each request gets.
const TTL_SOURCE: &str =
    "https://code.claude.com/docs/en/prompt-caching#which-ttl-each-request-gets";

/// How the profile pays, when a check is the main TTL left unset at the default that billing,
/// as answered in doctor, picks. A pinned TTL's line still notes the answered default.
fn answered_main_ttl(result: &CheckResult, r: &Common<'_>, out: &Out<'_>) -> Option<Billing> {
    if !matches!(result, CheckResult::Ttl(t) if t.bucket == crate::model::Bucket::Main) {
        return None;
    }
    let profile = out.profiles.get(r.profile)?;
    let reason = profile.ttl.main.reason.as_str();
    // A T3 instance whose ownership is unresolved keeps the reason but loses the value.
    (profile.ttl.main.value.is_some() && (reason == TTL_PLAN_DEFAULT || reason == TTL_API_DEFAULT))
        .then_some(profile.billing)
        .flatten()
}

/// Whether a check is the main TTL left unset, which Claude Code picks by how the request is paid for.
fn unset_main_ttl(result: &CheckResult, r: &Common<'_>, out: &Out<'_>) -> bool {
    matches!(result, CheckResult::Ttl(t) if t.bucket == crate::model::Bucket::Main)
        && out
            .profiles
            .get(r.profile)
            .is_some_and(|p| p.ttl.main.value.is_none() && p.ttl.main.reason == TTL_BILLING_UNKNOWN)
}

/// Why auto memory's state is unknown, as a sentence without its period.
fn memory_unresolved(c: &MemoryCheck) -> String {
    format!(
        "Unknown whether it is on: {}",
        c.unresolved.as_deref().unwrap_or_default()
    )
}

/// Why auto memory's line reads as it does, judged as a skill is by its cost per use.
fn memory_reason(m: &MemoryCheck, r: &Common<'_>) -> String {
    let on = r.current == Some(Setting::Toggle(true));
    match r.action {
        Action::Keep if !on && memory_free(m) && !m.lower_bound() => "Off; it costs nothing.".into(),
        Action::Keep if !on => {
            "Off now; its cost and uses earlier in the window are shown for reference.".into()
        }
        Action::Keep if r.savings_usd.is_some_and(|usd| usd <= 0.0) => {
            "On, and it costs nothing.".into()
        }
        Action::Keep => {
            "Its cost per read or write is within --max-cost-per-use.".into()
        }
        Action::Change => "On, and never read back or written in the window.".into(),
        Action::Advisory if r.recommended.is_some() && m.reads + m.writes == 0 => {
            "On, and no read back or write was recorded, but a transcript line that could not be read may hold one.".into()
        }
        Action::Advisory if r.recommended.is_some() => {
            "It cost more than --max-cost-per-use per read or write.".into()
        }
        Action::Advisory if m.unresolved.is_some() => format!("{}.", memory_unresolved(m)),
        Action::Advisory if r.savings_usd.is_none() => {
            "What it costs could not be measured.".into()
        }
        Action::Advisory if m.tier_unknown => {
            "Its price depends on a service tier some requests did not record, so its cost per use is not settled.".into()
        }
        // A total that leaves out what could not be priced shows neither nothing nor a
        // cost per use within the limit.
        Action::Advisory => "Its total leaves out what could not be priced, so it may cost more per use than measured.".into(),
    }
}

/// Whether auto memory cost nothing and was never used in the window.
fn memory_free(c: &MemoryCheck) -> bool {
    c.base.savings_usd == Some(0.0) && c.reads + c.writes == 0
}

/// Auto memory's reads and writes, and what writes cost when they could not be priced.
fn memory_uses(c: &MemoryCheck) -> String {
    let uses = c.reads + c.writes;
    // An unreadable line that could hold a use leaves nonuse unproven.
    if uses == 0 && c.use_unproven {
        return "no recorded reads or writes".into();
    }
    if uses == 0 {
        return "never read back or written".into();
    }
    format!(
        "used {} ({}, {})",
        plural(uses, "time"),
        plural(c.reads, "read"),
        plural(c.writes, "write"),
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
pub(crate) fn duration(ms: Millis) -> String {
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
        Some(Setting::Percent(0)) => "off".into(),
        Some(Setting::Percent(n)) => format!("{n}%"),
        Some(Setting::Idle(b)) => b.to_string(),
        Some(Setting::T3Idle(mode)) => mode.as_str().into(),
        Some(Setting::Speed(s)) => s.into(),
        Some(Setting::Toggle(b)) => if b { "on" } else { "off" }.into(),
        Some(Setting::Keepalive(s) | Setting::Effort(s)) => s.into(),
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
        Some(Setting::Percent(0)) => "off".into(),
        Some(Setting::Percent(n)) => format!("{n}%"),
        Some(Setting::Ttl(t)) => t.as_str().into(),
        Some(Setting::Keepalive(s) | Setting::Effort(s)) => s.into(),
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
/// Cells every line but the title is indented by.
pub(crate) const MARGIN: usize = 2;
/// Section rules stop here on a wide terminal.
const RULE_CELLS: usize = 100;

/// A verdict's ANSI color, which only repeats what the line's words say: red for
/// a recommended change, yellow for an advisory, green for a value already right.
fn tier(action: Action) -> &'static str {
    match action {
        Action::Change => RED,
        Action::Advisory => YELLOW,
        Action::Keep => GREEN,
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
    /// What every request this host recorded in the window cost at API prices.
    spend: Spend,
}

/// This host's spend over the report's window, the base of every percent.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Spend {
    /// The total, extrapolated over unpriced requests, plus `floor`.
    pub(crate) usd: f64,
    /// How far the extrapolated part may be off; None when nothing was extrapolated.
    pub(crate) margin: Option<Margin>,
    /// What the profiles that priced none of their requests are known to cost, a lower bound
    /// with nothing to extrapolate from.
    pub(crate) floor: f64,
    /// Whether some profile priced none of its requests, even one with no known charge.
    pub(crate) partial: bool,
    since: Option<Millis>,
    until: Millis,
}

impl Spend {
    pub(crate) fn new(report: &Report) -> Self {
        let crate::totals::Spent {
            usd,
            margin,
            floor,
            partial,
        } = crate::totals::Spent::of(&report.historical);
        Spend {
            usd,
            margin,
            floor,
            partial,
            since: report
                .scan
                .since
                .or(report.scan.requests.first().map(|r| r.time)),
            until: generated_at(report),
        }
    }

    /// The total as the title writes it, such as "~$9,392 ± $4.72", "at least $120", or
    /// "~$9,392 ± $4.72 plus at least $120" when only some profiles priced nothing.
    fn text(&self) -> String {
        spend_text(self.usd, self.margin, self.floor, self.partial)
    }

    /// What this host spends every 30 days at the window's rate.
    pub(crate) fn monthly(&self) -> f64 {
        monthly(self.usd, self.since, Some(self.until))
    }

    /// The share of this host's spend that `usd`, spent within the window, makes.
    ///
    /// A line's own window lies inside the host's, since a profile's history starts no
    /// earlier than the host's first request, so its total divides the host's directly.
    fn share(&self, usd: f64) -> Option<f64> {
        (self.usd > 0.0).then(|| usd / self.usd)
    }

    /// The share of this host's spend that `usd` every 30 days makes, at the window's rate.
    fn of_month(&self, usd: f64) -> Option<f64> {
        let host = self.monthly();
        (host > 0.0).then(|| usd / host)
    }
}

/// A spend of `usd` as the title writes it, `floor` of it from profiles that priced none of
/// their requests, `partial` when there is one.
pub(crate) fn spend_text(usd: f64, margin: Option<Margin>, floor: f64, partial: bool) -> String {
    let measured = usd - floor;
    if !partial {
        approx(usd, margin, "")
    } else if measured == 0.0 && margin.is_none() {
        format!("at least {}", about(floor))
    } else if floor > 0.0 {
        format!(
            "{} plus at least {}",
            approx(measured, margin, ""),
            about(floor)
        )
    } else {
        format!(
            "{} plus requests with no known price",
            approx(measured, margin, "")
        )
    }
}

impl<'a> Out<'a> {
    fn new(report: &'a Report, options: ReportOptions<'a>) -> Self {
        let mut profiles = IndexMap::default();
        for p in &report.scan.profiles {
            profiles.insert(p.id.as_str(), p);
        }
        Out {
            spend: Spend::new(report),
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
    fn wrap(&mut self, text: &str, indent: usize, continuation: Option<usize>, style: &str) {
        self.spans(&[(text, style)], indent, continuation);
    }
    /// A line of spans, each in its own style, wrapped inside the margin with rows hanging
    /// at `continuation` cells when given.
    fn spans(&mut self, spans: &[(&str, &str)], indent: usize, continuation: Option<usize>) {
        // Escape metadata before wrapping or adding our own ANSI. Long paths are
        // hard-wrapped, never shortened; wide characters use terminal cell widths.
        let escaped: Vec<(String, &str)> = spans
            .iter()
            .map(|(text, style)| (terminal_text(text), *style))
            .collect();
        let escaped: Vec<(&str, &str)> = escaped.iter().map(|(t, s)| (t.as_str(), *s)).collect();
        let wrapped = wrap_spans(
            &escaped,
            WrapOptions {
                width: self.options.width,
                indent: indent + MARGIN,
                continuation: continuation.map(|c| c + MARGIN),
            },
            self.options.color,
        );
        self.text.push_str(&wrapped);
        self.text.push('\n');
    }
    fn blank(&mut self) {
        self.text.push('\n');
    }
    /// What each dot's color means; only printed when color is on.
    fn legend(&mut self) {
        let (dot, hollow) = self.dots();
        // No-break spaces keep each dot with its words, so a narrow terminal wraps between them.
        let parts = [
            (Mark::Change, dot, "change  "),
            (Mark::Consider, dot, "consider  "),
            (Mark::Keep, dot, "keep  "),
            (Mark::Unknown, hollow, "no\u{a0}verdict"),
        ];
        let spans: Vec<(String, &str)> = parts
            .iter()
            .flat_map(|(mark, marker, word)| {
                [
                    (format!("{marker}\u{a0}"), mark.style()),
                    ((*word).into(), DIM),
                ]
            })
            .collect();
        let spans: Vec<(&str, &str)> = spans.iter().map(|(t, s)| (t.as_str(), *s)).collect();
        self.spans(&spans, 0, None);
    }
    /// The markers of a line with a verdict and one without.
    fn dots(&self) -> (&'static str, &'static str) {
        if self.options.unicode {
            ("●", "○")
        } else {
            ("*", "o")
        }
    }
    /// A section heading, such as "CHANGES  biggest saving first": its name bold, the rest
    /// dim, and on a color terminal a rule out to the width.
    fn section(&mut self, title: &str) {
        self.blank();
        let (name, rest) = title.split_once("  ").unwrap_or((title, ""));
        let rest = if rest.is_empty() {
            String::new()
        } else {
            format!("  {rest}")
        };
        let mut spans = vec![(name.to_string(), BOLD), (rest, DIM)];
        // The rule fills the heading's row and is left out where three cells don't fit.
        let used = MARGIN + cells(title) + 1;
        let room = self.options.width.unwrap_or(RULE_CELLS).min(RULE_CELLS);
        if self.options.color && room >= used + 3 {
            let rule = if self.options.unicode { "─" } else { "-" };
            spans.push((format!(" {}", rule.repeat(room - used)), DIM));
        }
        let spans: Vec<(&str, &str)> = spans.iter().map(|(t, s)| (t.as_str(), *s)).collect();
        self.spans(&spans, 0, None);
    }
    /// A dim note about what was read, such as a T3 state's coverage.
    fn note(&mut self, name: &str, value: &str) {
        self.line(&format!("{name}: {value}"), 0, DIM);
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
        self.estimate(name, value, None);
    }
    /// [`Out::cost`] with what it came to extrapolated over unpriced requests.
    fn estimate(&mut self, name: &str, value: &Cost, estimate: Option<Estimate>) {
        self.field(name, &extrapolated(value, estimate));
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
    /// The report's one-line header: the tool's name in bold, then `read`, what was read,
    /// dimmed. Unlike every other line it starts at the left edge.
    fn title(&self, read: &str) -> String {
        let name = terminal_text(&match self.options.host {
            Some(host) => format!("token-max: {}", host.name),
            None => "token-max".into(),
        });
        let read = terminal_text(&format!("  {read}"));
        let mut text = wrap_spans(
            &[(&name, BOLD), (&read, DIM)],
            WrapOptions {
                width: self.options.width,
                indent: 0,
                // Rows after the first hang under what was read.
                continuation: Some(cells(&name) + 2),
            },
            self.options.color,
        );
        text.push('\n');
        text
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
    /// Every profile on a line, for evidence cards.
    fn profile_names(&self, ids: &[String]) -> String {
        ids.iter()
            .map(|id| self.profile_name(id))
            .collect::<Vec<_>>()
            .join(", ")
    }
    /// The settings files a piece of context's line covers, or its profiles when a file
    /// cannot be resolved for each of them.
    fn switch_files(&self, ids: &[String]) -> String {
        let files: Option<IndexSet<String>> = ids
            .iter()
            .map(|id| {
                let p = self.profiles.get(id.as_str())?;
                let owner = format!("{}:", p.settings_file);
                crate::rows::owner_file(p, &owner).map(|file| self.home(file))
            })
            .collect();
        match files.filter(|f| !f.is_empty()) {
            Some(files) => files.into_iter().collect::<Vec<_>>().join(", "),
            None => self.profile_names(ids),
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

/// The section heading a report of several hosts: what `hosts` hosts spent, what the changes
/// they offer would save, and what their settings save against the defaults, added as they
/// are.
pub fn totals_section(
    totals: &crate::totals::Totals,
    hosts: usize,
    options: ReportOptions<'_>,
) -> String {
    let spent = spend_text(totals.spend, totals.margin, totals.floor, totals.partial);
    let name = format!("token-max: total over {hosts} hosts");
    let read = format!(
        "  {} requests and {} tokens in {}, {spent} at API prices",
        count(totals.requests),
        count(totals.tokens),
        plural(totals.sessions, "session")
    );
    let mut text = wrap_spans(
        &[(&name, BOLD), (&read, DIM)],
        WrapOptions {
            width: options.width,
            indent: 0,
            continuation: Some(cells(&name) + 2),
        },
        options.color,
    );
    text.push('\n');
    let saving = totals.savings.text("Changes offered", "these hosts'");
    let credit = totals.credit("these hosts'").unwrap_or_default();
    let note = opening_note(&credit, totals.partial);
    for (line, style) in [(saving, PLAIN), (credit, PLAIN), (note, DIM)] {
        if line.is_empty() {
            continue;
        }
        text.push_str(&wrap_spans(
            &[(&line, style)],
            WrapOptions {
                width: options.width,
                indent: MARGIN,
                continuation: Some(MARGIN),
            },
            options.color,
        ));
        text.push('\n');
    }
    text
}

/// What a figure marked `~` assumes, said once.
pub(crate) const TILDE: &str = "A ~ marks a figure extrapolated from the requests that could be priced to those that could not, assuming they resemble each other, followed by its 95% margin of error.";

/// The note under the credit that opens the walk, saying what each host's footer says only
/// after it: what a `~` in `credit` assumes, and that its percents leave out the profiles that
/// priced none of their requests, when the spend is `partial`. Empty when neither applies.
pub(crate) fn opening_note(credit: &str, partial: bool) -> String {
    let mut notes = vec![];
    if credit.contains('~') {
        notes.push(TILDE);
    }
    if partial && credit.contains('%') {
        notes.push("Each percent is of a spend to which a profile that priced none of its requests adds only what could be priced, so a true share may be smaller.");
    }
    notes.join(" ")
}

/// Render every setting and feature on one line, the changes first, with each line's
/// evidence and the evidence cards under details. `fixes` are `doctor::fixes` of the same
/// report, so each change names the edit or step `doctor` offers for it.
pub fn format_report<'a>(
    report: &'a Report,
    fixes: &[crate::doctor::Fix],
    options: ReportOptions<'a>,
) -> Listing {
    let mut out = Out::new(report, options);
    let (title, head, rows, vibes, credit) = summary(report, fixes, &mut out);
    if options.details {
        details(report, &mut out);
    }
    Listing {
        title,
        head,
        rows,
        vibes,
        tail: out.text,
        options: ReportOptions {
            host: None,
            ..options
        },
        home: out.home,
        spend: out.spend,
        credit,
    }
}

/// The report as `token-max doctor` prints it, naming the edits it would offer.
#[cfg(test)]
pub(crate) fn rendered(report: &Report, options: ReportOptions<'_>) -> String {
    format_report(
        report,
        &crate::doctor::fixes(report, options.unicode),
        options,
    )
    .text()
}

/// The advice `token-max vibes` shows for a report.
#[cfg(test)]
pub(crate) fn advice(report: &Report, options: ReportOptions<'_>) -> Vec<Item> {
    format_report(
        report,
        &crate::doctor::fixes(report, options.unicode),
        options,
    )
    .vibes
}

/// A rendered report whose list of lines can be printed again once `doctor`'s walk has
/// sorted its changes into those accepted, declined and left by hand.
pub struct Listing {
    /// The one-line header naming what was read.
    title: String,
    /// Everything between the header and the list.
    head: String,
    rows: Vec<Row>,
    /// The advice the transcripts cannot price, which `vibes` shows and this report leaves out.
    pub vibes: Vec<Item>,
    /// Everything after the list.
    tail: String,
    options: ReportOptions<'static>,
    home: Option<String>,
    spend: Spend,
    /// What the settings its lines name save against the defaults; None under `--defaults`.
    credit: Option<crate::defaults::Credit>,
}

/// What became of a change in `doctor`'s walk, for the list printed after it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Choice {
    /// Accepted, to be written once the person confirms.
    Accepted,
    /// Answered no, or left when the walk ended early.
    Declined,
    /// A step to take by hand, as the line's setting column reads.
    Manual(String),
}

impl Listing {
    /// The report as printed without a terminal.
    pub fn text(&self) -> String {
        format!("{}{}", self.title, self.with_list(None))
    }

    /// The one-line header naming what was read, which the walk prints before its questions.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// What the settings its lines name save against the defaults, as its own line says;
    /// None under `--defaults`.
    pub fn credit(&self) -> Option<crate::defaults::Credit> {
        self.credit.clone()
    }

    /// The report without its header, with its changes grouped by `choices`: the accepted
    /// first, ending with `total`, then the declined, then the steps by hand, then every
    /// other line.
    pub fn review(&self, choices: &[(Origin, Choice)], total: &str) -> String {
        self.with_list(Some(Review { choices, total }))
    }

    fn with_list(&self, review: Option<Review<'_>>) -> String {
        let mut out = Out {
            text: String::new(),
            options: self.options,
            arrow: if self.options.unicode { "→" } else { "->" },
            home: self.home.clone(),
            profiles: IndexMap::default(),
            removed: crate::collections::HashMap::default(),
            spend: self.spend,
        };
        rows_section(&self.rows, review, &mut out);
        format!("{}{}{}", self.head, out.text, self.tail)
    }

    /// What a report line's setting or feature does, in one sentence.
    pub fn what(&self, origin: Origin) -> String {
        self.rows
            .iter()
            .find(|r| r.origin == Some(origin))
            .map(|r| r.what.clone())
            .unwrap_or_default()
    }

    /// The evidence behind a report line, as one paragraph: what was measured, then why.
    pub fn evidence(&self, origin: Origin) -> String {
        self.rows
            .iter()
            .find(|r| r.origin == Some(origin))
            .map(|r| {
                [r.stats.as_str(), r.evidence.as_str()]
                    .into_iter()
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    }
}

/// How the list groups changes after `doctor`'s walk.
#[derive(Clone, Copy)]
struct Review<'a> {
    choices: &'a [(Origin, Choice)],
    /// The accepted changes' summed saving, labeled, printed after them.
    total: &'a str,
}

fn generated_at(report: &Report) -> Millis {
    parse_instant(&report.generated_at).expect("reports record an ISO generation time")
}

/// The sentences naming what replaces a T3 text-generation selection, each with a leading space.
fn t3_overrides(g: &T3TextGeneration) -> String {
    let mut text = String::new();
    for (key, s) in &g.overrides {
        let _ = write!(text, " {key} replaces it with {} {}.", s.instance, s.model);
    }
    if g.project_overrides > 0 {
        let _ = write!(
            text,
            " {} replace these selections in projectSettingsOverrides.",
            plural(g.project_overrides, "project")
        );
    }
    text
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
        line.push_str(&t3_overrides(g));
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
        out.note(
            "T3 state",
            &format!("{}; {}", display(&state.path), state.metadata),
        );
        for exclusion in &state.exclusions {
            out.note("T3 coverage", exclusion);
        }
        for (cause, n) in &state.unresolved {
            out.note(
                "T3 unresolved",
                &format!("{}: {cause}", plural(*n, "request")),
            );
        }
        for (instance, n) in &state.removed {
            out.note(
                "T3 removed",
                &format!(
                    "{} of instance {instance}, no longer in T3's settings: billed, left out of every current profile's checks",
                    plural(*n, "request")
                ),
            );
        }
    }
    if out.options.details {
        for line in t3_text_generation(report, out) {
            out.line(&line, 0, DIM);
        }
    }
}

/// The evidence cards after the lines: every check, piece of context and request group.
fn details(report: &Report, out: &mut Out<'_>) {
    let arrow = out.arrow;
    out.section("EVIDENCE");
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

    // A resolved effort is a cost with no verdict, so it is not listed as one to keep.
    let verdict = |c: &CheckResult| match c {
        CheckResult::Effort(_) if c.action() == Action::Keep => None,
        _ => Some(c.action()),
    };
    let groups = [
        (Some(Action::Change), "Recommended changes"),
        (Some(Action::Advisory), "Advisory findings"),
        (Some(Action::Keep), "Keep current settings"),
        (None, "Costs with no verdict"),
    ];
    for (action, title) in groups {
        let results: Vec<&CheckResult> = report
            .checks
            .iter()
            .filter(|c| verdict(c) == action)
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
            out.estimate("Cost", &c.cost, c.estimate);
            out.field("Models", &shares(&c.models, c.requests));
            out.field("Efforts", &shares(&c.efforts, c.requests));
            for s in &c.sources {
                out.field(
                    "Model source",
                    &format!(
                        "{}: {} ({})",
                        label(&s.source),
                        out.home_in(&s.detail),
                        plural(s.runs, "run")
                    ),
                );
            }
            match (&c.cheapest, &c.reprice) {
                (Some(model), Some(cost)) => {
                    out.estimate(&format!("Same tokens on {model}"), cost, c.reprice_estimate);
                }
                _ => out.field("Cheapest model", &reprice_text(c)),
            }
            out.field("Model setting", &out.home_in(&c.model_setting));
            out.field("Effort setting", &out.home_in(&c.effort_setting));
        }
    }

    if !report.fable.is_empty() {
        out.section("Fable weekly use");
        for u in &report.fable {
            out.blank();
            let names: Vec<String> = u.profiles.iter().map(|p| out.profile_name(p)).collect();
            out.line(&names.join(", "), 0, BOLD);
            out.field("Requests", &plural(u.requests, "Fable request"));
            for w in &u.weeks {
                out.estimate(
                    &format!("{}, {}", fable_span(w), plural(w.requests, "request")),
                    &w.cost,
                    w.estimate,
                );
            }
            if u.requests > 0 {
                out.field("Coverage", FABLE_PARTIAL);
            }
            let caveats = fable_caveats(u);
            if !caveats.is_empty() {
                out.field("Attribution", caveats.trim_start());
            }
            out.field("Model setting", FABLE_MODEL);
            out.field("Prices", CLAUDE_PRICING);
        }
    }

    if report.advisor.iter().any(|u| u.calls > 0) {
        out.section("Advisor model");
        for u in report.advisor.iter().filter(|u| u.calls > 0) {
            out.blank();
            let names: Vec<String> = u.profiles.iter().map(|p| out.profile_name(p)).collect();
            let whose = match u.line {
                AdvisorLine::Shared { .. } => "; shared, profile attribution unknown",
                _ => "",
            };
            out.line(&format!("{}{whose}", names.join(", ")), 0, BOLD);
            out.field(
                "Calls",
                &format!(
                    "{} on {}",
                    plural(u.calls, "advisor call"),
                    plural(u.requests, "request")
                ),
            );
            if u.shared > 0 {
                out.field(
                    "Shared",
                    &format!(
                        "{} also on another line, counted once on a shared line",
                        plural(u.shared, "request")
                    ),
                );
            }
            out.estimate("Cost", &u.cost, u.estimate);
            out.field("Prices", CLAUDE_PRICING);
        }
    }

    if !report.inbound.is_empty() {
        out.section("Messages from other sessions");
        for u in &report.inbound {
            out.blank();
            let names: Vec<String> = u.profiles.iter().map(|p| out.profile_name(p)).collect();
            out.line(&names.join(", "), 0, BOLD);
            out.field("Value", &inbound_value(&u.control));
            out.field(
                "Set in",
                &format!("{}:crossSessionInbound", out.home(&u.control.file)),
            );
            out.field("Read from", &inbound_read(&u.control, out));
            if let Some(why) = &u.control.unresolved {
                out.field("Unresolved", why);
            }
            out.field(
                "Started turns",
                &format!(
                    "{}, {}",
                    plural(u.idle, "message"),
                    plural(u.requests, "request")
                ),
            );
            if u.idle > 0 {
                out.estimate("Those turns", &u.cost, u.estimate);
            }
            out.field(
                "Between tool calls",
                &format!("{}, not priced", plural(u.busy, "message")),
            );
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

/// Claude Code's subagent page, which gives the built-in Explore's tools but not its prompt.
const CLAUDE_SUBAGENT_DOCS: &str = "https://code.claude.com/docs/en/sub-agents#built-in-subagents";

/// Anthropic's tip for Claude's built-in Explore subagent when it ran off Haiku, beside what
/// it cost and what the same tokens would cost on the cheapest model; None otherwise. The
/// agent file it advises is a step by hand: it replaces the built-in's system prompt, which
/// Anthropic doesn't publish, so no file token-max writes would explore the same way.
fn explore_item(c: &SubagentCost, profile: &str, out: &Out<'_>) -> Option<Item> {
    if c.harness != Harness::Claude
        || c.kind != "Explore"
        || !c.built_in
        || c.runs == 0
        || c.models.iter().all(|m| m.name.contains("haiku"))
    {
        return None;
    }
    let r = &c.replacement;
    let list = |files: &[String]| {
        let files: Vec<String> = files.iter().map(|f| out.home(f)).collect();
        files.join(" and ")
    };
    let mut text = format!(
        "Ran on {} and cost {}. {} Anthropic's model guide notes that the built-in Explore runs on the session's model, and suggests a user agent named Explore with model: haiku to keep exploration cheap.",
        shares(&c.models, c.requests),
        figure(c.estimate),
        reprice_text(c)
    );
    if !r.present.is_empty() {
        let (verb, them) = match r.present.len() {
            1 => ("replaces", "it"),
            _ => ("replace", "them"),
        };
        text += &format!(
            " {} now {verb} the built-in, so later runs there follow {them}.",
            list(&r.present)
        );
    }
    if r.forced {
        // The step still covers the runs where no forced model applies.
        let (scope, whom) = if r.missing.is_empty() {
            ("", "")
        } else {
            (" for some runs", " to them")
        };
        text += &format!(
            " CLAUDE_CODE_SUBAGENT_MODEL_FORCE is set{scope}, which drops an agent file's model, so the tip does not apply{whom} while it is."
        );
    }
    // Nothing left to do once every run has its replacement; nothing a file can do under force.
    let quiet = r.missing.is_empty() && !r.forced;
    let how = (!r.missing.is_empty()).then(|| How::Manual {
        // The built-in's documented restriction and its start without CLAUDE.md.
        step: format!(
            "add a new .md agent file to {} with the frontmatter name: Explore; description: when to use it; model: haiku; disallowedTools: Write, Edit; omitClaudeMd: true; and a system prompt of your own below it",
            list(&r.missing)
        ),
        reason: "the file replaces the built-in Explore's system prompt, which Anthropic doesn't publish".into(),
    });
    Some(Item {
        value: if how.is_some() {
            format!("inherit {} haiku", out.arrow)
        } else {
            String::new()
        },
        what: "The model Claude Code's built-in Explore subagent, which searches and reads code for the main session, runs on.".into(),
        how,
        quiet,
        ..guidance(
            subagent_name(c),
            profile,
            text,
            &[CLAUDE_MODEL_GUIDE, CLAUDE_SUBAGENT_DOCS],
        )
    })
}

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

/// A cost as [`approx`] writes it; "an unknown amount" when nothing was priced.
fn figure(estimate: Option<Estimate>) -> String {
    estimate.map_or_else(
        || "an unknown amount".into(),
        |e| approx(e.usd, e.margin, ""),
    )
}

/// What the same tokens would cost on the provider's cheapest current model.
fn reprice_text(c: &SubagentCost) -> String {
    match (&c.cheapest, &c.reprice) {
        (None, _) => "The price catalog has no cheapest current model for its provider.".into(),
        (Some(model), None) => {
            format!("It already ran on {model}, its provider's cheapest current model.")
        }
        (Some(model), Some(_)) => format!(
            "On {model} the same recorded tokens would cost {}, assuming the same turns.",
            figure(c.reprice_estimate)
        ),
    }
}

/// One subagent type's evidence: its runs, models, efforts and what a cheaper model would cost.
fn subagent_line(c: &SubagentCost, out: &Out<'_>) -> String {
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
                format!("{detail} ({})", plural(s.runs, "run"))
            }
        })
        .collect();
    format!(
        "{}{coverage} on {} at {effort}. Model source: {}. {} Model setting: {}. Effort setting: {}.",
        capitalize(&plural(c.runs, "run")),
        shares(&c.models, c.requests),
        sources.join("; "),
        reprice_text(c),
        out.home_in(&c.model_setting),
        out.home_in(&c.effort_setting),
    )
}

/// Anthropic's plan pricing, which lists each plan's Fable allowance.
const CLAUDE_PRICING: &str = "https://claude.com/pricing";
/// Anthropic's help article on how Fable models count against a plan's limits.
const FABLE_PLANS: &str =
    "https://support.claude.com/en/articles/15424964-claude-fable-models-on-your-plan";

/// The settings that choose Fable, ending its line.
const FABLE_SETTING: &str = "Model setting: /model, or model in settings.json.";
/// Where the model is chosen, which decides whether Fable runs.
const FABLE_MODEL: &str = "/model, or model in settings.json";
/// How many of a background feature's directories its evidence names.
const BACKGROUND_DIRECTORIES: usize = 3;

/// Anthropic's Fable rule for each plan, stated conditionally since the plan is never read.
const FABLE_RULE: &str = "on a Claude Max or Ultra plan or a Team or seat-based Enterprise premium seat, Fable models can use up to half the weekly usage limit, and use it faster than other models; on Pro or a Team or seat-based Enterprise standard seat, they run on usage credits; on usage-based Enterprise, they bill at API rates.";

/// Why a profile's Fable figures fall short of what the plan's cap counts.
const FABLE_PARTIAL: &str = "These figures are partial: the cap counts Fable use on every Claude surface, they cover only this profile's recorded sessions, and they may include requests paid with usage credits after the cap was reached. The weekly reset time is not recorded, and the limit is not published in tokens or dollars, so no share of it is shown.";

/// A grouped count with the noun in the right number, such as "1,024 runs".
pub(crate) fn plural(n: impl Into<i128>, noun: &str) -> String {
    let n = n.into();
    format!("{} {noun}{}", count(n), if n == 1 { "" } else { "s" })
}

/// Caveats on whose a profile's Fable requests were and which model ran, each a sentence.
fn fable_caveats(u: &FableUse) -> String {
    let mut text = String::new();
    if u.shared > 0 {
        let _ = write!(
            text,
            " {} also attributed to another Claude profile that signs in to Anthropic and reads the same transcripts, so whose they were is unknown.",
            match u.shared {
                1 => "1 of them is".into(),
                n => format!("{} of them are", count(n)),
            }
        );
    }
    if u.unrecorded > 0 {
        let _ = write!(
            text,
            " {} recorded no model, for the request or an advisor call, and may have run on Fable.",
            plural(u.unrecorded, "request")
        );
    }
    text
}

/// One span's label, such as `to 2026-11-14` or `3 days to 2026-11-07`.
fn fable_span(w: &FableWeek) -> String {
    let days = js_round((w.end - w.start) as f64 / DAY).max(1.0);
    let span = if days >= 7.0 {
        String::new()
    } else {
        format!(
            "{} day{} ",
            js_number(days),
            if days == 1.0 { "" } else { "s" }
        )
    };
    format!("{span}to {}", date(&iso(w.end)))
}

/// A profile's weekly Fable spend, with no share of a limit and no verdict.
fn fable_row(u: &FableUse) -> Row {
    let mut row = Row::new(
        Mark::Info,
        "Fable weekly use".into(),
        "What Fable models cost each week, which a Claude plan may limit apart from other models."
            .into(),
    );
    row.sources = vec![CLAUDE_PRICING];
    let caveats = fable_caveats(u);
    if u.requests == 0 {
        row.evidence =
            format!("No Fable requests recorded in the window.{caveats} {FABLE_SETTING}");
        return row;
    }
    let weeks: Vec<String> = u
        .weeks
        .iter()
        .map(|w| {
            format!(
                "{} ({}, {})",
                figure(w.estimate),
                plural(w.requests, "request"),
                fable_span(w)
            )
        })
        .collect();
    row.amount = Some(("costs", figure(u.estimate)));
    row.evidence = format!(
        "{} at API prices, per 7 days back from this report: {}. {FABLE_PARTIAL}{caveats} {FABLE_SETTING}",
        plural(u.requests, "Fable request"),
        weeks.join(", ")
    );
    row
}

/// The pages that document `crossSessionInbound` and how a message reaches a session.
const INBOUND_SOURCES: [&str; 2] = [
    "https://code.claude.com/docs/en/settings-reference#crosssessioninbound",
    "https://code.claude.com/docs/en/cross-session-messaging#control-inbound-messages",
];

/// Why the line has no verdict.
const INBOUND_NO_VERDICT: &str = "Hold or refuse would remove the turns these messages start and the work done in them, which sessions that coordinate depend on, so the line gives no verdict.";

/// What Claude Code does with a message when no file sets `crossSessionInbound`.
const INBOUND_UNSET: &str = "With no value set, Claude Code delivers a message when both sessions bypass permission prompts or neither does, and holds it for approval otherwise.";

/// Which files the value was read from, and what may outrank them unrecorded.
fn inbound_read(c: &InboundControl, out: &Out<'_>) -> String {
    let flag = "a --settings flag would outrank these files and is not recorded";
    match &c.directory {
        Some(cwd) => format!(
            "managed, user, project and local settings at {}, the latest session's directory; {flag}",
            out.home(cwd)
        ),
        None => format!("managed and user settings; {flag}"),
    }
}

/// The current `crossSessionInbound` value, or what Claude Code does with none set.
fn inbound_value(c: &InboundControl) -> String {
    match (&c.unresolved, c.value) {
        (Some(_), _) => "unknown".into(),
        (None, Some(value)) => value.as_str().into(),
        (None, None) => "unset, by permission mode".into(),
    }
}

/// The turns messages from other sessions started, with no verdict: orchestrators depend on
/// these messages, so their cost says nothing about whether to stop them.
fn inbound_row(u: &InboundUse, out: &Out<'_>) -> Row {
    let mut row = Row::new(
        Mark::Info,
        "Messages from other sessions".into(),
        "Lets your other Claude Code sessions message this one; a message to an idle session starts a turn that resends the whole conversation.".into(),
    );
    row.value = inbound_value(&u.control);
    row.sources = INBOUND_SOURCES.to_vec();
    let c = &u.control;
    row.setting = format!("{}:crossSessionInbound", out.home(&c.file));
    let mut parts = vec![];
    if let Some(why) = &c.unresolved {
        parts.push(not_enough(why) + ".");
    }
    if u.idle + u.busy == 0 {
        parts.push("No message from another session arrived in the window.".into());
    } else if u.idle == 0 {
        parts.push("No message from another session started a turn in the window.".into());
    } else {
        parts.push(format!(
            "{} from other sessions started turns in an idle session, which with the subagents they started made {} costing {}.",
            plural(u.idle, "message"),
            plural(u.requests, "request"),
            figure(u.estimate)
        ));
    }
    if u.busy > 0 {
        parts.push(format!(
            "{} arrived between tool calls of a running turn, at a cost not measured.",
            plural(u.busy, "more message")
        ));
    }
    if u.idle + u.busy > 0 {
        parts.push(INBOUND_NO_VERDICT.into());
    }
    if c.value.is_none() && c.unresolved.is_none() {
        parts.push(INBOUND_UNSET.into());
    }
    parts.push(format!("Read from {}.", inbound_read(c, out)));
    parts.push(format!(
        "Set crossSessionInbound to accept, hold or refuse in {}.",
        out.home(&c.file)
    ));
    row.evidence = parts.join(" ");
    row
}

/// One check's full evidence card, headed by its verdict, or `INFO` when it has none.
fn card(result: &CheckResult, action: Option<Action>, out: &mut Out<'_>) {
    let arrow = out.arrow;
    let r = common(result);
    out.blank();
    out.line(
        &format!(
            "{}  {} / {}  [{}]",
            action
                .map_or_else(|| "info".into(), |a| label(&a))
                .to_uppercase(),
            r.check,
            r.bucket,
            label(&r.evidence)
        ),
        0,
        action.map_or(DIM, tier),
    );
    if r.profiles.len() > 1 {
        out.field("Profiles", &out.profile_names(r.profiles));
    } else {
        out.field("Profile", &out.profile_name(r.profile));
    }
    out.field("Owner", &display(r.owner));
    out.field("Current", &setting(r.current));
    out.field("Suggested", &setting(r.recommended));
    out.field("Reason", &reason(result, &r));
    if let Some((note, source)) = plan_note(result, &r, out) {
        out.field("Plan", &format!("{} {source}", capitalize(note)));
    }
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
            r.savings_usd
                .map_or_else(|| "unknown".into(), |v| approx_usd(v, r.margin)),
            r.monthly_savings_usd.map_or_else(
                || "unknown".into(),
                |v| approx_usd(v, r.margin.map(|m| m.scale(monthly(1.0, r.since, r.until))))
            )
        ),
    );
    match result {
        CheckResult::Effort(e) => {
            let c = &e.control;
            out.field(
                "Model",
                &format!(
                    "{} ({})",
                    or_unknown(c.model.as_deref()),
                    out.home_in(&c.model_owner)
                ),
            );
            out.field("Default", &or_unknown(c.level.as_deref()));
            out.field("Runs at", &or_unknown(c.effective.as_deref()));
            if let Some(why) = &c.lowered {
                out.field("Lowered", &out.home_in(why));
            }
            for run in &e.runs {
                out.field(
                    "Recorded",
                    &format!(
                        "{}, {} requests, {}",
                        effort_pair(run),
                        count(run.requests),
                        extrapolated(&run.cost, run.estimate)
                    ),
                );
            }
        }
        CheckResult::Memory(m) => {
            out.field("Turn off", &memory_off(m, out));
            out.field(
                "Loaded memory and reads",
                &m.context_usd
                    .map_or_else(|| "unknown".into(), |usd| approx_usd(usd, m.base.margin)),
            );
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
            out.estimate("Estimated request cost", &b.cost, b.cost_estimate);
            out.estimate(
                "Later input costs its reads avoided",
                &b.lost_warming,
                b.lost_estimate,
            );
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
            if let crate::model::PerTtl {
                five_minutes: Some(short),
                one_hour: Some(long),
            } = t.comparison_estimate
                && (short.margin.is_some() || long.margin.is_some())
            {
                out.field(
                    "Extrapolated cache comparison",
                    &format!(
                        "5m {}; 1h {}; 1h saves {}",
                        approx(short.usd, short.margin, ""),
                        approx(long.usd, long.margin, ""),
                        approx(t.one_hour_savings_usd, r.margin, "")
                    ),
                );
            }
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
                    out.estimate(scenario.mode.as_str(), &scenario.cost, scenario.estimate);
                    // An error no one session owns leaves nothing extrapolated to show.
                    if let Some(savings) = scenario
                        .savings
                        .filter(|_| scenario.coverage.priced != scenario.coverage.total)
                    {
                        out.field(
                            "Saving over the current mode",
                            &format!(
                                "{}, extrapolated from the {} of {} sessions priced in both",
                                approx_usd(savings.usd, savings.margin),
                                scenario.coverage.priced,
                                scenario.coverage.total
                            ),
                        );
                    }
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
                        "{} over window; {} / 30 days{}",
                        approx_estimate(scenario.delta),
                        approx_estimate(scenario.monthly_delta),
                        match scenario.coverage {
                            c if c.priced != c.total =>
                                format!("; {} of {} sessions priced both ways", c.priced, c.total),
                            _ => String::new(),
                        }
                    ),
                );
                let periods = &scenario.periods;
                let finals: u64 = periods
                    .groups
                    .iter()
                    .filter(|g| g.kind.return_timing == ReturnTiming::Final)
                    .map(|g| g.periods)
                    .sum();
                out.field(
                    "Idle periods",
                    &format!(
                        "{} request/final periods; {} eligible, {} confirmed; {} censored; {} final periods without observed return",
                        periods.total(),
                        periods.with_status(PeriodStatus::Eligible),
                        periods.confirmed,
                        periods.with_status(PeriodStatus::Censored),
                        finals
                    ),
                );
            }
            let s = &i.after_recommendations.sensitivity;
            out.field(
                "Idle sensitivity, on minus off",
                &format!(
                    "closed {}; open/awake {}; larger summary/full prefix {}; late trigger {}. The aggregate estimate decides.",
                    approx_estimate(s.closed_delta),
                    approx_estimate(s.open_delta),
                    approx_estimate(s.larger_summary_delta),
                    approx_estimate(s.late_trigger_delta)
                ),
            );
            out.field(
                "Extra re-read break-even per resumed summary",
                &estimate(s.reread_break_even_usd_per_resumed_summary),
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
                &k.idle_compaction_savings.map_or_else(
                    || "unknown or unsupported".into(),
                    |e| approx_usd(e.usd, e.margin),
                ),
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
                "Recorded cache categories held constant; Claude cache-switch effects excluded. API-price premium, which a plan can count differently (see Plan). Work held constant; latency value excluded.",
            );
        }
        // A set value with no selected request has no replay to show.
        CheckResult::TurnEnd(c) if c.scenarios.is_empty() => {}
        CheckResult::Compaction(c) if c.scenarios.is_empty() => {}
        CheckResult::TurnEnd(c) => {
            for u in &c.usable {
                out.field(
                    "Usable window",
                    &format!(
                        "{}: {} tokens",
                        u.model.as_deref().map_or_else(|| "unknown".into(), display),
                        u.tokens
                    ),
                );
            }
            out.field(
                "Auto-compact limit replayed with it",
                &match c.window {
                    Some(w) if c.window_offered => {
                        format!("{w} tokens, which the auto-compact window line recommends")
                    }
                    Some(w) => format!("{w} tokens"),
                    None => "none set".into(),
                },
            );
            out.field("Current-threshold baseline cost", &money(&c.baseline_cost));
            let suggested = c
                .scenarios
                .iter()
                .find(|s| Some(s.window) == c.base.recommended);
            out.field(
                "Suggested-threshold cost",
                &suggested.map_or_else(|| "unknown".into(), |s| money(&s.cost)),
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
            if matches!(r.check, "compaction-window" | "turn-end-compaction") {
                "Earlier compaction loses detail and can add re-reads or changed work. "
            } else {
                ""
            }
        ),
    );
    if action == Some(Action::Change) {
        out.field(
            "Next",
            &format!(
                "Review evidence with --details, then update {} yourself.",
                display(r.owner)
            ),
        );
    }
    if action == Some(Action::Advisory) {
        let weighed = matches!(result, CheckResult::Memory(_) | CheckResult::Speed(_))
            && r.recommended.is_some();
        out.field(
            "Next",
            if weighed {
                "Weigh what it cost against what it gave before changing the setting; advisory findings do not affect the exit code."
            } else {
                "Review evidence and resolve current ownership/coverage before changing a setting; advisory findings do not affect the exit code."
            },
        );
    }
    for id in r.history_profiles {
        out.field("History profile", &out.profile_name(id));
    }
    match result {
        CheckResult::Background(_) | CheckResult::Memory(_) | CheckResult::Effort(_) => {}
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
                for g in &comparison.periods.groups {
                    out.field(
                        "T3 idle period group",
                        &format!(
                            "{} periods; {}; {}; {}; {}",
                            g.periods,
                            label(&g.kind.status),
                            label(&g.kind.return_timing),
                            label(&g.kind.outcome),
                            g.kind.reason
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
                for g in &scenario.periods.groups {
                    out.field(
                        "Idle period group",
                        &format!(
                            "{} periods; {}; {}; {}; {}",
                            g.periods,
                            label(&g.kind.status),
                            label(&g.kind.return_timing),
                            label(&g.kind.outcome),
                            g.kind.reason
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
        CheckResult::Keepalive(k) => {
            // Counts are what the priced sessions recorded; dollars cover every period.
            if let Some(c) = k
                .base
                .coverage
                .filter(|c| c.priced > 0 && c.priced < c.total)
            {
                out.field(
                    "Coverage",
                    &format!(
                        "ping and return counts cover the {} of {} periods that could be priced; dollar figures are extrapolated to all {}",
                        count(c.priced),
                        count(c.total),
                        count(c.total)
                    ),
                );
            }
            for s in &k.scenarios {
                out.field(
                    &format!("Cap {}", keepalive_duration(s.cap_ms)),
                    &format!(
                        "{} return pings and {} final pings cost {}; {} rescued returns; recorded work saves {}; net {}; {} if final sessions closed at once",
                        s.pings,
                        s.final_pings,
                        approx_usd(s.ping_cost_usd, s.margins.ping_cost),
                        s.rescued,
                        approx_usd(s.avoided_usd, s.margins.avoided),
                        approx_usd(s.savings_usd, s.margins.savings),
                        approx_usd(s.closed_savings_usd, s.margins.closed_savings)
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
        CheckResult::TurnEnd(c) if c.scenarios.is_empty() => {}
        CheckResult::Compaction(c) if c.scenarios.is_empty() => {}
        CheckResult::TurnEnd(c) => {
            out.cost("Current-threshold baseline", &c.baseline_cost);
            for scenario in &c.scenarios {
                out.field(
                    "Candidate threshold",
                    &format!(
                        "{}; {} affected requests; {} added compactions",
                        setting(Some(Setting::Percent(scenario.window))),
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
                    &scenario
                        .savings_usd
                        .map_or_else(|| "unknown".into(), |v| approx_usd(v, scenario.margin)),
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
            }
        }
        CheckResult::Compaction(c) => {
            out.cost("Current-window baseline", &c.baseline_cost);
            out.cost("Existing summaries", &c.observed_summary_cost);
            for scenario in c.scenarios.iter().chain(&c.cap) {
                // `vibes`' cap is listed for its card's figure, never weighed for the verdict.
                let cap = if c.cap.as_ref().is_some_and(|k| k.window == scenario.window) {
                    "; vibes' cap, priced for its card and never recommended"
                } else {
                    ""
                };
                out.field(
                    "Candidate window",
                    &format!(
                        "{} tokens; {} affected requests; {} added compactions{cap}",
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
                    &scenario
                        .savings_usd
                        .map_or_else(|| "unknown".into(), |v| approx_usd(v, scenario.margin)),
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
        | CheckResult::TurnEnd(_)
        | CheckResult::Idle(_)
        | CheckResult::Speed(_)
        | CheckResult::Background(_)
        | CheckResult::Keepalive(_)
        | CheckResult::Memory(_)
        | CheckResult::Effort(_) => {}
    }
    out.notes("Assumption", r.assumptions);
    out.notes("Uncertainty", r.uncertainty);
    out.notes("Source", r.sources);
}

fn reason(result: &CheckResult, r: &Common<'_>) -> String {
    match result {
        CheckResult::Speed(s) => return s.reason.clone(),
        CheckResult::Effort(_) => return EFFORT_UNPRICED.into(),
        CheckResult::Memory(m) => return memory_reason(m, r),
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
        CheckResult::Keepalive(_) => {
            return if r.recommended.is_none() {
                "The cache lifetime, ping size or too many idle periods could not be priced."
            } else {
                "Lowest estimated aggregate net cost among no pings, each cap and recommended idle compaction; final periods ping for the full cap."
            }
            .into();
        }
        CheckResult::T3Idle(_) => return "Lowest estimated changed cost among off, conditional on-return acceptance and conditional proactive server liveness; sessions without lifecycle or summary evidence are extrapolated from the priced ones.".into(),
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
    if matches!(r.check, "compaction-window" | "turn-end-compaction") {
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
            // The evidence lists every model; the name keeps to one line.
            match models.as_slice() {
                [] => "Auto-compact window".into(),
                [model] => format!("Auto-compact window ({model})"),
                [model, rest @ ..] => {
                    format!("Auto-compact window ({model} and {} more)", rest.len())
                }
            }
        }
        CheckResult::T3Idle(_) => "T3 idle compaction".into(),
        CheckResult::TurnEnd(_) => "Turn-end compaction".into(),
        CheckResult::Idle(_) => "Idle compaction".into(),
        CheckResult::Speed(_) => "Fast mode".into(),
        CheckResult::Keepalive(_) => "Keepalive pings".into(),
        CheckResult::Memory(m) => match m.harness {
            Harness::Codex => CODEX_MEMORY.into(),
            Harness::Claude => CLAUDE_MEMORY.into(),
        },
        CheckResult::Effort(_) => "Main-session effort".into(),
        CheckResult::Background(b) => match b.feature {
            BackgroundFeature::PromptSuggestions => "Prompt suggestions".into(),
            BackgroundFeature::SessionRecaps => "Session recaps".into(),
            BackgroundFeature::GoalCheckIns => "Goal check-ins".into(),
            BackgroundFeature::AutoRecaps => "Codex automatic recaps".into(),
        },
    }
}

/// What tells a background feature's line apart from the same feature's line for other
/// directories, coarsest first: why it resolved apart, then how many directories, then which.
fn background_scopes(b: &BackgroundCheck, out: &Out<'_>) -> Vec<String> {
    let name = |d: &Option<crate::model::Text>| {
        d.as_ref()
            .filter(|d| !d.is_empty())
            .map_or_else(|| "directory absent".into(), |d| out.home(d))
    };
    let mut parts: Vec<String> = b.scope.iter().cloned().collect();
    match &b.directories[..] {
        [] => {}
        [one] => parts.push(name(one)),
        [first, rest @ ..] => parts.extend([
            format!("{} directories", rest.len() + 1),
            format!("such as {}", name(first)),
        ]),
    }
    (1..=parts.len()).map(|n| parts[..n].join(", ")).collect()
}

/// Why a background feature's cost is unknown.
fn background_unknown(b: &BackgroundCheck) -> &'static str {
    if b.base.current.is_none() {
        return "Cost unknown: current scope or launch selection is unresolved.";
    }
    if no_share(b.base.coverage) {
        return NO_SHARE_COST;
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
    about(monthly(usd, since, until))
}

/// `usd` over `since..until` scaled to 30 days at the same rate; as is when the window is unknown.
pub(crate) fn monthly(usd: f64, since: Option<Millis>, until: Option<Millis>) -> f64 {
    match (since, until) {
        (Some(from), Some(to)) if to > from => (usd * 30.0 * DAY) / (to - from) as f64,
        _ => usd,
    }
}

/// A share of this host's spend as a whole percent, or under 1% when it rounds to none.
pub(crate) fn share_text(share: f64) -> String {
    let percent = share * 100.0;
    if percent > 0.0 && percent < 0.5 {
        "<1%".into()
    } else {
        format!("{}%", js_number(js_round(percent)))
    }
}

/// Why keepalive pings stay off.
fn keepalive_keep(k: &KeepaliveCheck, r: &Common<'_>) -> String {
    let interval = k.interval_ms.map(keepalive_duration).unwrap_or_default();
    let figure = |usd: f64| {
        approx(
            monthly(usd, r.since, r.until),
            r.margin.map(|m| m.scale(monthly(1.0, r.since, r.until))),
            "/mo",
        )
    };
    match k.margin_usd {
        Some(m) => format!(
            "Pinging idle sessions every {interval} would cost {} more than {}.{}",
            figure(-m),
            if k.idle_compaction_savings.is_some_and(|s| s.usd > 0.0) {
                "idle compaction"
            } else {
                "leaving them idle"
            },
            assumed_tier(r)
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
    let margin = r.margin.map(|m| m.scale(monthly(1.0, r.since, r.until)));
    if let Some(Setting::Keepalive(policy)) = r.recommended {
        return format!(
            "Pinging idle sessions {policy} would save {}, but {}.{}",
            approx(
                r.monthly_savings_usd.unwrap_or_else(|| monthly(
                    r.savings_usd.unwrap_or(0.0),
                    r.since,
                    r.until
                )),
                margin,
                "/mo"
            ),
            r.owner.strip_prefix("no setting: ").unwrap_or(r.owner),
            assumed_tier(r)
        );
    }
    if r.uncertainty
        .iter()
        .any(|u| u == crate::keepalive::KEEPALIVE_SPLIT)
    {
        capitalize(crate::keepalive::KEEPALIVE_SPLIT) + "."
    } else if k.lifetime_ms.is_none() {
        k.base
            .assumptions
            .first()
            .and_then(|a| a.strip_prefix("cache lifetime unknown: "))
            .map_or_else(
                || "The main cache TTL is unknown, so pings could not be priced.".into(),
                |why| format!("{}, so pings could not be priced.", capitalize(why)),
            )
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
        // A priced replay extrapolates over unpriced periods, so only an unknown served tier
        // or, for Claude, an unpriced idle compaction alternative leaves it open.
        let why = if r.uncertainty.iter().any(|s| s == TIER_SENSITIVITY) {
            "effective service tiers could reverse it"
        } else {
            "idle compaction could not be priced"
        };
        format!(
            "Pinging {} would {} {} compared with no pings, but {why}.",
            crate::keepalive::policy(interval, best.cap_ms),
            if best.savings_usd > 0.0 {
                "save"
            } else {
                "cost"
            },
            approx(
                monthly(best.savings_usd.abs(), r.since, r.until),
                best.margins
                    .savings
                    .map(|m| m.scale(monthly(1.0, r.since, r.until))),
                "/mo"
            )
        )
    } else {
        "No idle period could be priced.".into()
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
        CheckResult::Effort(e) => {
            return e.control.unresolved.as_deref().map_or_else(
                || "The current level is unknown.".into(),
                |why| capitalize(why) + ".",
            );
        }
        _ => {}
    }
    // Whatever its replay found, a T3 instance whose owner is unresolved has no setting to change.
    if r.uncertainty.iter().any(|u| u == crate::t3::UNOWNED) {
        return "The T3 instance is disabled or the file that owns this setting for it is unresolved, so its past costs do not establish a setting to change.".into();
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
        // A replay with any priced part extrapolates to the rest, so only none leaves it open.
        CheckResult::Ttl(_) => "No requests with measured cache usage to compare.".into(),
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
            // A replay with any priced part extrapolates to the rest, so either none was
            // priced or the comparison itself is unsupported.
            match c
                .scenarios
                .iter()
                .find(|s| s.coverage.priced > 0)
                .and_then(|s| s.comparison_cost.unknown.first())
            {
                Some(reason) => capitalize(reason) + ".",
                None => "No request a different window would change has recorded usage to compare windows.".into(),
            }
        }
        CheckResult::TurnEnd(c) => turn_end_unresolved(c),
        CheckResult::T3Idle(c) => t3_idle_unresolved(c, r),
        CheckResult::Idle(i) => {
            let a = &i.after_recommendations;
            if a.delta.is_none() && no_share(Some(a.coverage)) {
                return NO_SHARE.into();
            }
            let periods = &i.after_recommendations.periods;
            if periods.total() > 0
                && periods.with_status(PeriodStatus::Censored)
                    + periods.with_status(PeriodStatus::Ineligible)
                    == periods.total()
            {
                return "Sessions were never idle long enough for it to trigger, so it made no difference.".into();
            }
            if periods.with_status(PeriodStatus::Eligible) == 0 {
                return "No idle gaps could be compared with it on and off.".into();
            }
            if i.after_recommendations.delta.is_some_and(|d| d.usd == 0.0) {
                return "On and off cost the same on recorded idle gaps, so neither is cheaper."
                    .into();
            }
            "Idle summaries or resumed sessions could not be priced in any session it could act on."
                .into()
        }
        // Handled above; a doctor run never panics on a combination nothing names.
        CheckResult::Speed(_)
        | CheckResult::Background(_)
        | CheckResult::Keepalive(_)
        | CheckResult::Memory(_)
        | CheckResult::Effort(_) => "No verdict could be reached.".into(),
    }
}

/// One plain sentence on why a turn-end compaction line has no verdict.
fn turn_end_unresolved(c: &crate::model::TurnEndCheck) -> String {
    if c.usable.is_empty() {
        return "No transcript records how much of the context window Codex could use, so a threshold has nothing to be a percentage of.".into();
    }
    let acting: Vec<&crate::model::WindowScenario> = c
        .scenarios
        .iter()
        .filter(|s| Some(s.window) != c.base.current)
        .filter(|s| s.affected_requests > 0 || !s.added.is_empty())
        .collect();
    if acting.is_empty() {
        let unknown = c
            .scenarios
            .iter()
            .flat_map(|s| &s.comparison_cost.unknown)
            .find(|u| u.contains("compaction summary"));
        let undecided = [
            crate::compaction::TURN_END_UNKNOWN,
            crate::compaction::BUILT_IN_UNKNOWN,
        ]
        .into_iter()
        .find(|reason| {
            c.scenarios
                .iter()
                .flat_map(|s| &s.comparison_cost.unknown)
                .any(|u| u.contains(reason))
        });
        return if unknown.is_some() {
            "Too few recorded compactions to model a summary at a turn's end.".into()
        } else if let Some(reason) = undecided {
            capitalize(reason) + "."
        } else if c.scenarios.len() < 2 {
            "No lower threshold can be replayed.".into()
        } else {
            "No recorded turn ended with enough context to compact at a candidate threshold.".into()
        };
    }
    if acting.iter().any(|s| s.savings_usd == Some(0.0)) {
        return "The best threshold and the current one cost the same on replay, so neither is cheaper.".into();
    }
    match acting
        .iter()
        .find(|s| s.savings_usd.is_none())
        .and_then(|s| s.comparison_cost.unknown.first())
    {
        Some(reason) => capitalize(reason) + ".",
        None => "No threshold could be priced against the current one.".into(),
    }
}

/// Whether a comparison left out sessions T3 could not settle with nothing settled to stand
/// for them, which leaves it unestimated.
fn no_share(coverage: Option<Coverage>) -> bool {
    coverage.is_some_and(|c| c.left_out > 0 && c.stand_ins.is_none())
}

/// Why a line has no estimate when it left out sessions nothing settled can stand for.
macro_rules! no_share {
    () => {
        "T3 could not settle some sessions between this line's owner and another, and nothing it did settle can stand for them: no request between their possible owners, or no session like them on this line."
    };
}
const NO_SHARE: &str = no_share!();
/// [`NO_SHARE`] as the reason a cost is unknown.
const NO_SHARE_COST: &str = concat!("Cost unknown: ", no_share!());

/// The modes T3 could price, and the first input the others lack.
fn t3_idle_unresolved(c: &T3IdleCheck, r: &Common<'_>) -> String {
    let scenarios = &c.after_recommendations.scenarios;
    if scenarios
        .iter()
        .any(|s| s.savings.is_none() && no_share(Some(s.coverage)))
    {
        return NO_SHARE.into();
    }
    let priced: Vec<String> = scenarios
        .iter()
        .filter_map(|s| {
            let e = s.estimate?.scale(monthly(1.0, r.since, r.until));
            Some(format!(
                "{} {}",
                s.mode.as_str(),
                approx(e.usd, e.margin, "")
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
        .filter(|s| s.estimate.is_none())
        .map(|s| s.mode.as_str())
        .collect();
    let reasons: IndexSet<&str> = scenarios
        .iter()
        .flat_map(|s| s.cost.unknown.iter().map(String::as_str))
        .collect();
    let first = match reasons.first() {
        Some(first) if scenarios.iter().any(|s| s.savings.is_none()) => first,
        _ => {
            if c.after_recommendations.periods.total() == 0 {
                return "No main-thread idle gaps in this window to compare.".into();
            }
            // Every mode is priced, so only a disabled instance or ownership keeps it advisory.
            return format!(
                "{priced}The instance is disabled or its sessions' ownership is unresolved, so no mode is recommended."
            );
        }
    };
    let more = match reasons.len() - 1 {
        0 => String::new(),
        1 => " (and 1 other missing input)".into(),
        n => format!(" (and {n} other missing inputs)"),
    };
    let modes = match unpriced[..] {
        _ if unpriced.len() == scenarios.len() => "No mode is priced".into(),
        // Each mode priced some session, but never one the current mode priced too.
        [] => "No session is priced in every mode".into(),
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
/// A skill's file is its location, named only when two lines would otherwise read the same.
fn short_label(c: &ContextCost, out: &Out<'_>) -> String {
    match c.kind {
        ContextKind::McpTool => format!("MCP tool {}", c.name),
        ContextKind::Skill if c.name != SYNCED_SKILLS => format!("Skill {}", c.name),
        _ => piece_label(c, out),
    }
}

/// What a tool, skill or MCP server does: Claude Code's own sentence for its built-in tools,
/// else the first sentence of the description it ships, else that none was recorded.
fn piece_what(c: &ContextCost) -> String {
    let shipped = c.description.clone();
    match c.kind {
        ContextKind::Skill if c.name == SYNCED_SKILLS => {
            "Skills from your claude.ai account, which Claude Code syncs into every session.".into()
        }
        ContextKind::Tool => (c.harness == Harness::Claude)
            .then(|| crate::descriptions::claude_tool(&c.name))
            .flatten()
            .map(str::to_string)
            .or(shipped)
            .unwrap_or_else(|| "A tool with no recorded description.".into()),
        ContextKind::McpServer if c.harness == Harness::Codex && c.name == "codex_apps" => {
            "ChatGPT apps, such as connectors to other services, that Codex can call as tools."
                .into()
        }
        ContextKind::McpServer => shipped.unwrap_or_else(|| {
            let tools: Vec<&str> = c
                .details
                .iter()
                .filter(|d| d.kind == ContextKind::McpTool)
                .map(|d| d.name.rsplit("__").next().unwrap_or(&d.name))
                .collect();
            match tools.as_slice() {
                [] => "An MCP server with no recorded description.".into(),
                [tool] => {
                    format!("An MCP server with no recorded description; its tool is {tool}.")
                }
                [first @ .., last] if tools.len() <= 4 => format!(
                    "An MCP server with no recorded description; its tools are {} and {last}.",
                    first.join(", ")
                ),
                _ => format!(
                    "An MCP server with no recorded description; its tools include {} and {} more.",
                    tools[..3].join(", "),
                    count(tools.len() as u64 - 3)
                ),
            }
        }),
        ContextKind::McpTool => {
            shipped.unwrap_or_else(|| "An MCP tool with no recorded description.".into())
        }
        ContextKind::Skill => {
            shipped.unwrap_or_else(|| "A skill with no recorded description.".into())
        }
        _ => shipped.unwrap_or_else(|| "Context with no recorded description.".into()),
    }
}

/// What an instruction file is.
fn instructions_what(c: &ContextCost) -> String {
    match c.harness {
        Harness::Claude => {
            "Instructions Claude Code loads from this file into each session it applies to."
        }
        Harness::Codex => {
            "Instructions Codex loads from this file into each session it applies to."
        }
    }
    .into()
}

/// The directory a skill was found in, which tells apart two skills with one name.
fn skill_location(c: &ContextCost, out: &Out<'_>) -> Option<String> {
    let path = c.skill.as_ref()?.path.as_ref()?;
    let dir = path
        .rsplit_once(['/', '\\'])
        .map_or(path.as_str(), |(dir, _)| dir);
    Some(out.home(dir))
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
        Some(1) if typed => "the model used it once".into(),
        Some(1) => "used once".into(),
        Some(calls) => format!(
            "{} {} times",
            if typed { "the model used it" } else { "used" },
            count(calls),
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
    /// The settings that change how its tools load.
    setting: Option<&'static str>,
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
                "Claude MCP loading: env.ENABLE_TOOL_SEARCH in settings.json defaults to deferral on supported models. true defers tools; false loads upfront. auto uses a 10% schema threshold; auto:N changes the percentage. A server's alwaysLoad=true loads upfront; alwaysLoad=false overrides tool-author upfront annotations on Claude Code 2.1.287+.",
                "Claude tool search needs Opus 4/Sonnet 4/Haiku 4.5 or later; Google Agent Platform needs 4.5 or later. Azure-hosted Foundry deployments do not support it. Experimental-beta restrictions, HIPAA policy, denying ToolSearch, and gateways can prevent deferral.",
                unmeasured,
            ],
            source: Some("https://code.claude.com/docs/en/mcp#configure-tool-search"),
            setting: Some("env.ENABLE_TOOL_SEARCH, or a server's alwaysLoad"),
        });
    }
    if codex {
        guidance.push(Loading {
            notes: vec![
                "Codex MCP loading: omit_tools_from changes exposure on direct (initial tools), deferred (tool search), and code_mode surfaces. Omission does not block calls or guarantee another surface exists. Use enabled_tools to allow tools or disabled_tools to block them; the deny list wins. A per-tool patch requires a native completion proving the raw tool name in every owning profile; exposed names may be sanitized or shortened.",
                unmeasured,
            ],
            source: Some("https://learn.chatgpt.com/docs/config-file/config-reference"),
            setting: Some("mcp_servers.<id>.omit_tools_from"),
        });
    }
    // ChatGPT apps alone have no loading switch, only what the report cannot measure.
    if any && !claude && !codex {
        guidance.push(Loading {
            notes: vec![unmeasured],
            source: None,
            setting: None,
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
    let Some(size) = c.instruction_size else {
        return vec![
            "Size unknown: this record has no rendered instruction text to measure.".into(),
        ];
    };
    let budget = match (c.harness, agents_budget(c, profiles)) {
        (Harness::Codex, Ok(n)) => format!(
            " Codex reads up to {} bytes of its source files, before headers and wrappers (project_doc_max_bytes).",
            count(n as u64)
        ),
        (Harness::Codex, Err(reason)) => format!(" {reason}"),
        (Harness::Claude, _) => String::new(),
    };
    vec![format!(
        "{name}: {}, {} UTF-8 bytes{}.{budget}",
        plural(size.lines as u64, "recorded line"),
        count(size.bytes as u64),
        if c.harness == Harness::Codex {
            ", including any headers or wrappers"
        } else {
            ""
        }
    )]
}

/// A setting that caps how much of a tool's output stays in context.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Cap {
    label: &'static str,
    /// What the cap does.
    what: &'static str,
    /// The setting's name.
    key: &'static str,
    /// Its current value, briefly.
    value: String,
    /// A sentence naming the setting and its value.
    text: String,
    /// Its default, when the setting is set.
    default: Option<&'static str>,
}

/// Claude Code's Bash output cap when no setting names one.
const BASH_OUTPUT_DEFAULT: &str = "30,000 chars";
/// Claude Code's MCP output cap when `MAX_MCP_OUTPUT_TOKENS` is unset.
const MCP_OUTPUT_DEFAULT: &str = "25,000 tokens";

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
    // A setting at its default value differs from nothing.
    let cap = |label, key, value: String, text: String, default: Option<&'static str>| Cap {
        label,
        what: match label {
            "Bash output" => "How much of a Bash command's output stays in the conversation.",
            "MCP output" => "How many tokens of an MCP tool's result stay in the conversation.",
            _ => "How many tokens of a tool's output Codex keeps in the conversation.",
        },
        key,
        default: default.filter(|d| *d != value),
        value,
        text,
    };
    Some(match c.harness {
        Harness::Claude if c.name == "Bash" => match (
            number("bashOutputMaxChars"),
            number("BASH_MAX_OUTPUT_LENGTH"),
        ) {
            (Some(n), _) => cap(
                "Bash output",
                "bashOutputMaxChars",
                format!("{n} chars"),
                format!("bashOutputMaxChars is {n} characters"),
                Some(BASH_OUTPUT_DEFAULT),
            ),
            (None, Some(n)) => cap(
                "Bash output",
                "env.BASH_MAX_OUTPUT_LENGTH",
                format!("{n} chars"),
                format!("BASH_MAX_OUTPUT_LENGTH is {n} characters"),
                Some(BASH_OUTPUT_DEFAULT),
            ),
            (None, None) => cap(
                "Bash output",
                "bashOutputMaxChars",
                "unset".into(),
                "bashOutputMaxChars is unset, so about 30,000 characters".into(),
                None,
            ),
        },
        Harness::Claude if c.name.starts_with("mcp__") => match number("MAX_MCP_OUTPUT_TOKENS") {
            Some(n) => cap(
                "MCP output",
                "env.MAX_MCP_OUTPUT_TOKENS",
                format!("{n} tokens"),
                format!("MAX_MCP_OUTPUT_TOKENS is {n} tokens"),
                Some(MCP_OUTPUT_DEFAULT),
            ),
            None => cap(
                "MCP output",
                "env.MAX_MCP_OUTPUT_TOKENS",
                "unset".into(),
                format!("MAX_MCP_OUTPUT_TOKENS is unset, so {MCP_OUTPUT_DEFAULT}"),
                None,
            ),
        },
        Harness::Codex => match number("tool_output_token_limit") {
            Some(n) => cap(
                "Tool output",
                "tool_output_token_limit",
                format!("{n} tokens"),
                format!("tool_output_token_limit is {n} tokens"),
                Some("Codex's built-in limit"),
            ),
            None => cap(
                "Tool output",
                "tool_output_token_limit",
                "unset".into(),
                "tool_output_token_limit is unset".into(),
                None,
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

/// How a replay's figure was extrapolated over the `items` it priced, such as "requests with
/// measured usage", when some were not.
fn coverage_note(r: &Common<'_>, items: &str) -> String {
    let Some(c) = r.coverage else {
        return String::new();
    };
    let mut note = String::new();
    // Percent, because one request can bill several parts.
    if c.priced != c.total {
        note += &format!(
            ", extrapolated from the {}% of {items} to the rest, assuming they resemble each other",
            percent(c.priced, c.total)
        );
    }
    if c.left_out > 0 {
        let units = items.split(' ').next().unwrap_or(items);
        let (unit, verb) = match c.left_out {
            1 => (units.strip_suffix('s').unwrap_or(units), "counts"),
            _ => (units, "count"),
        };
        note += &format!(
            "; {} {unit} T3 could not settle between this line's owner and another {verb} as {}, this line's share of those T3 did settle, assuming they split the same way, which the margin does not cover",
            count(c.left_out),
            count(c.stand_ins.unwrap_or(0))
        );
    }
    note
}

/// The sentence a verdict priced at an assumed service tier adds, or nothing.
fn assumed_tier(r: &Common<'_>) -> &'static str {
    if r.assumptions.iter().any(|s| s == TIER_SIGNS) {
        " Its dollars are at an assumed service tier, since none was recorded, but every session moved the same way, so no tier could reverse this unless one changed within a session."
    } else {
        ""
    }
}

/// What most replays extrapolate from.
const MEASURED: &str = "requests with measured usage";
/// What the idle replays extrapolate from, since they pair each session's runs.
const PRICED_SESSIONS: &str = "sessions that could be priced";

/// Context pieces as one row's figure: their sum, whether it is a lower bound beside pieces
/// nothing priced, and a sentence counting those.
fn pieces_figure(pieces: &[&ContextCost]) -> (Option<Estimate>, bool, String) {
    let mut total: Option<Estimate> = None;
    for e in pieces.iter().filter_map(|c| c.estimate()) {
        let sum = total.get_or_insert(Estimate {
            usd: 0.0,
            margin: None,
        });
        sum.usd += e.usd;
        // The pieces ride on the same requests, so their errors can add up.
        sum.margin = Margin::plus(sum.margin, e.margin);
    }
    let unpriced = pieces.iter().filter(|c| c.estimated_usd.is_none()).count() as u64;
    let note = match unpriced {
        0 => String::new(),
        1 => "1 piece could not be priced. ".into(),
        n => format!("{} pieces could not be priced. ", count(n)),
    };
    (total, total.is_some() && unpriced > 0, note)
}

/// The profile a piece of context is filed under: the first of its sorted set.
fn first_profile(c: &ContextCost) -> &str {
    c.profiles.first().map_or("", String::as_str)
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

/// Cells the plain-text verdict word takes, with its gap.
const WORD_CELLS: usize = 9;
/// Names and values wider than this are not padded, so one long name doesn't widen every line.
const NAME_CELLS: usize = 40;
const VALUE_CELLS: usize = 18;
const TAG_CELLS: usize = 24;
/// Dollar figures wider than this, such as "at least $1.41", are not right-aligned.
const FIGURE_CELLS: usize = 10;

/// One setting or feature: one line naming its verdict, value, figure and setting, with
/// its evidence on a second line under `--details`.
#[derive(Clone)]
struct Row {
    mark: Mark,
    name: String,
    /// One plain sentence saying what the setting or feature does.
    what: String,
    /// The current value, and the recommended one after an arrow for a change.
    value: String,
    /// What it saves per 30 days or costs over the window, as a verb and a figure.
    amount: Option<(&'static str, String)>,
    /// That figure's share of this host's spend, which ranks the line.
    share: Option<f64>,
    /// The profile it belongs to, when the report covers several.
    profile: String,
    /// The setting that changes it, or the edit `doctor` offers for a change.
    setting: String,
    evidence: String,
    /// The pages the evidence cites, such as a plan's billing rule.
    sources: Vec<&'static str>,
    /// Where it was found, shown only when another line would read the same.
    location: Option<String>,
    /// What sets the line apart from another of the same setting and tag, coarsest first:
    /// each is added to the tag only while two lines would still read the same.
    scopes: Vec<String>,
    /// The report line it is, for a line `doctor` may offer.
    origin: Option<Origin>,
    /// What it saves every 30 days, when measured.
    saving: Option<f64>,
    /// How far `saving` may be off when extrapolated over unpriced requests.
    margin: Option<Margin>,
    /// How much was measured, such as the requests a replay covered, ahead of the evidence.
    stats: String,
    /// What the current value saves against its default, when the two differ.
    baseline: Option<crate::defaults::Baseline>,
}

impl Row {
    fn new(mark: Mark, name: String, what: String) -> Self {
        Row {
            mark,
            name,
            what,
            value: String::new(),
            amount: None,
            share: None,
            profile: String::new(),
            setting: String::new(),
            evidence: String::new(),
            sources: vec![],
            location: None,
            scopes: vec![],
            origin: None,
            saving: None,
            margin: None,
            stats: String::new(),
            baseline: None,
        }
    }

    /// A change the report recommends or suggests, which leads the list.
    fn offered(&self) -> bool {
        matches!(self.mark, Mark::Change | Mark::Consider)
    }

    /// What it cost over the window, extrapolated over unpriced requests with a ~ and its
    /// margin, and a lower bound when `at_least`, such as beside pieces nothing priced.
    fn costs(&mut self, estimate: Option<Estimate>, at_least: bool, spend: &Spend) {
        self.margin = estimate.and_then(|e| e.margin);
        self.amount = Some((
            "costs",
            match estimate {
                None => "unknown".into(),
                Some(e) if at_least => format!("at least {}", approx(e.usd, e.margin, "")),
                Some(e) => approx(e.usd, e.margin, ""),
            },
        ));
        self.share = estimate.and_then(|e| spend.share(e.usd));
    }

    /// What it saves every 30 days, `verb` such as "saves up to", and how far that may be off
    /// when extrapolated; unmeasured when None.
    fn saves(
        &mut self,
        verb: &'static str,
        usd: Option<f64>,
        margin: Option<Margin>,
        spend: &Spend,
    ) {
        self.saving = usd;
        self.margin = margin.filter(|_| usd.is_some());
        match usd {
            Some(usd) => {
                self.amount = Some((verb, approx(usd, self.margin, "/mo")));
                self.share = spend.of_month(usd);
            }
            None => {
                self.amount = Some(("saving", "not measured".into()));
                self.share = None;
            }
        }
    }
}

/// `text` padded with spaces to `width` cells.
fn pad(text: &str, width: usize) -> String {
    let cells = cells(text);
    format!("{text}{}", " ".repeat(width.saturating_sub(cells)))
}

/// `text` right-aligned in `width` cells.
fn pad_left(text: &str, width: usize) -> String {
    format!("{}{text}", " ".repeat(width.saturating_sub(cells(text))))
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

/// Larger shares first, and lines without one after. A change ranks by what it saves and
/// everything else by what it costs, so a kept setting's comparison saving or a change shown
/// only by its cost ranks with the lines whose figure is unknown.
fn by_share(a: &Row, b: &Row) -> std::cmp::Ordering {
    let rank = |r: &Row| {
        let saving = r
            .amount
            .as_ref()
            .is_some_and(|(verb, _)| verb.starts_with("saves"));
        r.share.filter(|_| r.offered() == saving)
    };
    match (rank(a), rank(b)) {
        (Some(a), Some(b)) => b.total_cmp(&a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// Every row, ranked: the changes by what they save, then everything else by what it costs.
/// After `doctor`'s walk, the changes split into those accepted, declined and by hand.
fn rows_section(rows: &[Row], review: Option<Review<'_>>, out: &mut Out<'_>) {
    let mut rows = rows.to_vec();
    let choice = |row: &Row| {
        let choices = review?.choices;
        let origin = row.origin?;
        choices.iter().find(|(o, _)| *o == origin).map(|(_, c)| c)
    };
    // A change the walk could not make reads as the step it became.
    for row in &mut rows {
        if let Some(Choice::Manual(setting)) = choice(row) {
            row.setting = setting.clone();
        }
    }
    // Name where a line was found only when two lines of one profile would read the same.
    let mut seen = crate::collections::HashMap::<(String, String), usize>::default();
    for row in &rows {
        *seen
            .entry((row.name.clone(), row.profile.clone()))
            .or_default() += 1;
    }
    for row in &mut rows {
        if let Some(location) = row.location.take()
            && seen[&(row.name.clone(), row.profile.clone())] > 1
        {
            row.name = format!("{} ({location})", row.name);
        }
    }
    // A tag that names one settings file leaves the setting to name only what changes in it.
    for row in &mut rows {
        let Some(file) = row
            .profile
            .strip_prefix('[')
            .and_then(|t| t.strip_suffix(']'))
            .filter(|f| !f.contains(", ") && !f.contains(": "))
        else {
            continue;
        };
        if let Some(edit) = row.setting.strip_suffix(&format!(" in {file}")) {
            row.setting = edit.to_string();
        } else if let Some(key) = row.setting.strip_prefix(&format!("{file}:")) {
            row.setting = key.to_string();
        }
    }
    // Name what separates lines of one setting that read the same, such as the groups of
    // working directories it resolved apart for, each time with the next finer detail.
    let tags: Vec<String> = rows.iter().map(|r| r.profile.clone()).collect();
    for depth in 0..rows.iter().map(|r| r.scopes.len()).max().unwrap_or(0) {
        let mut seen = crate::collections::HashMap::<(String, String), usize>::default();
        for row in &rows {
            *seen
                .entry((row.name.clone(), row.profile.clone()))
                .or_default() += 1;
        }
        for (row, tag) in rows.iter_mut().zip(&tags) {
            if seen[&(row.name.clone(), row.profile.clone())] > 1
                && let Some(scope) = row.scopes.get(depth)
            {
                row.profile = match tag.strip_suffix(']') {
                    Some(tag) => format!("{tag}: {scope}]"),
                    None => format!("[{scope}]"),
                };
            }
        }
    }
    // A stable sort keeps the verdict order among lines with the same figure.
    rows.sort_by_key(|r| r.mark);
    rows.sort_by(by_share);
    let (changes, rest): (Vec<Row>, Vec<Row>) = rows.into_iter().partition(Row::offered);
    let all = || changes.iter().chain(&rest);
    let color = out.options.color;
    let (dot, hollow) = out.dots();
    let indent = if color { 2 } else { WORD_CELLS };
    // Columns line up across the groups.
    let value_cells = column(all().map(|r| cells(&terminal_text(&r.value))), VALUE_CELLS);
    let figure_cells = column(
        all().filter_map(|r| r.amount.as_ref().map(|(_, f)| cells(f))),
        FIGURE_CELLS,
    );
    let verb_cells = all()
        .filter_map(|r| r.amount.as_ref().map(|(v, _)| cells(v)))
        .max()
        .unwrap_or(0);
    let amount = |r: &Row| {
        r.amount.as_ref().map_or_else(String::new, |(verb, f)| {
            format!("{} {}", pad(verb, verb_cells), pad_left(f, figure_cells))
        })
    };
    let amount_cells = column(
        all().map(|r| cells(&amount(r))),
        verb_cells + 1 + FIGURE_CELLS,
    );
    let shares = |r: &Row| r.share.map_or_else(String::new, share_text);
    let share_cells = all().map(|r| cells(&shares(r))).max().unwrap_or(0);
    let tag_cells = column(all().map(|r| cells(&r.profile)), TAG_CELLS);
    let mut name_cells = column(all().map(|r| cells(&terminal_text(&r.name))), NAME_CELLS);
    // On a narrow terminal, the name column gives way so the columns up to the share still
    // fit on one row; the file and setting after them wrap to the next.
    if let Some(width) = out.options.width {
        let rest = MARGIN + indent + value_cells + amount_cells + share_cells + 6;
        name_cells = name_cells.min(width.saturating_sub(rest));
    }
    let mut groups: Vec<(Vec<&Row>, &str)> = match review {
        None => vec![(changes.iter().collect(), "CHANGES  biggest saving first")],
        Some(_) => {
            let group = |want: fn(Option<&Choice>) -> bool| {
                changes
                    .iter()
                    .filter(|r| want(choice(r)))
                    .collect::<Vec<_>>()
            };
            vec![
                (
                    group(|c| c == Some(&Choice::Accepted)),
                    "ACCEPTED  biggest saving first",
                ),
                (
                    // A change the walk did not reach is unaccepted too.
                    group(|c| matches!(c, None | Some(Choice::Declined))),
                    "NOT ACCEPTED",
                ),
                (group(|c| matches!(c, Some(Choice::Manual(_)))), "BY HAND"),
            ]
        }
    };
    groups.push((rest.iter().collect(), "EVERYTHING ELSE  biggest cost first"));
    for (index, (section, heading)) in groups.into_iter().enumerate() {
        if section.is_empty() {
            continue;
        }
        out.section(heading);
        for row in section {
            let marker = match (color, row.mark) {
                (true, Mark::Unknown | Mark::Info) => format!("{hollow} "),
                (true, _) => format!("{dot} "),
                (false, mark) => pad(mark.word(), WORD_CELLS),
            };
            // A line without a verdict is dim throughout; one with a verdict keeps its name,
            // value and figure bright, its saving green, and the rest dim.
            let verdict = !matches!(row.mark, Mark::Unknown | Mark::Info);
            let (bright, saving) = match (verdict, &row.amount) {
                (false, _) => (DIM, DIM),
                (true, Some((verb, _))) if verb.starts_with("saves") => (PLAIN, GREEN),
                (true, _) => (PLAIN, PLAIN),
            };
            let mut columns = vec![
                (pad(&terminal_text(&row.name), name_cells), bright),
                (pad(&terminal_text(&row.value), value_cells), bright),
                (pad(&amount(row), amount_cells), saving),
                (pad_left(&shares(row), share_cells), DIM),
            ];
            if all().any(|r| !r.profile.is_empty()) {
                columns.push((pad(&row.profile, tag_cells), DIM));
            }
            columns.push((row.setting.clone(), DIM));
            while columns.last().is_some_and(|(t, _)| t.trim().is_empty()) {
                columns.pop();
            }
            if let Some((last, _)) = columns.last_mut() {
                last.truncate(last.trim_end().len());
            }
            let mut spans = vec![(marker, if verdict { row.mark.style() } else { DIM })];
            for (i, (text, style)) in columns.into_iter().enumerate() {
                let gap = if i == 0 { "" } else { "  " };
                spans.push((format!("{gap}{text}"), style));
            }
            let spans: Vec<(&str, &str)> = spans.iter().map(|(t, s)| (t.as_str(), *s)).collect();
            out.spans(&spans, 0, Some(indent));
            if let Some(baseline) = &row.baseline {
                out.line(&baseline.text(), indent, DIM);
            }
            if out.options.details {
                out.line(&format!("What: {}", row.what), indent, PLAIN);
                out.line(&row.evidence, indent, PLAIN);
            }
        }
        if let (0, Some(review)) = (index, review) {
            out.line(review.total, indent, PLAIN);
        }
    }
    // Sources cite the guidance the evidence quotes, so they come with it.
    let sources: IndexSet<&str> = all().flat_map(|r| r.sources.clone()).collect();
    if out.options.details && !sources.is_empty() {
        let sources: Vec<&str> = sources.into_iter().collect();
        out.blank();
        out.line(&format!("Sources: {}", sources.join(", ")), 0, PLAIN);
    }
}

/// A setting's value in tokens, shortened to thousands or millions when exact.
pub(crate) fn tokens(n: i64) -> String {
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

/// An instruction file's row: what it cost over the window, its latest size, and what
/// shortening it saves. No verdict: what a line of instructions is worth is not recorded.
fn instruction_row(c: &ContextCost, out: &Out<'_>) -> Row {
    let mut row = Row::new(Mark::Info, out.home(&c.name), instructions_what(c));
    if let Some(size) = c.instruction_size.filter(|size| size.lines > 0) {
        row.value = plural(size.lines as u64, "line");
    }
    row.evidence = match instruction_saving(c, out) {
        Some(saving) => format!("{saving} No setting turns it off."),
        None => "No setting turns it off.".into(),
    };
    row.setting = "the file itself".into();
    row
}

/// What an average 100 lines of an instruction file cost every 30 days, each line taking an
/// even share of its revision, so about what shortening it by that much saves; for a file
/// now under 100 lines, what the whole file cost, which removing it saves. Neither exceeds
/// the file.
fn instruction_saving(c: &ContextCost, out: &Out<'_>) -> Option<String> {
    let lines = c.instruction_size?.lines;
    // Still None when a revision's size is unknown, as for a longer file.
    let hundred = c.hundred_lines?;
    let (figure, short) = if lines < 100 {
        (c.estimate()?, true)
    } else {
        (hundred, false)
    };
    let figure = figure.scale(monthly(1.0, out.spend.since, Some(out.spend.until)));
    let figure = approx(figure.usd, figure.margin, "/mo");
    let assuming = "assuming the rest of the work is unchanged.";
    Some(if short {
        format!(
            "Its {} cost about {figure}, which removing them saves, {assuming}",
            plural(lines as u64, "line")
        )
    } else {
        format!(
            "Each 100 lines costs about {figure} on average, about what shortening it by that much saves, {assuming}"
        )
    })
}

/// The provider's size guidance an instruction file is over, with what each 100 lines of it
/// costs: Claude Code's 200 lines per CLAUDE.md, or the byte budget Codex cuts AGENTS.md at.
fn instruction_warning(
    c: &ContextCost,
    profiles: &[Profile],
    profile: &str,
    out: &Out<'_>,
) -> Option<Item> {
    let size = c.instruction_size?;
    let claude_file = c
        .name
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|n| n == "CLAUDE.md" || n == "CLAUDE.local.md");
    let per = instruction_saving(c, out).unwrap_or_default();
    let (text, source) = match c.harness {
        Harness::Claude if claude_file && size.lines >= 200 => (
            format!(
                "{} lines; Claude Code suggests under 200 per CLAUDE.md, and path-scoped rules or skills for instructions only some work needs. {per}",
                count(size.lines as u64)
            ),
            CLAUDE_MEMORY_DOCS,
        ),
        // Rendered bytes include headers and wrappers, so only reaching the budget can mean
        // Codex truncated the source files.
        Harness::Codex => {
            let budget = agents_budget(c, profiles)
                .ok()
                .filter(|&budget| size.bytes >= budget)?;
            // A resolved budget is shared by the selected profiles; name every source of it.
            let mut sources: Vec<String> = vec![];
            for p in profiles.iter().filter(|p| c.profiles.contains(&p.id)) {
                let source = match &p.instruction_budget_file {
                    Some(file) => format!("{} sets", out.home(file)),
                    None => "Codex loads by default".into(),
                };
                if !sources.contains(&source) {
                    sources.push(source);
                }
            }
            (
                format!(
                    "{} bytes rendered, counting headers and wrappers, at or above the {} bytes {} (project_doc_max_bytes), so Codex may have cut it short. A command-line override is not recorded. {per}",
                    count(size.bytes as u64),
                    count(budget as u64),
                    sources.join(" and ")
                ),
                CODEX_AGENTS_DOCS,
            )
        }
        _ => return None,
    };
    Some(guidance(
        out.home(&c.name),
        profile,
        text.trim_end().into(),
        &[source],
    ))
}

/// Provider guidance that bears on the line `name` but that the transcripts cannot price,
/// which `vibes` shows as a cited card: what the provider says and what was measured beside
/// it, and the pages it cites.
fn guidance(name: String, profile: &str, text: String, sources: &[&str]) -> Item {
    Item {
        name: tagged(name, profile),
        value: String::new(),
        what: String::new(),
        text,
        sources: sources.iter().map(|s| s.to_string()).collect(),
        how: None,
        quiet: false,
        after: vec![],
    }
}

/// A line's name with its profile tag, when the report covers several profiles.
fn tagged(name: String, profile: &str) -> String {
    match profile {
        "" => name,
        tag => format!("{name} {tag}"),
    }
}

/// Whose advice the advisor pairing is, before Claude Code's page on the advisor.
const ADVISOR_PICK: &str =
    "The maintainer's pick of one rank above the main-session default. Claude Code's docs:";

/// What an advisor line's recorded calls cost, as sentences.
fn advisor_calls(u: &AdvisorUse) -> String {
    let mut text = if u.calls == 0 {
        if u.shared == 0 {
            "No advisor call is recorded in the window.".into()
        } else {
            String::new()
        }
    } else {
        let models: Vec<&str> = u.models.iter().map(|m| m.name.as_str()).collect();
        format!(
            "{} on {} ran on {}, costing {} at API prices over the window.",
            plural(u.calls, "advisor call"),
            plural(u.requests, "request"),
            models.join(", "),
            figure(u.estimate)
        )
    };
    if u.shared > 0 {
        let _ = write!(
            text,
            "{}{} that called the advisor also {} a profile on another advisor line, so whose {} is unknown; {} counted once on a shared line instead.",
            if text.is_empty() { "" } else { " " },
            plural(u.shared, "request"),
            if u.shared == 1 { "names" } else { "name" },
            if u.shared == 1 { "it was" } else { "they were" },
            if u.shared == 1 { "it is" } else { "they are" },
        );
    }
    text
}

/// The advisor's `doctor` line, for a settings line that sets one or recorded a call, or a
/// T3 or shared line, whose `control` is None: what the calls cost, with no verdict, since
/// what the advisor saves against a stronger main model is not measured.
fn advisor_row(u: &AdvisorUse, control: Option<&AdvisorControl>, out: &Out<'_>) -> Row {
    let mut row = Row::new(
        Mark::Info,
        "Advisor model".into(),
        "A second, stronger model Claude consults at decision points, which reads the whole conversation at its own rates.".into(),
    );
    row.sources = vec![crate::advisor::DOCS];
    let calls = advisor_calls(u);
    let (set, setting) = match &u.line {
        AdvisorLine::Settings => {
            let control = control.expect("a settings line has its control");
            row.value = control.model.clone().unwrap_or_else(|| "none".into());
            let set = match (&control.model, &control.blocked) {
                (Some(m), None) => format!("Set to {m}."),
                (Some(m), Some(b)) => {
                    format!("Set to {m}, but none would load from these settings: {b}.")
                }
                (None, _) => "No settings file read sets it now; a session's --advisor flag or /advisor turns it on.".into(),
            };
            (set, out.home(&control.owner))
        }
        AdvisorLine::T3 { owner } => (
            "T3 Code threads made these calls, and no native profile reads their settings file, whose advisorModel is not read: T3 picks each thread's main model, so its instances get no advisor card.".into(),
            out.home(owner),
        ),
        AdvisorLine::Removed => (
            "A T3 instance since removed from T3's settings made these calls, so no setting of its own is left to change.".into(),
            "no setting".into(),
        ),
        AdvisorLine::Shared { owners } => (
            "Each of these requests names profiles on more than one advisor line, as when two config roots read one projects directory, so whose they were is unknown; they count here once rather than on each line.".into(),
            owners
                .iter()
                .map(|o| out.home(o))
                .collect::<Vec<_>>()
                .join(", "),
        ),
    };
    row.evidence = out.home_in(&format!("{calls} {set}"));
    row.setting = setting;
    row
}

/// The advisor `vibes` suggests for a line of Claude profiles with no `advisorModel`, one
/// rank above its main-session default, or what stops one loading or keeps it unsuggested.
fn advisor_item(u: Option<&AdvisorUse>, p: &Profile, profile: &str, out: &Out<'_>) -> Item {
    use crate::advisor::Pick;
    let control = p.advisor.as_ref().expect("an advisor card has its control");
    let effort = p.effort.as_ref();
    let main = effort.and_then(|e| e.model.as_deref());
    // The window's calls, after a space, which show a blocked advisor still loaded somewhere.
    let calls = u.map_or_else(String::new, |u| format!(" {}", advisor_calls(u)));
    let file = control
        .owner
        .strip_suffix(":advisorModel")
        .unwrap_or(&control.owner);
    let owner = effort.map_or("", |e| e.model_owner.as_str());
    let quiet = |text: String| (String::new(), text, None, true);
    let (value, text, how, quiet) = match (&control.model, &control.blocked, main) {
        (Some(m), Some(b), _) => (
            String::new(),
            format!("Set to {m} in {file}, but none would load from these settings: {b}.{calls}"),
            None,
            false,
        ),
        // A full model ID is checked against Claude Code's pairing table; an alias's version
        // changes with Claude Code, so only Fable's rule judges one.
        (Some(m), None, _) => {
            let set = format!("Set to {m} in {file}");
            let advise = |text: String| (String::new(), text, None, false);
            match main.map(|main| (main, crate::advisor::pick(main))) {
                Some((main, Pick::Unsupported)) => advise(format!(
                    "{set}, but Claude Code attaches no advisor to {main}, the main-session default ({owner}).{calls}"
                )),
                Some((main, Pick::Top)) if crate::advisor::accepts(main, m) == Some(false) => {
                    advise(format!(
                        "{set}, but Claude Code pairs {main}, the main-session default ({owner}), only with a Fable advisor of its version or later.{calls}"
                    ))
                }
                Some((main, _)) if crate::advisor::accepts(main, m) == Some(false) => {
                    advise(format!(
                        "{set}, but Claude Code's pairing table doesn't take it as an advisor for {main}, the main-session default ({owner}), so none is attached.{calls}"
                    ))
                }
                _ => quiet(format!("{set}.{calls}")),
            }
        }
        // A recorded call shows the advisor loaded where the blocker did not reach.
        (None, Some(b), _) if u.is_some_and(|u| u.calls > 0) => quiet(format!(
            "No advisor is suggested, since none would load from these settings: {b}.{calls}"
        )),
        (None, Some(b), _) => quiet(format!(
            "No advisor is suggested, since none would load: {b}."
        )),
        (None, None, None) => quiet(format!(
            "No advisor is suggested: the main-session default is unknown, since {}.",
            effort
                .and_then(|e| e.unresolved.as_deref())
                .unwrap_or("no effort control was read")
        )),
        (None, None, Some(main)) => match crate::advisor::pick(main) {
            Pick::Top => quiet(format!(
                "No advisor is suggested: nothing ranks above {main}, the main-session default, and Claude Code pairs a Fable main model only with a Fable advisor."
            )),
            Pick::Unsupported => quiet(format!(
                "No advisor is suggested: Claude Code attaches none to {main}, the main-session default. It takes one on Fable, on Opus and Sonnet 4.6 or later, and on Haiku 4.5 or later."
            )),
            Pick::Advisor(pick) => {
                let mut text = format!(
                    "Claude consults the advisor at decision points rather than on every turn, so a faster main model with a stronger advisor \"typically costs less than running the stronger model throughout\", Claude Code's page says. {} ranks one above {main}, the main-session default ({owner}). Each call reads the whole conversation again at the advisor's rates, uncached, and what the pairing saves or costs is not measured.{calls} The advisor is experimental, and Amazon Bedrock, Google Cloud's Agent Platform and Microsoft Foundry have none.",
                    capitalize(pick)
                );
                if let Some(gateway) = &control.gateway {
                    let _ = write!(
                        text,
                        " {gateway} sends requests through a gateway, which must pass the advisor tool on intact."
                    );
                }
                if pick == "fable" {
                    text += " A Fable advisor needs Fable access, and on plans that bill Fable to usage credits, the one-time consent /model fable asks for; until then Claude Code sends requests without it.";
                }
                // T3's enabled instances read the same file under a main model T3 picks per
                // thread; a disabled one runs no threads.
                let t3: Vec<_> = out
                    .profiles
                    .values()
                    .filter(|o| {
                        o.harness == Harness::Claude
                            && crate::discovery::same_file(&o.settings_file, &p.settings_file)
                    })
                    .filter_map(|o| o.t3.as_ref())
                    .filter(|t| t.enabled)
                    .collect();
                if !t3.is_empty() {
                    let _ = write!(
                        text,
                        " T3 Code threads read {} too, and T3 picks each thread's main model, so each thread whose model and provider take this advisor would use it",
                        p.settings_file
                    );
                    if t3.iter().any(|t| t.settings_unresolved) {
                        text += ", unless an instance's launch arguments, secret environment or machine policy, which token-max can't resolve, override that file";
                    }
                    text += ".";
                }
                // The edit pairs the advisor with every project's model, so it is not offered
                // when a project's file picks the main model. A recorded main model still gets
                // it, saying so, since the walk shows the change and asks first.
                let how = if let Some(file) = project_file(owner, &p.settings_file) {
                    let _ = write!(
                        text,
                        " {file} sets that main model for its project only, and an advisor set in {} would pair with every project's model, so no edit is offered; set advisorModel by hand where that project reads it.",
                        p.settings_file
                    );
                    None
                } else {
                    if crate::effort::recorded(owner) {
                        text += " No setting names the main model, so a session's /model or --model choice may run another one; the advisor pairs with whichever runs, where that model accepts it.";
                    }
                    text += " A project whose own settings pick another main model takes this advisor too, where that model accepts it.";
                    Some(crate::doctor::advisor_how(&control.owner, pick, p))
                };
                (format!("none {} {pick}", out.arrow), text, how, false)
            }
        },
    };
    Item {
        name: tagged("Advisor model".into(), profile),
        value,
        what: "A second, stronger model Claude consults at decision points.".into(),
        text: out.home_in(&text),
        sources: vec![ADVISOR_PICK.into(), crate::advisor::DOCS.into()],
        how,
        quiet,
        // No `doctor` line writes the main model, `advisorModel` or what blocks the advisor.
        after: vec![],
    }
}

/// The project settings file in `owner`, a setting's owner such as `<file>:model` or
/// `<file>:env.ANTHROPIC_MODEL`: one in a `.claude` directory other than `user_file`'s.
/// None for the user's file, managed settings, the environment or a recorded model.
fn project_file<'a>(owner: &'a str, user_file: &str) -> Option<&'a str> {
    let file = owner
        .strip_suffix(":model")
        .or_else(|| owner.strip_suffix(":env.ANTHROPIC_MODEL"))?;
    let project = !crate::discovery::same_file(file, user_file)
        && std::path::Path::new(file)
            .parent()
            .and_then(|dir| dir.file_name())
            .is_some_and(|dir| dir == ".claude");
    project.then_some(file)
}

/// Whose advice the Bash cap's margin is, before the pages on how the cap and cache work.
const BASH_PICK: &str =
    "The maintainer's pick of a margin under the cache TTL. Claude Code's docs:";

/// A Bash timeout cap as `vibes` shows it: the long waits measured under it, the cap a
/// guessed margin under the cache TTL suggests, and what moving it trades.
fn bash_item(cap: &BashCap, profile: &str, how: Option<How>, out: &Out<'_>) -> Item {
    let w = &cap.waits;
    // What long foreground Bash waits cost: a total over the window, like their count.
    let mut waits = match (w.long, w.unknown) {
        (0, 0) => "No foreground Bash wait outlasted the cache.".to_string(),
        (0, _) => "No foreground Bash wait is known to have outlasted the cache.".to_string(),
        _ => format!(
            "{} outlasted the cache ({} hit the timeout, {} polled in a loop); the cache misses cost {} at API prices over the window.",
            plural(w.long, "foreground Bash wait"),
            count(w.timed_out),
            count(w.polling),
            figure(w.miss_estimate)
        ),
    };
    // A wait with no cache lifetime to compare is not one that fit under the cache.
    if w.unknown > 0 {
        waits += &format!(
            " {} of {} could not be compared, since no cache lifetime is recorded for the request that started it.",
            count(w.unknown),
            plural(w.calls, "foreground Bash wait")
        );
    }
    let current = duration(cap.current);
    let ttl = cap.ttl.map_or("unknown", Ttl::as_str);
    // An edit to the user file changes nothing where a project sets the cap itself.
    const UNREAD: &str = "Project and local settings, which override it, are not read.";
    let set_in = format!("Set in {}. {UNREAD}", out.home(&cap.owner));
    // Where the compared TTL came from: the setting, or the lifetime a request wrote.
    let basis = cap.ttl_basis.as_str();
    const BACKGROUND: &str = "A command past the cap is not killed: it keeps running in the background, where Claude checks on it in later turns.";
    let (value, text) = match cap.suggested {
        None => (
            String::new(),
            format!(
                "Capped at {current}{}. No cap is suggested: {}. {waits} {set_in}",
                // An owner that can't be resolved may not be the one these sessions ran under.
                if cap.unresolved.as_ref().is_some_and(|u| *u != cap.ttl_basis) {
                    " in the settings read, which these sessions may not have run under"
                } else {
                    ""
                },
                out.home_in(cap.unresolved.as_deref().unwrap_or("the cap is unresolved"))
            ),
        ),
        Some(suggested) if suggested == cap.current => (
            String::new(),
            format!(
                "Capped at {current}, the margin suggested under the {ttl} cache, {basis}. {waits} {set_in}"
            ),
        ),
        Some(suggested) => {
            let to = duration(suggested);
            let trade = if suggested < cap.current {
                format!(
                    "A foreground wait at or past the {ttl} cache TTL, {basis}, lets the cache expire before Claude replies, and a {to} cap leaves room under it. {waits} What a lower cap saves is not measured, and the {to} margin is a guess. {BACKGROUND} Some commands need longer in the foreground; keep the cap if yours do. {UNREAD}"
                )
            } else {
                format!(
                    "A {current} cap moves commands to the background sooner than the {ttl} cache, {basis}, needs. Raising it to {to} trades longer foreground waits for fewer background check-ins. {waits} What a higher cap saves is not measured, and the {to} margin is a guess. {BACKGROUND} {UNREAD}"
                )
            };
            (format!("{current} {} {to}", out.arrow), trade)
        }
    };
    let value_is_empty = value.is_empty();
    Item {
        name: tagged("Bash timeout cap".into(), profile),
        value,
        what: "The longest Claude waits on a foreground Bash command before Claude Code moves it to the background.".into(),
        text,
        sources: std::iter::once(BASH_PICK)
            .chain(crate::bash::SOURCES)
            .map(String::from)
            .collect(),
        how,
        quiet: value_is_empty,
        after: vec![],
    }
}

/// Whose advice the compaction window cap is.
const WINDOW_PICK: &str = "The maintainer's pick, for output quality rather than cost.";
/// Anthropic's page on context rot: accuracy and recall fall as a context grows.
const CONTEXT_ROT: &str = "https://platform.claude.com/docs/en/build-with-claude/context-windows";

/// Where a window card's line stands in `doctor`.
#[derive(Clone, Copy)]
enum Doctor {
    /// The line keeps the window on cost.
    Keeps,
    /// The line recommends this window on cost, a change the walk did not write.
    Recommends(i64),
    /// The line gives no verdict.
    Unknown,
    /// The walk wrote the line's change, which moved the window here.
    Wrote(i64),
}

/// A compaction line's window as `vibes` shows it beside the maintainer's cap: the window
/// left after `doctor`'s change, what `doctor`'s replay priced the cap at, and how the card
/// relates to that line's verdict. Its name and what it does are the caller's.
fn window_item(c: &CompactionCheck, profile: &Profile, doctor: Doctor, out: &Out<'_>) -> Item {
    let cap = crate::vibes::window_cap(profile.harness);
    let codex = profile.harness == Harness::Codex;
    let line = "doctor's auto-compact window line";
    let mut item = Item {
        name: String::new(),
        value: String::new(),
        what: String::new(),
        text: String::new(),
        sources: if codex {
            vec![WINDOW_PICK.to_string()]
        } else {
            vec![
                format!("{WINDOW_PICK} Anthropic's docs on context rot:"),
                CONTEXT_ROT.into(),
            ]
        },
        how: None,
        quiet: true,
        after: vec![],
    };
    let (window, now) = match (doctor, c.base.current) {
        (Doctor::Wrote(to), _) => (
            to,
            format!(
                "{line} recommended {} on cost, and the walk wrote it.",
                tokens(to)
            ),
        ),
        // With no request to name a model, the percentage override leaves the trigger unresolved
        // somewhere at or below the set window, so only a window under the cap is settled.
        (_, Some(w))
            if w > cap && c.scenarios.is_empty() && profile.compaction.percent.is_some() =>
        {
            item.text = format!(
                "Set to {}, but {} lowers where each model compacts under it, and with no request in the window to name a model token-max cannot tell whether that is above the {} window the maintainer caps it at.",
                tokens(w),
                crate::controls::PERCENT_KEY,
                tokens(cap)
            );
            return item;
        }
        (_, Some(w)) if c.unset => (
            w,
            format!(
                "No setting names it, so Claude Code compacts this model near {}, its window for the model.",
                tokens(w)
            ),
        ),
        (_, Some(w)) => match c.setting {
            Some(set) => (
                w,
                format!(
                    "Set to {}, which the model's {} context caps.",
                    tokens(set),
                    tokens(w)
                ),
            ),
            None => (w, format!("Set to {}.", tokens(w))),
        },
        // A smaller default would rise to the cap, so an unknown window is not advised down.
        (_, None) => {
            item.text = format!(
                "token-max could not resolve this window, so it cannot tell whether it is above the {} window the maintainer caps it at. Set by {}.",
                tokens(cap),
                out.home(&c.base.owner)
            );
            return item;
        }
    };
    if window <= cap {
        item.text = format!(
            "{now} That is at or under the {} window the maintainer caps it at, so there is nothing to change.",
            tokens(cap)
        );
        return item;
    }
    let why = if codex {
        "No OpenAI page says how output changes with the window, so this rests on the maintainer's pick alone."
    } else {
        "Anthropic's docs say accuracy and recall degrade as a context grows."
    };
    // The line's own replay of the cap, against the window it compared every candidate with.
    // A cap priced at an assumed service tier is unmeasured even when the line's winner is
    // settled, since no tier reversing the winner says nothing of the cap.
    let replayed = c.scenarios.iter().chain(&c.cap).find(|s| s.window == cap);
    // An unpriced replay says so first: recording the tier would not price it.
    let saving = replayed.and_then(|s| s.savings_usd);
    let assumed = saving.is_some()
        && replayed.is_some_and(|s| {
            crate::prices::tier_sensitive(&s.comparison_cost)
                || crate::prices::tier_sensitive(&s.baseline_comparison_cost)
        });
    let priced = replayed.filter(|_| !assumed).and_then(|s| {
        // Equal only when the replay changed nothing and left nothing unpriced.
        let same = s.savings_usd == Some(0.0)
            && s.affected_requests == 0
            && s.added.is_empty()
            && s.comparison_cost.unknown.is_empty()
            && s.baseline_comparison_cost.unknown.is_empty();
        Some((s.savings_usd?, s.margin, same))
    });
    let against = match (doctor, c.base.current) {
        (Doctor::Wrote(_), Some(w)) => format!("the {} window before that change", tokens(w)),
        (_, Some(w)) => format!("the {} window", tokens(w)),
        (_, None) => "the recorded requests".into(),
    };
    let cost = match priced {
        // Every recorded context stayed under the cap, so its replay is the window's own.
        Some((_, _, true)) => format!(
            "Against {against}, its replay of a {} window changes no recorded request, so it costs the same at API prices.",
            tokens(cap)
        ),
        Some((usd, margin, false)) => {
            let scale = monthly(1.0, c.base.since, c.base.until);
            let margin = margin.map(|m| m.scale(scale));
            let usd = usd * scale;
            let amount = if usd < 0.0 {
                format!("costs {} more", approx(-usd, margin, "/mo"))
            } else {
                format!("saves {}", approx(usd, margin, "/mo"))
            };
            // Both windows replay the TTL doctor's TTL line recommends, which may not be today's.
            let ttl = match (c.ttls.main, c.ttls.subagent) {
                (Some(main), Some(sub)) if sub == main => {
                    format!(", both at a {} cache TTL,", main.as_str())
                }
                (Some(main), Some(sub)) => format!(
                    ", both at a {} main-session and {} subagent cache TTL,",
                    main.as_str(),
                    sub.as_str()
                ),
                (Some(main), None) => {
                    format!(", both at a {} main-session cache TTL,", main.as_str())
                }
                (None, Some(sub)) => format!(", both at a {} subagent cache TTL,", sub.as_str()),
                (None, None) => ",".into(),
            };
            format!(
                "Against {against}{ttl} its replay of a {} window {amount} at API prices.",
                tokens(cap)
            )
        }
        None if assumed => format!(
            "Its replay of a {} window rests on an assumed service tier, so what the cap costs is not measured.",
            tokens(cap)
        ),
        None => format!(
            "Its replay did not price a {} window, so what the cap costs is not measured.",
            tokens(cap)
        ),
    };
    // How this card stands beside the line's measured verdict, which it does not change.
    let verdict = match doctor {
        Doctor::Wrote(_) => String::new(),
        Doctor::Keeps => format!(" {line} keeps it on cost."),
        Doctor::Recommends(r) => format!(" {line} recommends {} on cost.", tokens(r)),
        Doctor::Unknown => format!(" {line} gives no verdict on cost."),
    };
    item.text = format!(
        "{now} The maintainer would cap it at {} for better output, not a smaller bill: a smaller window compacts sooner, so the model works from a shorter context. {why}{verdict} {cost} The cap is advice beside that line, not its verdict.",
        tokens(cap)
    );
    item.value = format!("{} {} {}", tokens(window), out.arrow, tokens(cap));
    item.quiet = false;
    item.how = Some(match &c.unsettable {
        // As the line would, the change goes by hand where no edit is known to hold.
        Some(reason) => crate::doctor::window_by_hand(c, c.capped.as_ref(), cap, profile, reason),
        None => crate::doctor::window_change(c, c.capped.as_ref(), cap, profile),
    });
    item
}

/// A window card once `doctor`'s walk wrote a compaction window change that can move this
/// window in a way the scan cannot replay, such as a plan that pins other models' triggers.
fn window_rescan(c: &CompactionCheck, profile: &Profile, out: &Out<'_>) -> Item {
    Item {
        value: String::new(),
        text: format!(
            "doctor's walk just wrote a compaction window change that can move this window. Run token-max vibes again to compare the {} cap with the settings as they are now.",
            tokens(crate::vibes::window_cap(profile.harness))
        ),
        how: None,
        quiet: true,
        ..window_item(c, profile, Doctor::Unknown, out)
    }
}

/// Whose advice turning memory off is, before the providers' pages on it.
const MEMORY_PICK: &str = "The maintainer's pick: a lasting fact about a project belongs in its repository. How memory works:";

/// Auto memory's line as `vibes`' card reads it.
struct MemoryLine<'a> {
    result: &'a CheckResult,
    common: &'a Common<'a>,
    /// The line's name, with its profile tag.
    name: String,
    /// Whether `doctor`'s line recommends or suggests turning it off.
    offered: Option<Offered>,
}

/// `doctor`'s change to turn auto memory off, as its walk takes it.
#[derive(Clone, Copy)]
struct Offered {
    /// Worth considering, rather than recommended.
    suggested: bool,
    /// Whether the walk takes it up, which it doesn't for an edit saving too little to ask.
    asked: bool,
}

/// Auto memory as `vibes` shows it: off is advised when most main sessions ran in a
/// repository with an instruction file to keep facts in, beside `doctor`'s measured cost and
/// uses. A card that leaves the question to `doctor`, or advises nothing, is quiet and has no
/// change; the caller gives the others theirs.
fn memory_item(line: MemoryLine<'_>, out: &Out<'_>) -> Item {
    let CheckResult::Memory(m) = line.result else {
        unreachable!("a memory line")
    };
    let r = line.common;
    let p = m.places;
    let mut share = match p.sessions {
        0 => "No main session ran in the window, so where the work happens is unknown.".to_string(),
        n => format!(
            "{} of {} ({}%) in the window ran in a repository with a CLAUDE.md or AGENTS.md",
            count(p.instructed),
            plural(n, "main session"),
            percent(p.instructed, n)
        ),
    };
    if p.unknown > 0 {
        let _ = write!(
            share,
            ", counting as without {} whose transcript recorded loading none and whose directory is gone or unrecorded",
            plural(p.unknown, "session")
        );
    }
    if p.sessions > 0 {
        share.push('.');
    }
    const REASON: &str = "A lasting fact about a project belongs in that file, where it is reviewed and every host and teammate sees it, not saved silently on one machine.";
    // A total that leaves a part out says so wherever it is quoted, as doctor's line does.
    let mut left = vec![];
    if m.unmeasured_generation {
        left.push("Codex's extraction and consolidation of memories".to_string());
    }
    if m.unpriced_reads > 0 {
        left.push(format!(
            "memory read back by {} that recorded no context",
            plural(m.unpriced_reads, "request")
        ));
    }
    let measured = match r.savings_usd {
        Some(usd) if !left.is_empty() => format!(
            "doctor's line measured at least {} at API prices over the window, {}; that leaves out {}, which could not be priced",
            figure(check_estimate(Some(usd), r)),
            memory_uses(m),
            left.join(" and ")
        ),
        Some(usd) => format!(
            "doctor's line measured {} at API prices over the window, {}",
            figure(check_estimate(Some(usd), r)),
            memory_uses(m)
        ),
        None => format!(
            "doctor's line could not price it over the window ({})",
            memory_uses(m)
        ),
    };
    let (value, quiet, text) = match (m.base.current, line.offered) {
        (Some(false), _) => (String::new(), true, format!("Off. {share}")),
        _ if !p.most() => (
            String::new(),
            true,
            format!(
                "{share} Outside a repository, memory may be the only place a fact is kept, so turning it off is not advised. {measured}."
            ),
        ),
        (
            _,
            Some(Offered {
                suggested,
                asked: true,
            }),
        ) => (
            String::new(),
            true,
            format!(
                "{share} {REASON} {measured}, and already {} turning it off for its cost per use, so doctor asks and this card does not.",
                if suggested { "suggests" } else { "recommends" }
            ),
        ),
        (current, offered) => (
            format!(
                "{} {} off",
                if current.is_some() { "on" } else { "may be on" },
                out.arrow
            ),
            false,
            match offered {
                Some(Offered { suggested, .. }) => format!(
                    "{share} {REASON} {measured}, and already {} turning it off for its cost per use, but its walk does not ask about a change saving under {}/mo.",
                    if suggested { "suggests" } else { "recommends" },
                    about(crate::doctor::ASKED_USD)
                ),
                None => format!(
                    "{share} {REASON} {measured}; judging only cost, it offers no change. {}",
                    memory_reason(m, r)
                ),
            },
        ),
    };
    Item {
        name: line.name,
        value,
        what: crate::descriptions::check(line.result).into(),
        text,
        sources: vec![
            MEMORY_PICK.into(),
            crate::memory::SOURCES[usize::from(m.harness == Harness::Codex)].into(),
        ],
        how: None,
        quiet,
        after: vec![],
    }
}

/// What the maintainer picked for how long Claude Code keeps transcripts, before the pages
/// on the setting and on what the sweep deletes.
const RETENTION_PICK: &str = "The maintainer's pick of 365 days, which keeps a year of history without growing disk without bound. Claude Code's docs:";

/// The days of transcripts Claude Code is advised to keep, a year.
const RETENTION_DAYS: u64 = 365;

/// Whether the retention a profile's settings give is the one in force: a T3 instance that is
/// disabled, or whose launch arguments may replace the native settings, may keep other
/// transcripts than that file says, so no edit to it is offered.
fn retention_settled(profile: &Profile) -> bool {
    profile.retention.is_some()
        && profile
            .t3
            .as_ref()
            .is_none_or(|t| t.enabled && !t.settings_unresolved)
}

/// How long Claude Code keeps transcripts, as `vibes` shows it when it is under a year, with
/// the disk the transcripts token-max read take today and `how` to raise it.
fn retention_item(
    control: &RetentionControl,
    days: u64,
    profile: &str,
    disk: u64,
    how: How,
    out: &Out<'_>,
) -> Item {
    let (to, kept) = (plural(RETENTION_DAYS, "day"), plural(days, "day"));
    // Where the sweep reads the days from: a file's key, or the key the default would go in.
    let set_in = match control.owner.strip_suffix(" (default)") {
        Some(owner) => format!("Not set; it would go in {}.", out.home(owner)),
        None => format!("Set in {}.", out.home(&control.owner)),
    };
    let held = if disk > 0 {
        format!(" The transcripts token-max read take {} today.", size(disk))
    } else {
        String::new()
    };
    Item {
        name: tagged("Transcript retention".into(), profile),
        value: format!("{kept} {} {to}", out.arrow),
        what:
            "How long Claude Code keeps session transcripts before its startup sweep deletes them."
                .into(),
        text: format!(
            "Claude Code deletes eligible transcripts after {kept}, so token-max has less history to replay.{held} Keeping {to} costs disk that grows with use. The setting only stops future deletion: history already deleted is gone. {set_in} Project and local settings, which override it, are not read."
        ),
        sources: [
            RETENTION_PICK,
            "https://code.claude.com/docs/en/settings-reference#cleanupperioddays",
            "https://code.claude.com/docs/en/claude-directory#cleaned-up-automatically",
        ]
        .map(String::from)
        .into(),
        how: Some(how),
        quiet: false,
        after: vec![],
    }
}

/// A check's verdict before its line is drawn.
///
/// A comparison with no requests is not a verdict, whatever the tie-break said. Window
/// coverage counts only changed parts, so zero there can still be a keep.
fn check_action(result: &CheckResult, r: &Common<'_>) -> Action {
    let empty = matches!(
        result,
        CheckResult::Ttl(_) | CheckResult::Compaction(_) | CheckResult::TurnEnd(_)
    ) && r.requests == 0;
    if empty { Action::Advisory } else { r.action }
}

/// A check line's mark. A measured background generation over the limit is advisory only
/// because its uses are not recorded, auto memory over the limit per use is advisory as a
/// skill is, and fast mode's premium buys speed no transcript values, so each is worth
/// considering off. Other advisories lack evidence.
fn check_mark(result: &CheckResult, r: &Common<'_>) -> Mark {
    match check_action(result, r) {
        Action::Change => Mark::Change,
        // An effort is a cost with no verdict.
        Action::Keep if matches!(result, CheckResult::Effort(_)) => Mark::Info,
        Action::Keep => Mark::Keep,
        Action::Advisory => match result {
            CheckResult::Background(_) if r.savings_usd.is_some() => Mark::Consider,
            CheckResult::Memory(_) | CheckResult::Speed(_) if r.recommended.is_some() => {
                Mark::Consider
            }
            _ => Mark::Unknown,
        },
    }
}

/// A line the report asks the person to act on.
pub(crate) struct Recommendation<'a> {
    /// Worth considering, rather than a recommended change.
    pub suggested: bool,
    /// The line's name, such as "Main-agent prompt cache TTL".
    pub name: String,
    /// The current value and the one to change to, such as "5m -> 1h".
    pub value: String,
    /// What it saves every 30 days, when measured.
    pub saving: Option<f64>,
    /// How far `saving` may be off when extrapolated over unpriced requests.
    pub margin: Option<Margin>,
    /// How the saving reads, such as "saves up to".
    pub verb: &'static str,
    /// The saving's share of this host's spend every 30 days.
    pub share: Option<f64>,
    pub source: Source<'a>,
    pub origin: Origin,
}

/// What a recommendation's line reports on.
pub(crate) enum Source<'a> {
    Check(&'a CheckResult),
    Piece(&'a ContextCost),
}

/// The value a recommended or suggested check changes to: a suggestion turns a feature
/// off, or sets fast mode's standard speed.
fn check_target(result: &CheckResult, r: &Common<'_>, mark: Mark) -> String {
    match (mark, result) {
        (Mark::Change, _) | (_, CheckResult::Speed(_)) => match r.recommended {
            Some(Setting::Window(n)) => tokens(n),
            other => setting_value(other),
        },
        _ => "off".into(),
    }
}

/// Which line of a report a change comes from, so the list can name the edit offered for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Origin {
    /// A check, by its index in the report's checks.
    Check(usize),
    /// A piece of context, by its index among `setting_rows` of the report's context.
    Context(usize),
    /// One of `vibes`' items, by its index among them; never a line of this report.
    Vibe(usize),
}

/// Every change the report recommends or suggests, in its order: biggest monthly saving
/// first, and those whose saving was not measured after the rest.
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
    for (index, result) in report.checks.iter().enumerate() {
        let r = common(result);
        let mark = check_mark(result, &r);
        if !matches!(mark, Mark::Change | Mark::Consider) {
            continue;
        }
        let target = check_target(result, &r, mark);
        let mut current = if unset_main_ttl(result, &r, &out) {
            "unset".into()
        } else {
            match r.current {
                Some(Setting::Window(n)) => tokens(n),
                other => setting_value(other),
            }
        };
        if matches!(result, CheckResult::Compaction(c) if c.unset)
            || matches!(result, CheckResult::TurnEnd(c) if c.unset)
            || answered_main_ttl(result, &r, &out).is_some()
        {
            current += " (default)";
        }
        let saving = check_saving(result, &r, &out);
        lines.push(Recommendation {
            suggested: mark == Mark::Consider,
            name: setting_name(result),
            value: format!("{current} {arrow} {target}"),
            saving,
            margin: check_margin(result, &r).filter(|_| saving.is_some()),
            verb: check_verb(result, &r, &out),
            share: saving.and_then(|usd| out.spend.of_month(usd)),
            source: Source::Check(result),
            origin: Origin::Check(index),
        });
    }
    for (index, c) in setting_rows(&report.context).enumerate() {
        let mark = match c.verdict {
            // A switch the file already sets is a change made before any request showed it.
            _ if crate::defaults::applied(report, c).is_some() => continue,
            Some(Verdict::Off) => Mark::Change,
            Some(Verdict::Consider) => Mark::Consider,
            _ => continue,
        };
        let target = if typed_only(c.control.as_ref()) {
            "typed only"
        } else {
            "off"
        };
        let saving = piece_saving(c, &out.spend);
        lines.push(Recommendation {
            suggested: mark == Mark::Consider,
            name: short_label(c, &out),
            value: format!("on {arrow} {target}"),
            saving,
            margin: piece_margin(c, &out.spend).filter(|_| saving.is_some()),
            verb: "saves",
            share: saving.and_then(|usd| out.spend.of_month(usd)),
            source: Source::Piece(c),
            origin: Origin::Context(index),
        });
    }
    // A stable sort keeps the report's order among equal savings.
    lines.sort_by(|a, b| match (a.saving, b.saving) {
        (Some(a), Some(b)) => b.total_cmp(&a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    lines
}

/// The headline: how many changes the report recommends and how many are worth considering,
/// as bold spans with each count in its verdict's color.
fn headline(changes: usize, considered: usize) -> Vec<(String, &'static str)> {
    const BOLD_RED: &str = "1;31";
    const BOLD_YELLOW: &str = "1;33";
    const BOLD_GREEN: &str = "1;32";
    let s = if changes == 1 { "" } else { "s" };
    let parts: Vec<(String, &'static str)> = match (changes, considered) {
        (0, 0) => vec![(
            "No changes recommended or worth considering".into(),
            BOLD_GREEN,
        )],
        (0, c) => vec![
            ("No changes recommended; ".into(), BOLD),
            (format!("{c} worth considering"), BOLD_YELLOW),
        ],
        (r, 0) => vec![(format!("{r} change{s} recommended"), BOLD_RED)],
        (r, c) => vec![
            (format!("{r} change{s} recommended"), BOLD_RED),
            (" and ".into(), BOLD),
            (format!("{c} worth considering"), BOLD_YELLOW),
        ],
    };
    parts
        .into_iter()
        .chain(std::iter::once((".".into(), BOLD)))
        .collect()
}

/// The verb of a saving that holds only when a default would otherwise be the other value.
pub(crate) const UP_TO: &str = "saves up to";
/// The verb of a saving that leaves part of what it would save unmeasured.
pub(crate) const AT_LEAST: &str = "saves at least";

/// How a recommended or suggested check's saving reads, such as "saves up to".
fn check_verb(result: &CheckResult, r: &Common<'_>, out: &Out<'_>) -> &'static str {
    match result {
        _ if unset_main_ttl(result, r, out) => UP_TO,
        // Memory with unmeasured generation or reads saves at least what was priced.
        CheckResult::Memory(m) if m.lower_bound() => AT_LEAST,
        _ => "saves",
    }
}

/// What a recommended or suggested check saves every 30 days, when it was measured.
fn check_saving(result: &CheckResult, r: &Common<'_>, out: &Out<'_>) -> Option<f64> {
    match result {
        // A lower or higher effort changes the output, which the transcripts cannot price.
        CheckResult::Effort(_) => None,
        // Either default may be in force, so the saving is the whole gap or nothing.
        CheckResult::Ttl(t) if unset_main_ttl(result, r, out) => {
            Some(monthly(t.one_hour_savings_usd.abs(), r.since, r.until))
        }
        _ => r
            .monthly_savings_usd
            .or_else(|| r.savings_usd.map(|usd| monthly(usd, r.since, r.until))),
    }
}

/// A check's measured figure over the window, with its margin when extrapolated.
fn check_estimate(usd: Option<f64>, r: &Common<'_>) -> Option<Estimate> {
    usd.map(|usd| Estimate {
        usd,
        margin: r.margin,
    })
}

/// How far a check's monthly saving may be off when extrapolated over unpriced requests,
/// scaled from the window to 30 days as the saving is.
fn check_margin(result: &CheckResult, r: &Common<'_>) -> Option<Margin> {
    match result {
        CheckResult::Effort(_) => None,
        _ => r.margin.map(|m| m.scale(monthly(1.0, r.since, r.until))),
    }
}

/// What turning a piece of context off saves every 30 days: what it cost, all else equal.
///
/// A Codex catalog skill's listing cost is no such saving, since removing it can backfill the
/// catalog or expand the other descriptions.
fn piece_saving(c: &ContextCost, spend: &Spend) -> Option<f64> {
    c.estimated_usd
        .filter(|_| c.skill.is_none())
        .map(|usd| monthly(usd, spend.since, Some(spend.until)))
}

/// How far [`piece_saving`] may be off when extrapolated over unpriced requests.
fn piece_margin(c: &ContextCost, spend: &Spend) -> Option<Margin> {
    c.margin
        .map(|m| m.scale(monthly(1.0, spend.since, Some(spend.until))))
}

/// The settings file a piece's switch lives in: its first profile's.
fn piece_file(c: &ContextCost, out: &Out<'_>) -> String {
    out.profiles
        .get(first_profile(c))
        .map_or_else(|| "settings.json".into(), |p| out.home(&p.settings_file))
}

/// The setting that turns a piece of context off, such as `~/.claude/settings.json:permissions.deny`.
fn piece_setting(c: &ContextCost, out: &Out<'_>) -> String {
    match (&c.control, &c.skill) {
        (_, Some(s)) if s.path.is_none() => "no setting resolved".into(),
        (Some(control), _) => format!("{}:{}", piece_file(c, out), control.path.join(".")),
        (None, Some(_)) => format!("{}:skills.config", piece_file(c, out)),
        (None, None) if c.kind == ContextKind::Skill => "its plugin, with /plugin".into(),
        (None, None) if c.kind == ContextKind::McpServer => {
            "the plugin or launcher that supplies it".into()
        }
        (None, None) => "no setting".into(),
    }
}

/// The sessions the selected requests ran in, each subagent counted with the session that
/// spawned it.
pub(crate) fn sessions(scan: &crate::model::Scan) -> u64 {
    let parents: HashMap<&str, &str> = scan
        .subagents
        .iter()
        .filter_map(|s| Some((&*s.session, &**s.parent.as_ref()?)))
        .collect();
    let roots: HashSet<&str> = scan
        .requests
        .iter()
        .map(|r| {
            let mut session: &str = &r.session;
            // Bounded so a cycle in recorded lineage cannot loop.
            for _ in 0..parents.len() {
                match parents.get(session) {
                    Some(parent) => session = parent,
                    None => break,
                }
            }
            session
        })
        .collect();
    roots.len() as u64
}

/// The report: what was read, then every setting and feature on one line, the changes
/// first, with each line's evidence under `--details`.
/// Returns the text before the list, the list's rows, its advice and what the settings save
/// against the defaults; what follows them stays in `out`.
fn summary(
    report: &Report,
    fixes: &[crate::doctor::Fix],
    out: &mut Out<'_>,
) -> (
    String,
    String,
    Vec<Row>,
    Vec<Item>,
    Option<crate::defaults::Credit>,
) {
    let arrow = out.arrow;
    let coverage = &report.scan.coverage;
    let spend = out.spend;
    let many = report.scan.profiles.len() > 1;
    let spent = spend.text();
    let title = out.title(&match spend.since {
        Some(since) if coverage.requests > 0 => format!(
            "{} requests in {} from {} to {} ({}), {spent} at API prices",
            count(coverage.requests),
            plural(sessions(&report.scan), "session"),
            date(&iso(since)),
            date(&report.generated_at),
            days(Some(since), Some(spend.until))
        ),
        _ => format!(
            "No requests found{}",
            report
                .scan
                .since
                .map_or_else(String::new, |s| format!(" since {}", date(&iso(s))))
        ),
    });
    let profiles: Vec<String> = report
        .scan
        .profiles
        .iter()
        .map(|p| out.short_profile(&p.id))
        .collect();
    if report.scan.defaults {
        out.line(
            "As a user with no settings files would see it (--defaults): every user, project and local settings file, T3 Code's settings and the shell's setting variables are pretended away, so each setting is at its Claude Code or Codex default and every tool, skill and MCP server is on. Managed and system policy still apply.",
            0,
            PLAIN,
        );
    }
    if !profiles.is_empty() {
        out.line(&format!("Profiles: {}.", profiles.join(", ")), 0, DIM);
    }
    t3_coverage(report, out);
    out.line(&format!("{PRICES_MEAN} Nothing was changed."), 0, DIM);
    let conditional: u64 = report
        .historical
        .iter()
        .filter(|g| !g.tier_sensitivity.is_empty())
        .map(|g| g.conditional_requests)
        .sum();
    if conditional > 0 {
        out.line(
            &format!(
                "{} Codex requests are priced at an assumed service tier, which their records leave unclear; --details shows each tier's cost.",
                count(conditional)
            ),
            0,
            DIM,
        );
    }
    if coverage.missing_usage > 0 {
        out.line(
            &format!(
                "{} requests have no recorded usage and are left out of comparisons.",
                count(coverage.missing_usage)
            ),
            0,
            DIM,
        );
    }
    if coverage.skipped_lines > 0 {
        out.line(
            &format!(
                "{} unreadable transcript lines in {} files were skipped. One holds back a recommendation only when it names the feature or a tool result in its file has no call; a call cut off before its result was written would not show.",
                count(coverage.skipped_lines),
                count(coverage.skipped_files)
            ),
            0,
            DIM,
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

    let checks: Vec<(usize, &CheckResult, Common<'_>)> = report
        .checks
        .iter()
        .enumerate()
        .map(|(i, c)| (i, c, common(c)))
        .collect();
    // A check for a working directory that no longer exists has no setting left
    // to change; show it only when it is the sole result for its setting.
    let key = |(_, result, r): &(usize, &CheckResult, Common<'_>)| {
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
    let action = |(_, result, r): &(usize, &CheckResult, Common<'_>)| check_action(result, r);
    let decided: crate::collections::HashSet<String> = checks
        .iter()
        .filter(|c| action(c) != Action::Advisory)
        .map(key)
        .collect();
    // Only a vanished directory is proven irrelevant; any other unresolved owner,
    // such as a project override, stays visible beside the resolved verdict.
    let checks: Vec<(usize, &CheckResult, Common<'_>)> = checks
        .into_iter()
        .filter(|c| {
            !(action(c) == Action::Advisory
                && matches!(c.1, CheckResult::Speed(s) if s.reason.contains("working directory no longer exists"))
                && decided.contains(&key(c)))
        })
        .collect();
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
    // A line names the settings file that owns it, or its profiles when no file does.
    let tag = |ids: &[String], owner: &dyn Fn(&Profile) -> String, out: &Out<'_>| {
        if !many {
            return String::new();
        }
        let files: Option<IndexSet<String>> = ids
            .iter()
            .map(|id| {
                let p = out.profiles.get(id.as_str())?;
                crate::rows::owner_file(p, &owner(p)).map(|file| out.home(file))
            })
            .collect();
        let names = files.filter(|f| !f.is_empty()).unwrap_or_else(|| {
            ids.iter()
                .map(|id| {
                    tags.get(id.as_str())
                        .cloned()
                        .unwrap_or_else(|| out.short_profile(id))
                })
                .collect()
        });
        format!("[{}]", names.into_iter().collect::<Vec<_>>().join(", "))
    };
    // A tool, skill or MCP switch lives in each profile's own settings file.
    let switch = |p: &Profile| format!("{}:", p.settings_file);
    // The edit or step `doctor` offers for a line, which names its setting.
    let offer = |origin: Origin, out: &Out<'_>| {
        fixes
            .iter()
            .find(|f| f.origins.contains(&origin))
            .map(|f| crate::doctor::setting(f, out.home.as_deref()))
    };
    // A switch the settings file already sets shows as off below, even while the latest
    // request, made before the change, still carried the feature.
    let applied = |c: &ContextCost| crate::defaults::applied(report, c).is_some();
    let pieces: Vec<(usize, &ContextCost)> = setting_rows(&report.context)
        .enumerate()
        .filter(|(_, c)| c.verdict.is_some() && !applied(c))
        .collect();
    // Under --defaults, how a feature the settings files turn off is priced on.
    let pretended_note = |c: &ContextCost| match c.pretended_requests {
        Some(all) => format!(
            " A settings file turns it off, so {} of this line's {} requests carried it: its cost is scaled from those to all of them, and its uses are theirs.",
            count(c.requests),
            count(all)
        ),
        None => String::new(),
    };
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
    let check_row = |c: &(usize, &CheckResult, Common<'_>), out: &Out<'_>| {
        let (index, result, r) = c;
        let mark = check_mark(result, r);
        let mut row = Row::new(
            mark,
            setting_name(result),
            crate::descriptions::check(result).into(),
        );
        row.origin = Some(Origin::Check(*index));
        row.baseline = crate::defaults::check(result, out.profiles.get(r.profile).copied());
        if r.requests > 0 {
            row.stats = format!(
                "Measured on {} over {}.",
                plural(r.requests, "request"),
                days(r.since, r.until)
            );
        }
        row.profile = tag(r.profiles, &|_| r.owner.to_string(), out);
        if let CheckResult::Background(b) = result {
            row.scopes = background_scopes(b, out);
        }
        let unset = unset_main_ttl(result, r, out);
        let current = match r.current {
            None if unset => "unset".into(),
            Some(Setting::Window(n)) => tokens(n),
            other => setting_value(other),
        };
        row.value = current.clone();
        if matches!(result, CheckResult::Compaction(c) if c.unset)
            || matches!(result, CheckResult::TurnEnd(c) if c.unset)
            || answered_main_ttl(result, r, out).is_some()
        {
            row.value += " (default)";
        }
        let official = match plan_note(result, r, out) {
            Some((note, source)) => {
                row.sources.push(source);
                format!("{} ", capitalize(note))
            }
            None => String::new(),
        };
        let set_in = format!("{official}Set in {}.", out.home(r.owner));
        let unknown = || {
            format!(
                "{}{}{} {set_in}",
                not_enough(&unresolved(result, r)),
                context_sizes(report, result, r.profile),
                context_rot(report, result)
            )
        };
        row.evidence = match result {
            CheckResult::Effort(e) => effort_evidence(e, mark, out),
            CheckResult::Ttl(t) => match mark {
                Mark::Unknown => unknown(),
                Mark::Keep if t.one_hour_savings_usd == 0.0 => {
                    format!("5m and 1h cost the same on replay. {set_in}")
                }
                _ => {
                    let other = if r.current == Some(Setting::Ttl(Ttl::OneHour)) {
                        "5m"
                    } else {
                        "1h"
                    };
                    let better = if mark == Mark::Keep {
                        format!("{} costs less than {other}", setting_value(r.current))
                    } else if unset {
                        let gap = approx(
                            monthly(t.one_hour_savings_usd.abs(), r.since, r.until),
                            check_margin(result, r),
                            "",
                        );
                        let winner = setting_value(r.recommended);
                        format!(
                            "{winner} costs {gap} less every 30 days, saved whenever the unset default would be {}, and setting it keeps {winner} however usage is paid",
                            if winner == "1h" { "5m" } else { "1h" }
                        )
                    } else {
                        format!("{} costs less", setting_value(r.recommended))
                    };
                    format!(
                        "Replayed at 5m and 1h, assuming the same sessions{}; {better}. {set_in}",
                        coverage_note(r, MEASURED)
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
                if c.models.iter().flatten().count() > 1 {
                    let models: Vec<&str> = c.models.iter().flatten().map(|m| &**m).collect();
                    let _ = write!(text, "Covers {}. ", models.join(", "));
                }
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
                            "Replayed assuming the same sessions{}{net}.{} {}{} {set_in}",
                            coverage_note(r, MEASURED),
                            assumed_tier(r),
                            if smaller {
                                "Detail lost to summaries is not measured."
                            } else {
                                "How work changes at longer contexts is not measured."
                            },
                            context_rot(report, result)
                        )
                    }
                    Mark::Keep => format!(
                        "No other candidate window was cheaper, replayed assuming the same sessions.{}{} {set_in}",
                        assumed_tier(r),
                        context_rot(report, result)
                    ),
                    _ => unknown(),
                };
                text
            }
            CheckResult::TurnEnd(c) => {
                let after = match c.window {
                    Some(window) if c.window_offered => format!(
                        " under the {} auto-compact limit the Auto-compact window line recommends",
                        tokens(window)
                    ),
                    Some(window) => format!(" under the {} auto-compact limit", tokens(window)),
                    None => String::new(),
                };
                // The two edits are offered apart, so say what this one's figure rests on.
                let assumes = if c.window_offered {
                    " The saving assumes that limit is set too."
                } else {
                    ""
                };
                match mark {
                    Mark::Change => {
                        let added = c
                            .scenarios
                            .iter()
                            .find(|s| r.recommended == Some(Setting::Percent(s.window)))
                            .map_or(0, |s| s.added.len());
                        format!(
                            "Replayed{after}, assuming the same sessions{}, net of {} added summar{}.{}{assumes} Detail lost to summaries is not measured. {set_in}",
                            coverage_note(r, MEASURED),
                            count(added as u64),
                            if added == 1 { "y" } else { "ies" },
                            assumed_tier(r)
                        )
                    }
                    Mark::Keep => format!(
                        "No other threshold was cheaper, replayed{after} assuming the same sessions.{} {set_in}",
                        assumed_tier(r)
                    ),
                    _ => unknown(),
                }
            }
            CheckResult::Idle(_) => {
                let on = |s: Option<Setting<'_>>| s == Some(Setting::Idle(true));
                let loss = " A summary drops detail, which is not measured.";
                match mark {
                    Mark::Change => format!(
                        "Replayed on and off, assuming the same sessions{}.{} {set_in}",
                        coverage_note(r, PRICED_SESSIONS),
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
                    coverage_note(r, PRICED_SESSIONS),
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
                let mut text = match b.directories.len() {
                    0 => String::new(),
                    n => {
                        // A feature that ran in many directories names a few, not every worktree.
                        let shown: Vec<String> = b
                            .directories
                            .iter()
                            .take(BACKGROUND_DIRECTORIES)
                            .map(|d| {
                                d.as_ref()
                                    .map_or_else(|| "directory absent".into(), |d| out.home(d))
                            })
                            .collect();
                        let more = n - shown.len();
                        if more > 0 {
                            format!(
                                "In {} directories, such as {}. ",
                                count(n as u64),
                                shown.join(", ")
                            )
                        } else {
                            format!("In {}. ", shown.join(", "))
                        }
                    }
                };
                let off = format!("{official}{}", background_off(b, out));
                text += &match mark {
                    Mark::Change => format!(
                        "Estimated where it would have run{}; its uses are not recorded. {off}.",
                        coverage_note(r, MEASURED)
                    ),
                    Mark::Consider => format!(
                        "Estimated where it would have run{}; its uses are not recorded, so it may still cost less than {} per use. {off} to turn it off.",
                        coverage_note(r, MEASURED),
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
                        coverage_note(r, MEASURED),
                        about(report.max_cost_per_use_usd)
                    ),
                    _ if r.current.is_none() => format!(
                        "{} {} Candidate owner: {}. {official}Verify ownership before using: {}.",
                        not_enough(background_unknown(b)),
                        b.reason,
                        out.home(r.owner),
                        background_off(b, out)
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
                let mut notes = vec![];
                match r.coverage {
                    Some(c) if r.savings_usd.is_some() && c.priced == 0 => notes.push(
                        "from memory's reads and writes alone; no request recorded its context"
                            .to_string(),
                    ),
                    Some(c) if r.savings_usd.is_some() => notes.push(format!(
                        "extrapolated from the {}% of requests that recorded their context",
                        percent(c.priced, c.total)
                    )),
                    _ => {}
                }
                if m.unpriced_reads > 0 && r.savings_usd.is_some() {
                    notes.push(format!(
                        "memory read back by {} that recorded no context could not be priced",
                        plural(m.unpriced_reads, "request")
                    ));
                }
                if unmeasured {
                    notes.push(if r.savings_usd == Some(0.0) && r.coverage.is_none() {
                        "no measured cost, but extraction and consolidation are not measured".into()
                    } else {
                        "extraction and consolidation are not measured".into()
                    });
                }
                let measured = if mark == Mark::Keep && memory_free(m) && !m.lower_bound() {
                    "costs nothing".to_string()
                } else {
                    memory_uses(m)
                };
                notes.insert(0, measured);
                let mut text = format!("{} Measured: {}.", memory_reason(m, r), notes.join("; "));
                if m.unresolved.is_some() {
                    row.value = "may be on".into();
                }
                if mark != Mark::Keep {
                    let _ = write!(text, " {}.", memory_off(m, out));
                }
                text
            }
            CheckResult::Speed(_) => {
                let measured = match mark {
                    Mark::Consider => {
                        "The premium is measured at API rates for the same work; the speed it buys is not valued, so turning it off is only worth considering.".to_string()
                    }
                    Mark::Keep if r.current == Some(Setting::Speed("standard")) => String::new(),
                    Mark::Keep => "Its premium over the recorded usage came to nothing.".into(),
                    _ => not_enough(&unresolved(result, r)),
                };
                [measured, set_in]
                    .into_iter()
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        };
        // A line ends with the setting that changes it, so the plan's rule goes before that
        // where the line names one.
        if !official.is_empty() && !row.evidence.contains(&official) {
            let _ = write!(row.evidence, " {}", official.trim_end());
        }
        // A change shows the value it changes to, as `doctor` offers it.
        if row.offered() {
            row.value = format!(
                "{} {arrow} {}",
                row.value,
                check_target(result, r, row.mark)
            );
        }
        // The figure: what a change saves, else what the setting cost or saves as it is.
        if row.offered() {
            row.saves(
                check_verb(result, r, out),
                check_saving(result, r, out),
                check_margin(result, r),
                &spend,
            );
        } else {
            match result {
                CheckResult::Effort(e) if !e.runs.is_empty() => {
                    row.costs(e.estimate, false, &spend);
                }
                CheckResult::Ttl(t) if mark == Mark::Keep && t.one_hour_savings_usd != 0.0 => {
                    row.saves(
                        "saves",
                        Some(monthly(t.one_hour_savings_usd.abs(), r.since, r.until)),
                        check_margin(result, r),
                        &spend,
                    );
                }
                CheckResult::Background(_) if r.current == Some(Setting::Toggle(true)) => {
                    // A feature that is on says plainly when its cost could not be measured.
                    match r.savings_usd {
                        Some(usd) if usd <= 0.0 => {}
                        usd => row.costs(check_estimate(usd, r), false, &spend),
                    }
                }
                // Codex extraction and consolidation, or reads nothing priced, are left out.
                CheckResult::Memory(m) => match r.savings_usd {
                    Some(0.0) => {}
                    usd => row.costs(check_estimate(usd, r), m.lower_bound(), &spend),
                },
                _ => {}
            }
        }
        row.setting = match offer(Origin::Check(*index), out) {
            Some(setting) if row.offered() => setting,
            // Why nothing can be set is evidence, under details.
            _ if r.owner.starts_with("no setting") => "no setting".into(),
            _ => out.home(r.owner),
        };
        row
    };
    let piece_row = |(index, c): &(usize, &ContextCost), out: &Out<'_>| {
        let typed = typed_only(c.control.as_ref());
        let verdict = c.verdict.expect("pieces have verdicts");
        let mark = match verdict {
            Verdict::Off => Mark::Change,
            Verdict::Consider => Mark::Consider,
            Verdict::Keep => Mark::Keep,
        };
        let mut row = Row::new(mark, short_label(c, out), piece_what(c));
        row.origin = Some(Origin::Context(*index));
        row.profile = tag(&c.profiles, &switch, out);
        row.location = skill_location(c, out);
        row.value = match verdict {
            Verdict::Off | Verdict::Consider if typed => format!("on {arrow} typed only"),
            Verdict::Off | Verdict::Consider => format!("on {arrow} off"),
            Verdict::Keep => "on".into(),
        };
        if row.offered() && c.skill.is_none() {
            row.saves(
                "saves",
                piece_saving(c, &spend),
                piece_margin(c, &spend),
                &spend,
            );
        } else {
            row.costs(c.estimate(), false, &spend);
        }
        // A share of the figure beside it, which carries the line's ~ and margin. One use's
        // share is that figure, so it is left out.
        let each = c
            .cost_per_use_usd
            .filter(|_| c.calls != Some(1))
            .map_or_else(String::new, |usd| format!(", {} each", about(usd)));
        let mut text = format!(
            "{}{each}; it cost {} over the window",
            capitalize(&uses(c)),
            figure(c.estimate())
        );
        if c.skill.is_some() {
            let _ = write!(text, "; {}", catalog_evidence(c));
        }
        text.push('.');
        text += &pretended_note(c);
        // What leaves nonuse unproven outside the Codex catalog.
        if verdict == Verdict::Consider && c.skill.is_none() && c.calls == Some(0) {
            text += match hidden_use(
                c,
                &report.scan.snapshots,
                &report.scan.exposure,
                &report.scan.profiles,
            ) {
                Some(Hidden::Named) => {
                    " An unreadable transcript line names it, so it may have been used."
                }
                Some(Hidden::Cut) => {
                    " A cut transcript record could hold a use: a tool result there has no recorded call."
                }
                Some(Hidden::Unfinished) => {
                    " A transcript's unfinished last line could hold a use."
                }
                None => {
                    " It was first sent during the window, so the window cannot rule out a use."
                }
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
        row.setting = match offer(Origin::Context(*index), out) {
            Some(setting) if row.offered() => setting,
            _ => piece_setting(c, out),
        };
        row
    };
    let mut rows: Vec<Row> = checks.iter().map(|c| check_row(c, out)).collect();
    // Advice the transcripts cannot price, which `vibes` shows instead of this report.
    let mut vibes: Vec<Item> = vec![];
    for cap in &report.bash {
        let how = report
            .scan
            .profiles
            .iter()
            .find(|p| p.id == cap.profile)
            .and_then(|profile| crate::doctor::bash_how(cap, profile));
        let profile = tag(&cap.profiles, &|_| cap.owner.clone(), out);
        vibes.push(bash_item(cap, &profile, how, out));
    }
    let owned = |p: &Profile| {
        p.retention
            .as_ref()
            .map_or_else(String::new, |r| r.owner.clone())
    };
    let retained = report.scan.profiles.iter().filter(|p| retention_settled(p));
    for line in crate::rows::groups(retained, owned) {
        let control = line
            .lead
            .retention
            .as_ref()
            .expect("filtered on a retention control");
        let Some(days) = control.days.filter(|days| *days < RETENTION_DAYS) else {
            continue;
        };
        let disk = report
            .scan
            .snapshots
            .iter()
            .filter(|s| s.harness == Harness::Claude && line.covers(&s.profiles))
            .map(|s| s.bytes)
            .sum();
        let how = crate::doctor::retention_how(control, RETENTION_DAYS, line.lead);
        let profile = tag(&line.ids, &owned, out);
        vibes.push(retention_item(control, days, &profile, disk, how, out));
    }
    for (index, result, r) in &checks {
        if !matches!(result, CheckResult::Memory(_)) {
            continue;
        }
        let offered = fixes
            .iter()
            .find(|f| f.origins.contains(&Origin::Check(*index)))
            .map(|f| Offered {
                suggested: f.suggested,
                asked: crate::doctor::takes_up(f),
            });
        let mut item = memory_item(
            MemoryLine {
                result,
                common: r,
                name: tagged(
                    setting_name(result),
                    &tag(r.profiles, &|_| r.owner.to_string(), out),
                ),
                offered,
            },
            out,
        );
        if !item.quiet {
            item.how = report
                .scan
                .profiles
                .iter()
                .find(|p| p.id == r.profile)
                .map(|profile| crate::doctor::check_how(result, profile));
        }
        vibes.push(item);
    }
    for (index, result, r) in &checks {
        if let CheckResult::Compaction(c) = result
            && let Some(profile) = out.profiles.get(r.profile).copied()
        {
            // The lines whose change the walk can write, by the window it moves to.
            let writable = |i: usize| {
                fixes
                    .iter()
                    .any(|f| f.origins.contains(&Origin::Check(i)) && matches!(f.how, How::Edit(_)))
            };
            let doctor = match check_mark(result, r) {
                Mark::Change => c
                    .base
                    .recommended
                    .map_or(Doctor::Unknown, Doctor::Recommends),
                Mark::Keep => Doctor::Keeps,
                _ => Doctor::Unknown,
            };
            let named = |mut item: Item| {
                item.name = tagged(
                    setting_name(result),
                    &tag(r.profiles, &|_| r.owner.to_string(), out),
                );
                item.what = crate::descriptions::check(result).into();
                item
            };
            let mut item = named(window_item(c, profile, doctor, out));
            // Another window line's change on this harness can move this window, as can this
            // line's own change when it pins other models' triggers: the scan can't replay
            // either, so the card asks for a fresh one.
            let others: Vec<Origin> = report
                .checks
                .iter()
                .enumerate()
                .filter(|(i, other)| {
                    *i != *index
                        && writable(*i)
                        && matches!(other, CheckResult::Compaction(o)
                            if out.profiles.get(o.base.profile.as_str())
                                .is_some_and(|p| p.harness == profile.harness))
                })
                .map(|(i, _)| Origin::Check(i))
                .collect();
            if !others.is_empty() {
                item.after.push(crate::vibes::After {
                    origins: others,
                    item: Box::new(named(window_rescan(c, profile, out))),
                });
            }
            if let (true, Doctor::Recommends(to)) = (writable(*index), doctor) {
                let written = match c.reowned {
                    Some(_) => window_rescan(c, profile, out),
                    None => window_item(c, profile, Doctor::Wrote(to), out),
                };
                item.after.push(crate::vibes::After {
                    origins: vec![Origin::Check(*index)],
                    item: Box::new(named(written)),
                });
            }
            vibes.push(item);
        }
        if let CheckResult::Effort(e) = result
            && e.base.action == Action::Keep
        {
            let how = report
                .scan
                .profiles
                .iter()
                .find(|p| p.id == e.base.profile)
                .and_then(|profile| crate::doctor::default_how(&e.control, profile));
            let profile = tag(r.profiles, &|_| r.owner.to_string(), out);
            vibes.extend(default_item(e, &profile, how, out));
        }
    }
    let subagent_owner = |p: &Profile| {
        p.agents
            .effort
            .as_ref()
            .map_or_else(String::new, |e| e.owner.clone())
    };
    let set = report
        .scan
        .profiles
        .iter()
        .filter(|p| p.agents.effort.is_some());
    for group in crate::rows::groups(set, subagent_owner) {
        // Profiles sharing the effort setting can pair it with different subagent models, or
        // with settings token-max can't read, which each card has to say.
        let mut by_model: IndexMap<(Option<&str>, Option<&str>), Vec<&Profile>> =
            IndexMap::default();
        for p in &group.members {
            let model = p.agents.model.as_ref().map(|m| m.value.as_str());
            let unresolved = p.agents.unresolved.as_deref();
            by_model.entry((model, unresolved)).or_default().push(p);
        }
        // Lines split by model share a file, so they name their profiles instead.
        let split = by_model.len() > 1;
        for members in by_model.into_values() {
            let ids: Vec<String> = members.iter().map(|p| p.id.clone()).collect();
            let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            let profile = if split {
                tag(&ids, &|_| String::new(), out)
            } else {
                tag(&ids, &subagent_owner, out)
            };
            let line = SubagentLine {
                lead: members[0],
                profile,
                versions: refs
                    .iter()
                    .filter_map(|id| {
                        let version = crate::doctor::claude_versions(report, &[id]).pop()?;
                        // One profile's card names no profile.
                        let name = match members.len() {
                            1 => String::new(),
                            _ => tags
                                .get(id)
                                .cloned()
                                .unwrap_or_else(|| out.short_profile(id)),
                        };
                        Some((name, version))
                    })
                    .collect(),
            };
            vibes.extend(subagent_item(&line, out));
        }
    }
    // Lines of one setting that split only because the controls they replay differ name the
    // same file; name their profiles as well, so no two read alike.
    let clashing: Vec<usize> = (0..rows.len())
        .filter(|&i| {
            !rows[i].profile.is_empty()
                && (0..rows.len()).any(|j| {
                    rows[i].name == rows[j].name
                        && rows[i].profile == rows[j].profile
                        && checks[i].2.profiles != checks[j].2.profiles
                })
        })
        .collect();
    for i in clashing {
        let names: Vec<String> = checks[i]
            .2
            .profiles
            .iter()
            .map(|id| {
                tags.get(id.as_str())
                    .cloned()
                    .unwrap_or_else(|| out.short_profile(id))
            })
            .collect();
        let row = &mut rows[i];
        row.profile = format!(
            "{}: {}]",
            row.profile.trim_end_matches(']'),
            names.join(", ")
        );
    }
    rows.extend(pieces.iter().map(|c| piece_row(c, out)));
    // Switchable context still sent today without enough pricing or use evidence for a
    // verdict. Auto memory has its own line. A tool its server's switch covers, or one with
    // no switch of its own, is part of its server's line.
    for c in setting_rows(&report.context).filter(|c| {
        c.current
            && c.control.is_some()
            && c.verdict.is_none()
            && !applied(c)
            && !matches!(c.kind, ContextKind::Memory | ContextKind::McpDiscovery)
    }) {
        let mut row = Row::new(Mark::Unknown, short_label(c, out), piece_what(c));
        row.profile = tag(&c.profiles, &switch, out);
        row.location = skill_location(c, out);
        row.value = "on".into();
        if c.estimated_usd.is_some() {
            row.costs(c.estimate(), false, &spend);
        }
        let uses = uses(c);
        let mut text = not_enough(&match c.estimated_usd {
            None => format!("None of its requests could be priced; {uses}."),
            Some(_) if c.tier_unknown_requests > 0 => format!(
                "{} requests have unresolved tier evidence ({} priced using assumed tiers), so tier sensitivity could reverse keep or off; {uses}.",
                c.tier_unknown_requests, c.conditional_requests
            ),
            Some(_) => "Its recorded uses are unknown, so keep or off is unresolved.".into(),
        });
        text += &pretended_note(c);
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
        row.setting = piece_setting(c, out);
        rows.push(row);
    }
    // Tools, skills and MCP servers a settings file turns off, which are on by default. No
    // request records one once it is off, so what it saves is estimated from what it cost
    // while it was still sent, or unknown.
    let real = |file: &str| crate::discovery::realpath(file).unwrap_or_else(|_| file.into());
    let mut seen_off: Vec<(String, &crate::model::Control)> = vec![];
    for p in &report.scan.profiles {
        let file = real(&p.settings_file);
        for control in &p.switched_off {
            if seen_off
                .iter()
                .any(|(f, c)| *f == file && crate::defaults::same_switch(c, control))
            {
                continue;
            }
            seen_off.push((file.clone(), control));
            let group: Vec<&Profile> = report
                .scan
                .profiles
                .iter()
                .filter(|q| q.harness == p.harness && real(&q.settings_file) == file)
                .collect();
            let ids: Vec<String> = group.iter().map(|q| q.id.clone()).collect();
            let piece = setting_rows(&report.context).find(|c| {
                c.harness == p.harness
                    && c.profiles.iter().any(|id| ids.contains(id))
                    && c.control
                        .as_ref()
                        .is_some_and(|k| crate::defaults::same_switch(k, control))
            });
            // A file that outranks this one for any profile sharing it may turn it back on,
            // and a feature still sent though no request predates the change has something
            // outranking the file; either way the feature's own line, if any, says what it
            // costs.
            if group.iter().any(|q| crate::doctor::outranked(q, control))
                || piece.is_some_and(|c| c.current && !applied(c))
            {
                continue;
            }
            let shown = out.home(&p.settings_file);
            let requests = report
                .scan
                .requests
                .iter()
                .filter(|r| r.harness == p.harness && r.profiles.iter().any(|id| ids.contains(id)))
                .count() as u64;
            let (name, what, saving, margin, evidence) = match piece {
                Some(c) => {
                    // A Codex catalog skill's cost is no saving: removing it can backfill the catalog.
                    let estimate = crate::defaults::estimate(
                        c.estimated_usd.filter(|_| c.skill.is_none()),
                        c.requests,
                        requests,
                        spend.since,
                        Some(spend.until),
                    );
                    let saving = estimate.map_or(crate::defaults::Saving::NotMeasured, |usd| {
                        crate::defaults::Saving::Estimated(usd)
                    });
                    // Scaled as the saving is, from the requests that carried it to 30 days.
                    let margin = c.margin.filter(|_| estimate.is_some()).map(|m| {
                        m.scale(
                            requests as f64 / c.requests as f64
                                * monthly(1.0, spend.since, Some(spend.until)),
                        )
                    });
                    let evidence = match estimate {
                        Some(_) => format!(
                            "Off in {shown}. While still sent it cost {} on {} requests; the estimate assumes each of this line's {} requests in the window would have carried it.",
                            figure(c.estimate()),
                            count(c.requests),
                            count(requests)
                        ),
                        _ => format!(
                            "Off in {shown}. What it cost while still sent can't stand for what it saves, so the saving is unknown."
                        ),
                    };
                    (short_label(c, out), piece_what(c), saving, margin, evidence)
                }
                None => (
                    crate::defaults::switch_name(control),
                    crate::defaults::switch_what(control),
                    crate::defaults::Saving::NotMeasured,
                    None,
                    format!(
                        "Off in {shown}. No request in the window carried it, so what it saves is unknown."
                    ),
                ),
            };
            let mut row = Row::new(Mark::Info, name, what);
            row.profile = tag(&ids, &switch, out);
            row.value = crate::defaults::switch_value(control);
            row.setting = format!("{shown}:{}", control.path.join("."));
            row.evidence = evidence;
            row.baseline = Some(crate::defaults::Baseline {
                default: "on".into(),
                saving,
                margin,
            });
            rows.push(row);
        }
    }
    // Under --defaults, a feature a settings file turns off that no request in the window
    // carried: on, at a cost no transcript shows. An allowlist names no tool it leaves out.
    let mut seen_on: Vec<(String, &crate::model::Control)> = vec![];
    for p in &report.scan.profiles {
        for (file, control) in &p.pretended_on {
            // A server recommended off hides its tools' lines, which still say what was sent.
            let carried = report
                .context
                .iter()
                .flat_map(|c| std::iter::once(c).chain(&c.details))
                .any(|c| {
                    c.harness == p.harness
                        && c.profiles.contains(&p.id)
                        && c.control
                            .as_ref()
                            .is_some_and(|k| crate::defaults::covers(control, k))
                });
            let real_file = real(file);
            if carried
                || control.path.last().is_some_and(|k| k == "enabled_tools")
                || seen_on
                    .iter()
                    .any(|(f, c)| *f == real_file && crate::defaults::same_switch(c, control))
            {
                continue;
            }
            seen_on.push((real_file.clone(), control));
            let ids: Vec<String> = report
                .scan
                .profiles
                .iter()
                .filter(|q| {
                    q.harness == p.harness
                        && q.pretended_on.iter().any(|(f, k)| {
                            real(f) == real_file && crate::defaults::same_switch(k, control)
                        })
                })
                .map(|q| q.id.clone())
                .collect();
            let shown = out.home(file);
            let mut row = Row::new(
                Mark::Unknown,
                crate::defaults::switch_name(control),
                crate::defaults::switch_what(control),
            );
            row.profile = tag(&ids, &switch, out);
            row.value = "on".into();
            row.costs(None, false, &spend);
            row.setting = format!("{shown}:{}", control.path.join("."));
            row.evidence = format!(
                "{shown} turns it off, so no request in the window carried it, and what it costs on is unknown."
            );
            rows.push(row);
        }
    }
    // Under --defaults, what managed or system policy turns off stays off, and a user can't
    // turn it on, so its line says so and recommends nothing.
    let mut seen_policy: Vec<&crate::model::Control> = vec![];
    for p in &report.scan.profiles {
        for (file, control) in &p.policy_off {
            if seen_policy
                .iter()
                .any(|c| crate::defaults::same_switch(c, control))
            {
                continue;
            }
            seen_policy.push(control);
            let ids: Vec<String> = report
                .scan
                .profiles
                .iter()
                .filter(|q| {
                    q.harness == p.harness
                        && q.policy_off
                            .iter()
                            .any(|(_, k)| crate::defaults::same_switch(k, control))
                })
                .map(|q| q.id.clone())
                .collect();
            let shown = out.home(file);
            let allowlist = control.path.last().is_some_and(|k| k == "enabled_tools");
            let mut row = Row::new(
                Mark::Info,
                crate::defaults::switch_name(control),
                crate::defaults::switch_what(control),
            );
            row.profile = tag(&ids, &switch, out);
            row.value = if allowlist {
                "listed tools only".into()
            } else {
                crate::defaults::switch_value(control)
            };
            row.setting = format!("{shown}:{}", control.path.join("."));
            row.evidence = if allowlist {
                format!(
                    "Policy in {shown} keeps every tool it doesn't list off, which a user can't change."
                )
            } else {
                format!("Policy in {shown} keeps it off, which a user can't change.")
            };
            rows.push(row);
        }
    }
    for g in report
        .scan
        .t3
        .iter()
        .filter_map(|s| s.text_generation.as_ref())
    {
        let mut row = Row::new(
            Mark::Unknown,
            "T3 text generation".into(),
            "The model T3 Code uses to write thread titles, branch names, PR text and other generated text.".into(),
        );
        row.value = format!("{} {}", g.selection.instance, g.selection.model);
        let mut text = format!(
            "{} ({}). Set in {}.",
            not_enough(
                "T3 writes thread titles, branch names, PR text and other generated text in ephemeral sessions that keep no native records"
            ),
            g.resolution,
            out.home(&g.owner)
        );
        text.push_str(&t3_overrides(g));
        row.evidence = text;
        row.setting = out.home(&g.owner);
        rows.push(row);
    }
    let instructions = setting_rows(&report.context)
        .enumerate()
        .filter(|(_, c)| c.current && c.kind == ContextKind::Instructions);
    for (index, c) in instructions {
        let mut row = instruction_row(c, out);
        row.origin = Some(Origin::Context(index));
        row.profile = tag(&c.profiles, &switch, out);
        row.costs(c.estimate(), false, &spend);
        rows.push(row);
        vibes.extend(instruction_warning(
            c,
            &report.scan.profiles,
            &tag(&c.profiles, &switch, out),
            out,
        ));
    }
    let loading = mcp_guidance(report.context.iter().filter(|c| c.current));
    if !loading.is_empty() {
        let mut row = Row::new(
            Mark::Info,
            "MCP tool loading".into(),
            "Whether each MCP tool's full definition is sent with every request, or only its name until the model loads it.".into(),
        );
        let notes: IndexSet<&str> = loading.iter().flat_map(|g| g.notes.clone()).collect();
        row.evidence = notes.into_iter().collect::<Vec<_>>().join(" ");
        row.sources = loading.iter().filter_map(|g| g.source).collect();
        row.setting = loading
            .iter()
            .filter_map(|g| g.setting)
            .collect::<Vec<_>>()
            .join("; ");
        rows.push(row);
    }
    // Tool output, one line per cap setting, with tools no cap governs together.
    let mut outputs: IndexMap<(&[String], Option<Cap>), Vec<&ContextCost>> = IndexMap::default();
    for c in report
        .context
        .iter()
        .filter(|c| c.kind == ContextKind::ToolOutput)
    {
        outputs
            .entry((c.profiles.as_slice(), output_cap(report, c)))
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
        let (total, at_least, unpriced) = pieces_figure(group);
        let largest = group.iter().filter_map(|c| c.largest_characters).max();
        let largest = largest.map_or_else(String::new, |n| {
            format!("Its largest result was {} characters. ", count(n))
        });
        let mut row = match &key.1 {
            // A cap is a setting whose cost transcripts cannot weigh.
            Some(cap) => {
                let mut row = Row::new(Mark::Unknown, cap.label.into(), cap.what.into());
                row.value = cap.value.clone();
                // Transcripts can't price a different cap, so what this one saves is unknown.
                row.baseline = cap.default.map(|default| crate::defaults::Baseline {
                    default: default.into(),
                    saving: crate::defaults::Saving::NotMeasured,
                    margin: None,
                });
                row.setting = cap.key.into();
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
                let mut row = Row::new(
                    Mark::Info,
                    "Other tool output".into(),
                    "Tool results kept in the conversation from tools no checked cap limits."
                        .into(),
                );
                row.setting = "no cap is checked".into();
                row.evidence = format!(
                    "From {}{more}. {largest}{unpriced}No cap is checked for it.",
                    names.join(", ")
                );
                row
            }
        };
        row.costs(total, at_least, &spend);
        row.profile = tag(key.0, &switch, out);
        rows.push(row);
    }
    // One line for each subagent type: what it cost, with no verdict.
    for c in &report.subagents {
        let mut row = Row::new(
            Mark::Info,
            subagent_name(c),
            crate::descriptions::subagent(c).into(),
        );
        row.profile = tag(&c.profiles, &switch, out);
        row.costs(c.estimate, false, &spend);
        row.evidence = subagent_line(c, out);
        vibes.extend(explore_item(c, &row.profile, out));
        // The setting, without why it is the one.
        row.setting = out
            .home_in(&c.model_setting)
            .split("; ")
            .next()
            .unwrap_or_default()
            .to_string();
        rows.push(row);
    }
    for u in &report.lsp {
        let mut profile = tag(&u.profiles, &switch, out);
        // This card's settings file, also when named another way, such as through a link.
        let settings = crate::discovery::realpath(&u.settings_file).ok();
        let same = |file: &str| {
            file == u.settings_file
                || settings.is_some() && crate::discovery::realpath(file).ok() == settings
        };
        // Profiles that share a settings file but not a plugins directory get a card each.
        if report
            .lsp
            .iter()
            .any(|o| same(&o.settings_file) && o.plugins_dir != u.plugins_dir)
        {
            let dir = u
                .plugins_dir
                .as_deref()
                .map_or_else(|| "a secret directory".into(), |d| out.home_in(d));
            profile = tagged(profile, &format!("(plugins in {dir})"));
        }
        let view = crate::lsp::View {
            home: out.home.as_deref(),
            arrow: out.arrow,
        };
        let named = |i: Item| Item {
            name: tagged(i.name.clone(), &profile),
            ..i
        };
        // A `doctor` line denying the LSP tool in this card's settings leaves the plugin only
        // its diagnostics, so the card reads as denied once the walk writes it. The file may
        // be written through another name for it, such as a link.
        let denies: Vec<Origin> = fixes
            .iter()
            .filter(|f| {
                matches!(&f.how, How::Edit(p) if same(&p.file)
                    && p.edits.iter().any(|e| e.op == crate::doctor::Op::Append
                        && e.path == ["permissions", "deny"]
                        && e.value == "LSP"))
            })
            .flat_map(|f| f.origins.iter().copied())
            .collect();
        let denied = (u.denied.is_none() && !denies.is_empty()).then(|| {
            crate::lsp::items(
                &crate::lsp::LspUse {
                    denied: Some(u.settings_file.clone()),
                    ..u.clone()
                },
                &view,
            )
        });
        let cards = crate::lsp::items(u, &view);
        for (i, card) in cards.into_iter().enumerate() {
            let mut card = named(card);
            if let Some(after) = denied.as_ref().and_then(|d| d.get(i)) {
                card.after.push(crate::vibes::After {
                    origins: denies.clone(),
                    item: Box::new(named(after.clone())),
                });
            }
            vibes.push(card);
        }
    }
    for u in &report.inbound {
        let mut row = inbound_row(u, out);
        let owner = |p: &Profile| {
            p.inbound
                .as_ref()
                .map_or_else(String::new, |c| format!("{}:crossSessionInbound", c.file))
        };
        row.profile = tag(&u.profiles, &owner, out);
        if u.idle > 0 {
            row.costs(u.estimate, false, &spend);
        }
        rows.push(row);
    }
    for u in &report.fable {
        let mut row = fable_row(u);
        row.profile = tag(&u.profiles, &switch, out);
        if u.requests > 0 {
            row.costs(u.estimate, false, &spend);
        }
        row.setting = FABLE_MODEL.into();
        if u.requests > 0 {
            // One total over every week, so name the span it covers.
            let start = u.weeks.iter().map(|w| w.start).min().unwrap_or_default();
            let end = u.weeks.iter().map(|w| w.end).max().unwrap_or_default();
            vibes.push(guidance(
                row.name.clone(),
                &row.profile,
                format!(
                    "Cost {} at API prices over {} from {} to {}. {FABLE_PARTIAL}{} {}",
                    figure(u.estimate),
                    plural(u.requests, "Fable request"),
                    date(&iso(start)),
                    date(&iso(end)),
                    fable_caveats(u),
                    capitalize(FABLE_RULE)
                ),
                &[FABLE_PLANS],
            ));
        }
        rows.push(row);
    }
    let advisor_owner = |p: &Profile| {
        p.advisor
            .as_ref()
            .map_or_else(String::new, |a| a.owner.clone())
    };
    for u in &report.advisor {
        let row = match &u.line {
            AdvisorLine::Settings => {
                let control = out
                    .profiles
                    .get(u.profiles[0].as_str())
                    .and_then(|p| p.advisor.as_ref())
                    .expect("a settings line's lead has its control");
                if control.model.is_none() && u.calls == 0 && u.shared == 0 {
                    continue;
                }
                let mut row = advisor_row(u, Some(control), out);
                row.profile = tag(&u.profiles, &advisor_owner, out);
                row
            }
            AdvisorLine::T3 { .. } | AdvisorLine::Removed | AdvisorLine::Shared { .. } => {
                let mut row = advisor_row(u, None, out);
                row.profile = tag(&u.profiles, &switch, out);
                row
            }
        };
        let mut row = row;
        if u.calls > 0 {
            row.costs(u.estimate, false, &spend);
        }
        rows.push(row);
    }
    let advised = report.scan.profiles.iter().filter(|p| p.advisor.is_some());
    for card in crate::rows::groups(advised, |p| {
        crate::advisor::owner(p).expect("filtered on an advisor")
    }) {
        let profile = tag(&card.ids, &advisor_owner, out);
        let calls = report
            .advisor
            .iter()
            .find(|u| u.line == AdvisorLine::Settings && u.profiles == card.ids);
        vibes.push(advisor_item(calls, card.lead, &profile, out));
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
        let mut row = Row::new(Mark::Info, short_label(c, out), piece_what(c));
        row.profile = tag(&c.profiles, &switch, out);
        row.location = skill_location(c, out);
        row.costs(c.estimate(), false, &spend);
        row.setting = piece_setting(c, out);
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
    let core: Vec<&ContextCost> = fixed
        .iter()
        .filter(|c| !matches!(c.kind, ContextKind::Skill | ContextKind::McpServer))
        .copied()
        .collect();
    if !core.is_empty() {
        let mut row = Row::new(
            Mark::Info,
            "System prompt and core tools".into(),
            "The harness's own instructions and the tools it cannot work without, such as Bash and Read, sent with every request.".into(),
        );
        let (total, at_least, unpriced) = pieces_figure(&core);
        row.costs(total, at_least, &spend);
        row.setting = "no setting".into();
        row.evidence = format!(
            "{unpriced}Includes core tools such as Bash and Read, and the skill and subagent lists. No setting turns these off."
        );
        rows.push(row);
    }

    out.blank();
    let changes = rows.iter().filter(|r| r.mark == Mark::Change).count();
    let considered = rows.iter().filter(|r| r.mark == Mark::Consider).count();
    let headline = headline(changes, considered);
    let headline: Vec<(&str, &str)> = headline.iter().map(|(t, s)| (t.as_str(), *s)).collect();
    out.spans(&headline, 0, None);
    // In a report of several hosts, each section's percents are of its own host's spend.
    let host = match out.options.host {
        Some(host) if host.name != "this host" => format!("{}'s", host.name),
        _ => "this host's".into(),
    };
    // Every setting is at its default already, so what the changes save from there replaces
    // the comparison with the defaults.
    let together = crate::doctor::total(fixes, report.scan.defaults);
    if !together.is_empty() {
        out.line(&together, 0, PLAIN);
    }
    let credit = (!report.scan.defaults)
        .then(|| crate::defaults::Credit::of(rows.iter().filter_map(|r| r.baseline.as_ref())));
    if let Some(credit) = &credit {
        out.line(&credit.text(spend.monthly(), &host), 0, PLAIN);
    }
    out.blank();
    // With a profile that priced nothing, the base is only what could be priced, so a share can read high.
    let base = if !spend.partial && spend.margin.is_some() {
        format!(
            "{host} spend over the window, {spent}, extrapolated from the requests that could be priced"
        )
    } else if !spend.partial {
        format!("{host} spend over the window, {spent}")
    } else {
        format!(
            "{host} spend over the window, {spent}, since a profile that priced none of its requests adds only what could be priced, so a line's true share may be smaller"
        )
    };
    // Said once, for every figure extrapolated over requests that could not be priced.
    // The details also print every compaction window's estimate, the cap's included.
    let windows = out.options.details
        && report.checks.iter().any(|c| {
            matches!(c, CheckResult::Compaction(c)
                if c.scenarios.iter().chain(&c.cap).any(|s| s.margin.is_some()))
        });
    let tilde = if spend.margin.is_some() || rows.iter().any(|r| r.margin.is_some()) || windows {
        format!(" {TILDE}")
    } else {
        String::new()
    };
    out.line(
        &format!(
            "Savings are per 30 days at the window's rate, and costs are totals over the window; each percent is of {base}.{tilde} A tool, skill, MCP server or auto memory that was never used is recommended off, and one that cost more than {} per use (--max-cost-per-use) is worth considering off.",
            about(report.max_cost_per_use_usd)
        ),
        0,
        DIM,
    );
    if report
        .checks
        .iter()
        .any(|c| matches!(c, CheckResult::Background(_)))
    {
        out.line("Background estimates apply current files to history; historical settings and unrecorded launch overrides remain unknown.", 0, DIM);
    }
    if out.options.color {
        out.legend();
    }
    let head = std::mem::take(&mut out.text);
    if out.options.details {
        if !report.subagents.is_empty() {
            out.blank();
            out.line(SUBAGENT_EVIDENCE, 0, PLAIN);
        }
        catalog_guidance(report, out);
    }
    if spend.since.is_some_and(|s| spend.until > s) && !pieces.is_empty() {
        out.line(
            "A feature's saving is what it cost, assuming nothing else changes when it is removed; cache writes and compaction can shift.",
            0,
            DIM,
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
        DIM,
    );
    out.line(
        "Each line compares one setting with everything else unchanged, so a sum of savings may count the same requests twice.",
        0,
        DIM,
    );
    if !out.options.details {
        out.line(
            "--details shows the evidence, assumptions and sources behind each line.",
            0,
            DIM,
        );
    }
    (title, head, rows, vibes, credit)
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
        // Unpriced requests are extrapolated from the priced ones, never counted as free.
        let cost = match c.estimate() {
            None => "cost unknown".to_string(),
            Some(Estimate { usd, margin }) => format!(
                "{}{}{}",
                approx(usd, margin, ""),
                span.map_or_else(String::new, |span| {
                    let scale = 30.0 * DAY / span;
                    match margin {
                        None => format!(" (about {} per 30 days)", about(usd * scale)),
                        Some(m) => format!(
                            " ({} per 30 days)",
                            approx(usd * scale, Some(m.scale(scale)), "")
                        ),
                    }
                }),
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
                "; {} in the window, the largest {} characters",
                plural(results, "result"),
                count(c.largest_characters.unwrap_or(0))
            ),
            _ if c.skill.is_some() => format!(
                "; {} observed skill turns; {}",
                count(c.calls.unwrap_or(0)),
                catalog_evidence(c)
            ),
            Some(0) => "; no recorded use".into(),
            Some(calls) => format!(
                "; used {}, last {}",
                plural(calls, "time"),
                date(&iso(c.last_used.unwrap_or(0)))
            ),
        };
        let profile = if many {
            format!(" [{}]", out.switch_files(&c.profiles))
        } else {
            String::new()
        };
        let text = format!("{}{profile}: {cost}{used}", piece_label(c, out));
        let tier = match crate::defaults::applied(report, c) {
            Some(_) => PLAIN,
            None => feature_tier(c, covered),
        };
        out.line(&text, 2, tier);
        // A line several sources share is broken down by the profiles that recorded it.
        if c.shares.len() > 1 {
            for share in &c.shares {
                let priced = if share.sample.priced() == 0 {
                    "none priced".to_string()
                } else {
                    format!(
                        "{} over {} priced",
                        about(share.sample.sum().to_f64()),
                        count(share.sample.priced())
                    )
                };
                out.line(
                    &format!(
                        "{}: {} requests, {priced}",
                        out.profile_names(&share.profiles),
                        count(share.requests)
                    ),
                    6,
                    PLAIN,
                );
            }
        }
        for note in context_guidance(c, &report.scan.profiles, &out.home(&c.name)) {
            out.line(&note, 6, PLAIN);
        }
        out.notes("Pricing assumption", &c.assumptions);
        // A red verdict accepts one way its nonuse stays unproven beside unreadable lines.
        if !covered && c.verdict == Some(Verdict::Off) && c.skill.is_none() && c.calls == Some(0) {
            let lines = unreadable_lines(c, &report.scan.snapshots, &report.scan.profiles);
            if lines > 0 {
                let (noun, verb) = if lines == 1 {
                    ("line", "does not")
                } else {
                    ("lines", "do not")
                };
                out.line(
                    &format!(
                        "{} unreadable transcript {noun} {verb} name it or leave a tool result without a call; a call cut off before its result was written would not show.",
                        count(lines)
                    ),
                    6,
                    PLAIN,
                );
            }
        }
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
            _ if crate::defaults::applied(report, c).is_some() => {
                "Already off in its settings file, since the latest request that carried it"
                    .into()
            }
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

    #[test]
    fn retention_is_advised_only_where_its_settings_are_the_ones_in_force() {
        let mut native = make_profile("native", ROOT);
        assert!(!retention_settled(&native));
        native.retention = Some(RetentionControl {
            days: Some(30),
            owner: format!("{ROOT}/settings.json:cleanupPeriodDays (default)"),
        });
        assert!(retention_settled(&native));
        let mut t3 = crate::test_support::make_t3_profile("t3", ROOT);
        t3.retention = native.retention.clone();
        assert!(retention_settled(&t3));
        // Launch arguments may replace the settings file, so the days it names may not apply.
        t3.t3.as_mut().unwrap().settings_unresolved = true;
        assert!(!retention_settled(&t3));
        t3.t3.as_mut().unwrap().settings_unresolved = false;
        t3.t3.as_mut().unwrap().enabled = false;
        assert!(!retention_settled(&t3));
    }

    const NOV1: Millis = 1_793_491_200_000;
    const DAY_MS: Millis = 86_400_000;
    const ROOT: &str = "/synthetic/home/.claude";
    /// The user agent directory where a file would replace the built-in Explore in [`ROOT`].
    const ROOT_AGENTS: &str = "/synthetic/home/.claude/agents";

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
            profiles: vec!["profile".into()],
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
                ..Default::default()
            }),
            savings_usd: Some(150.0),
            margin: None,
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
            comparison_estimate: PerTtl::default(),
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
                margin: None,
                coverage: Coverage {
                    priced: 3,
                    total: 4,
                    ..Default::default()
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
                    summary_scale: None,
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
            cap: None,
            usage_estimates: vec![],
            cache_evidence: vec![],
            reowned: None,
            capped: None,
            unsettable: None,
        })
    }

    fn period(status: PeriodStatus, returned: bool) -> PeriodKind {
        PeriodKind {
            status,
            reason: "synthetic gap",
            return_timing: if returned {
                ReturnTiming::AfterExpiry
            } else {
                ReturnTiming::Final
            },
            outcome: PeriodOutcome::Unobserved,
        }
    }

    fn counts(kinds: impl IntoIterator<Item = PeriodKind>) -> PeriodCounts {
        let mut counts = PeriodCounts::default();
        for kind in kinds {
            counts.add(kind, kind.status == PeriodStatus::Eligible);
        }
        counts
    }

    fn idle_scenario(periods: PeriodCounts, delta: Option<f64>) -> IdleScenario {
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
            delta: delta.map(|usd| Estimate { usd, margin: None }),
            coverage: Coverage {
                priced: 1,
                total: 1,
                ..Default::default()
            },
            monthly_delta: delta.map(|usd| Estimate { usd, margin: None }),
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
                closed_delta: Some(Estimate {
                    usd: 0.1,
                    margin: None,
                }),
                open_delta: None,
                larger_summary_delta: Some(Estimate {
                    usd: -2.5,
                    margin: None,
                }),
                late_trigger_delta: Some(Estimate {
                    usd: 0.3,
                    margin: None,
                }),
                lifetime_break_even_fraction: None,
                reread_break_even_usd_per_resumed_summary: Some(0.02),
            },
            unknown: vec!["synthetic idle unknown".into()],
        }
    }

    fn idle_check(base: CheckBase<bool>, periods: PeriodCounts, delta: Option<f64>) -> CheckResult {
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
            hundred_lines: None,
            description: None,
            shares: vec![],
            pretended_requests: None,
            profiles: vec!["profile".into()],
            kind,
            name: name.into(),
            requests: 100,
            priced: if estimated.is_some() { 100 } else { 0 },
            conditional_requests: 0,
            tier_unknown_requests: 0,
            assumptions: vec![],
            tokens: estimated.map(|_| 12_345),
            cost_usd: Usd::from_f64(estimated.unwrap_or(0.0)),
            estimated_usd: estimated,
            margin: None,
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
        // Ten of its requests unpriced, so its saving is extrapolated with a margin.
        ttl_main.margin = Some(Margin::Usd(1.25));
        let mut ttl_sub = check_base(Action::Keep, Some(Ttl::OneHour), Some(Ttl::OneHour));
        ttl_sub.coverage = Some(Coverage {
            priced: 2000,
            total: 2000,
            ..Default::default()
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
                counts([
                    period(PeriodStatus::Eligible, true),
                    period(PeriodStatus::Censored, false),
                ]),
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
                    unpriced_reads: 0,
                    use_unproven: false,
                    tier_unknown: false,
                    places: Default::default(),
                })
            },
        ];
        Report {
            subagents: vec![],
            fable: vec![],
            bash: vec![],
            advisor: vec![],
            inbound: vec![],
            lsp: vec![],
            generated_at: "2026-12-01T00:00:00.000Z".into(),
            scan: Scan {
                subagents: vec![],
                t3: vec![],
                sessions: Default::default(),
                defaults: false,
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
                    unreadable: Default::default(),
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
                // Half the requests priced, extrapolated to twice what they cost.
                cost: cost(None, 750.0),
                estimate: Some(Estimate {
                    usd: 1500.0,
                    margin: Some(Margin::Usd(60.0)),
                }),
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

    /// Each change `doctor` offers, as the walk describes it: a heading, then its edits.
    fn changes(report: &Report) -> String {
        let mut text = String::new();
        for fix in crate::doctor::fixes(report, false) {
            let described = crate::doctor::describe(&fix, None);
            let _ = writeln!(text, "  {}", described.head);
            for (line, _) in described.body {
                let _ = writeln!(text, "    {line}");
            }
        }
        text
    }

    fn text(report: &Report, width: Option<usize>, details: bool) -> String {
        rendered(
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

    /// `text` without the rule a color terminal draws after each section heading.
    fn unruled(text: &str) -> String {
        text.split_inclusive('\n')
            .map(|line| {
                let body = line.trim_end_matches('\n');
                let bare = body.trim_end_matches(['─', '-']);
                if body.len() - bare.len() >= 3 && bare.ends_with(' ') {
                    format!("{}{}", bare.trim_end(), &line[body.len()..])
                } else {
                    line.to_string()
                }
            })
            .collect()
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
    fn defaults_show_what_policy_keeps_off_on_its_own_line() {
        let mut report = synthetic();
        report.scan.defaults = true;
        report.scan.profiles[0].policy_off = vec![
            (
                "/etc/managed-settings.json".into(),
                deny("Monitor").unwrap(),
            ),
            (
                "/etc/managed-settings.json".into(),
                deny("mcp__docs__*").unwrap(),
            ),
        ];
        let text = text(&report, None, true);
        let monitor = report_line(&text, "Monitor tool off");
        assert_eq!(monitor.mark, "info");
        assert_eq!(
            monitor.evidence,
            "Policy in /etc/managed-settings.json keeps it off, which a user can't change."
        );
        assert_eq!(report_line(&text, "MCP server docs off").mark, "info");
    }

    #[test]
    fn settings_that_differ_from_their_defaults_say_what_they_save_against_them() {
        let mut report = synthetic();
        // The subagent TTL is pinned at 1h, against Claude Code's 5m default; the main TTL
        // is pinned at 5m, whose default depends on how usage is paid.
        let text = text(&report, None, false);
        let sub = report_line(&text, "Subagent prompt cache TTL");
        assert_eq!(sub.default, "default 5m, saves $4.20/mo");
        let main = report_line(&text, "Main-agent prompt cache TTL");
        assert_eq!(
            main.default,
            "default 1h on a Claude plan, 5m on usage credits or an API key, costs up to ~$12/mo ± $1.25 more"
        );
        // A window at the model's tuned default says nothing; an explicit one names it.
        report.scan.profiles[0].compaction.tuned.insert(
            "claude-opus-5-5".into(),
            TunedWindow {
                value: Some(1_000_000),
                reason: "synthetic".into(),
            },
        );
        // Workflow's switch is on, Retired was turned off after it was sent, and Monitor
        // was never sent at all.
        report.scan.profiles[0].switched_off = [deny("Retired"), deny("Monitor")]
            .into_iter()
            .flatten()
            .collect();
        let text = super::tests::text(&report, None, true);
        let headline = text
            .lines()
            .find(|l| l.trim_start().starts_with("Compared with"));
        assert_eq!(
            headline.map(str::trim_start),
            Some(
                "Compared with defaults, your settings save about $4.22/mo, <1% of this host's spend, including an estimate for 1 feature that is now off. 1 setting whose default depends on how usage is paid costs up to ~$12/mo ± $1.25 more, 1% of this host's spend. 2 more settings differ by amounts not measured. Overlapping settings may save less."
            )
        );
        // What every offered change saves together sits just above the comparison.
        let lines: Vec<&str> = text.lines().map(str::trim_start).collect();
        let at = lines
            .iter()
            .position(|l| l.starts_with("Compared with"))
            .unwrap();
        assert!(
            lines[at - 1].starts_with("Together, these changes save "),
            "{}",
            lines[at - 1]
        );
        let window = report_line(&text, "Auto-compact window (claude-opus-5-5) 200k");
        assert_eq!(window.default, "default 1M, saving not measured");
        assert_eq!(
            report_line(&text, "Auto-compact window (claude-opus-5-5) 1M").default,
            ""
        );
        let retired = report_line(&text, "Retired tool off");
        assert_eq!(retired.mark, "info");
        assert!(
            retired
                .default
                .starts_with("default on, saves an estimated $"),
            "{retired:#?}"
        );
        assert!(retired.evidence.starts_with("Off in "), "{retired:#?}");
        let monitor = report_line(&text, "Monitor tool off");
        assert_eq!(monitor.default, "default on, saving not measured");
        assert_eq!(
            Some(monitor.what.as_str()),
            crate::descriptions::claude_tool("Monitor")
        );
        assert!(
            monitor
                .evidence
                .contains("No request in the window carried it")
        );
        // A switch the file sets before any request shows it reads as off, and doctor no
        // longer offers it: Artifact was recommended off, Agent was kept.
        report.scan.profiles[0].switched_off = [
            switch(&["enableArtifact"], ControlValue::Bool(false)),
            deny("Agent"),
        ]
        .into_iter()
        .flatten()
        .collect();
        let text = super::tests::text(&report, None, false);
        let lines = report_lines(&text);
        for name in ["Artifact tool", "Agent tool"] {
            let named: Vec<_> = lines.iter().filter(|l| l.head.starts_with(name)).collect();
            assert_eq!(named.len(), 1, "{name}: {named:#?}");
            assert!(
                named[0].head.starts_with(&format!("{name} off")),
                "{named:#?}"
            );
            assert!(
                named[0]
                    .default
                    .starts_with("default on, saves an estimated $"),
                "{named:#?}"
            );
        }
        assert!(
            !crate::doctor::fixes(&report, false)
                .iter()
                .any(|f| f.name.starts_with("Artifact"))
        );
        let detailed = super::tests::text(&report, None, true);
        assert!(detailed.contains("Already off in its settings file"));
        assert!(!detailed.contains("Recommended off: \"enableArtifact\": false"));
        // A file that outranks the profile's and sets the switch keeps the feature's own line.
        let project = Workspace::new();
        let outranking = project.home.join("settings.json");
        std::fs::write(&outranking, r#"{"enableArtifact": true}"#).unwrap();
        report.scan.profiles[0].outranking_files = vec![outranking.to_string_lossy().into()];
        let text = super::tests::text(&report, None, false);
        assert!(
            report_line(&text, "Artifact tool on -> off")
                .default
                .is_empty()
        );
        // So does one whose latest request no longer carried the feature: no off line.
        fn historical(costs: &mut [crate::model::ContextCost], path: &str) {
            for c in costs {
                if c.control.as_ref().is_some_and(|k| k.path.join(".") == path) {
                    c.current = false;
                }
                historical(&mut c.details, path);
            }
        }
        historical(&mut report.context, "enableArtifact");
        let text = super::tests::text(&report, None, false);
        assert!(
            !report_lines(&text)
                .iter()
                .any(|l| l.head.starts_with("Artifact tool off")),
            "{text}"
        );
        // One that sets it off too leaves it off.
        std::fs::write(&outranking, r#"{"enableArtifact": false}"#).unwrap();
        let text = super::tests::text(&report, None, false);
        assert!(
            report_line(&text, "Artifact tool off")
                .default
                .starts_with("default on")
        );
        // A profile sharing the file that another file turns it back on for hides the
        // line, whichever profile comes first.
        let turned_on = project.home.join("on.json");
        std::fs::write(&turned_on, r#"{"enableArtifact": true}"#).unwrap();
        let mut sharing = report.scan.profiles[0].clone();
        sharing.id = "sharing".into();
        sharing.outranking_files = vec![turned_on.to_string_lossy().into()];
        for first in [false, true] {
            let mut report = report.clone();
            if first {
                report.scan.profiles.insert(0, sharing.clone());
            } else {
                report.scan.profiles.push(sharing.clone());
            }
            let text = super::tests::text(&report, None, false);
            assert!(
                !report_lines(&text)
                    .iter()
                    .any(|l| l.head.starts_with("Artifact tool off")),
                "{text}"
            );
        }
    }

    #[test]
    fn each_check_prices_its_current_value_against_its_documented_default() {
        use crate::defaults::{Saving, check};
        let profile = make_profile("profile", ROOT);
        let saving =
            |result: &CheckResult| check(result, Some(&profile)).map(|b| (b.default, b.saving));
        let periods = || counts([period(PeriodStatus::Eligible, true)]);
        // Idle compaction is on by default; on minus off is what turning it off saves.
        let off = check_base(Action::Keep, Some(false), Some(false));
        assert_eq!(
            saving(&idle_check(off, periods(), Some(3.5))),
            Some(("on".into(), Saving::Saves(3.5)))
        );
        let on = check_base(Action::Keep, Some(true), Some(true));
        assert_eq!(saving(&idle_check(on, periods(), Some(3.5))), None);
        // Fast mode costs its premium more than the default, standard speed.
        let fast = check_base(
            Action::Change,
            Some("fast".to_string()),
            Some("standard".into()),
        );
        assert_eq!(
            saving(&speed_check(fast, "fast")),
            Some(("off".into(), Saving::Saves(-150.0)))
        );
        let standard = check_base(Action::Keep, Some("standard".to_string()), None);
        assert_eq!(saving(&speed_check(standard, "standard")), None);
        // Codex memories are off by default, so on costs what the check measured.
        let memory = |harness, current: bool, unmeasured_generation| {
            CheckResult::Memory(MemoryCheck {
                base: check_base(Action::Keep, Some(current), Some(current)),
                harness,
                off: "off".into(),
                off_file: None,
                context_usd: None,
                writes_usd: None,
                reads: 0,
                writes: 0,
                unresolved: None,
                overrides: vec![],
                unmeasured_generation,
                unpriced_reads: 0,
                use_unproven: false,
                tier_unknown: false,
                places: Default::default(),
            })
        };
        assert_eq!(
            saving(&memory(Harness::Codex, true, false)),
            Some(("off".into(), Saving::Saves(-150.0)))
        );
        assert_eq!(
            saving(&memory(Harness::Codex, true, true)),
            Some(("off".into(), Saving::NotMeasured))
        );
        assert_eq!(saving(&memory(Harness::Codex, false, false)), None);
        assert_eq!(
            saving(&memory(Harness::Claude, false, false)),
            Some(("on".into(), Saving::NotMeasured))
        );
        // A line that could hold a use leaves nonuse unproven, so it is only unrecorded.
        let CheckResult::Memory(mut unused) = memory(Harness::Claude, true, false) else {
            unreachable!()
        };
        assert_eq!(memory_uses(&unused), "never read back or written");
        unused.use_unproven = true;
        assert_eq!(memory_uses(&unused), "no recorded reads or writes");
        // A different effort changes the output, so its saving is never measured.
        let effort = |level: &str, owner: &str| {
            let (level, effective) = level.split_once('>').unwrap_or((level, level));
            CheckResult::Effort(EffortCheck {
                base: check_base(Action::Keep, Some(effective.to_string()), None),
                harness: Harness::Claude,
                control: EffortControl {
                    model: Some("claude-opus-5-5".into()),
                    model_owner: "synthetic".into(),
                    level: Some(level.into()),
                    owner: owner.into(),
                    effective: Some(effective.into()),
                    lowered: None,
                    caps: vec![],
                    default: Some("medium".into()),
                    unresolved: None,
                    picked: None,
                    provider: None,
                },
                enterprise: false,
                runs: vec![],
                estimate: None,
            })
        };
        let set = format!("{ROOT}/settings.json:effortLevel");
        assert_eq!(
            saving(&effort("high", &set)),
            Some(("medium".into(), Saving::NotMeasured))
        );
        assert_eq!(saving(&effort("medium", &set)), None);
        assert_eq!(saving(&effort("medium", "x (the model's default)")), None);
        // A cap that lowers the default runs below it, whatever names the level.
        assert_eq!(
            saving(&effort("medium>low", "x (the model's default)")),
            Some(("medium".into(), Saving::NotMeasured))
        );
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
        let summary = text(&report, None, true);
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
        let line = report_line(&text(&report, None, true), "Auto-compact window");
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
        // Exact binary halves round up, where Rust's formatter rounds them to even.
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
    fn a_feature_says_what_it_does_from_curated_shipped_or_recorded_names() {
        let piece = |kind, name: &str, description: Option<&str>| {
            let mut c = context_piece(kind, name, None, Some(1.0), Some(0));
            c.description = description.map(str::to_string);
            c
        };
        // Claude Code's own sentence outranks the description written for the model.
        let artifact = piece(ContextKind::Tool, "Artifact", Some("Renders HTML."));
        assert!(piece_what(&artifact).starts_with("Lets Claude publish"));
        let unknown = piece(ContextKind::Tool, "Mystery", Some("Does a thing."));
        assert_eq!(piece_what(&unknown), "Does a thing.");
        let unknown = piece(ContextKind::Tool, "Mystery", None);
        assert_eq!(piece_what(&unknown), "A tool with no recorded description.");
        // A server with no description of its own names its tools.
        let mut server = piece(ContextKind::McpServer, "docs", None);
        assert_eq!(
            piece_what(&server),
            "An MCP server with no recorded description."
        );
        for (n, tool) in ["read", "search", "list", "get", "put"].iter().enumerate() {
            server.details.push(piece(
                ContextKind::McpTool,
                &format!("mcp__docs__{tool}"),
                None,
            ));
            let want = match n {
                0 => "its tool is read.",
                1 => "its tools are read and search.",
                3 => "its tools are read, search, list and get.",
                4 => "its tools include read, search, list and 2 more.",
                _ => continue,
            };
            assert_eq!(
                piece_what(&server),
                format!("An MCP server with no recorded description; {want}")
            );
        }
        server.description = Some("Searches the docs.".into());
        assert_eq!(piece_what(&server), "Searches the docs.");
    }

    #[test]
    fn an_extrapolated_idle_change_names_its_priced_sessions() {
        let mut report = synthetic();
        let mut base = check_base(Action::Change, Some(false), Some(true));
        base.owner = format!("{ROOT}/settings.json:idleCompaction");
        base.evidence = Evidence::Estimated;
        base.savings_usd = Some(30.0);
        base.monthly_savings_usd = Some(30.0);
        base.margin = Some(Margin::Usd(6.0));
        base.coverage = Some(Coverage {
            priced: 4,
            total: 5,
            ..Default::default()
        });
        report.checks = vec![idle_check(base, PeriodCounts::default(), Some(-30.0))];
        let rendered = text(&report, None, true);
        let row = report_line(&rendered, "Idle compaction");
        assert!(row.head.contains("saves ~$30/mo ± $6"), "{row:?}");
        assert!(
            row.evidence.starts_with("Replayed on and off, assuming the same sessions, extrapolated from the 80% of sessions that could be priced to the rest, assuming they resemble each other."),
            "{row:?}"
        );
        // Sessions T3 could not settle stand in at the line's share, an assumption the
        // margin leaves out.
        if let CheckResult::Idle(c) = &mut report.checks[0] {
            c.base.coverage = Some(Coverage {
                priced: 4,
                total: 6,
                left_out: 4,
                stand_ins: Some(1),
            });
        }
        let rendered = text(&report, None, true);
        let row = report_line(&rendered, "Idle compaction");
        assert!(
            row.evidence.starts_with("Replayed on and off, assuming the same sessions, extrapolated from the 66.6% of sessions that could be priced to the rest, assuming they resemble each other; 4 sessions T3 could not settle between this line's owner and another count as 1, this line's share of those T3 did settle, assuming they split the same way, which the margin does not cover."),
            "{row:?}"
        );
        // One open session reads in the singular.
        if let CheckResult::Idle(c) = &mut report.checks[0] {
            c.base.coverage.as_mut().unwrap().left_out = 1;
            c.base.coverage.as_mut().unwrap().stand_ins = Some(0);
        }
        let rendered = text(&report, None, true);
        let row = report_line(&rendered, "Idle compaction");
        assert!(
            row.evidence.contains(
                "; 1 session T3 could not settle between this line's owner and another counts as 0,"
            ),
            "{row:?}"
        );
        // With no share to count the open sessions at, the line names that gap rather than
        // missing usage.
        if let CheckResult::Idle(c) = &mut report.checks[0] {
            c.after_recommendations.delta = None;
            c.after_recommendations.coverage = Coverage {
                priced: 4,
                total: 4,
                left_out: 2,
                stand_ins: None,
            };
            c.base.action = Action::Advisory;
            c.base.recommended = None;
            c.base.savings_usd = None;
            c.base.monthly_savings_usd = None;
            c.base.margin = None;
        }
        let rendered = text(&report, None, true);
        let row = report_line(&rendered, "Idle compaction");
        assert!(
            row.evidence
                .starts_with("Not enough evidence: T3 could not settle some sessions"),
            "{row:?}"
        );
    }

    #[test]
    fn the_summary_gives_one_line_per_setting() {
        let report = synthetic();
        let plain = text(&report, None, false);
        let text = text(&report, None, true);
        // By default each setting is one line; details add its evidence under it.
        assert!(report_lines(&plain).iter().all(|r| r.evidence.is_empty()));
        assert_eq!(report_lines(&plain).len(), report_lines(&text).len());
        let rows = report_lines(&text);
        let line = |head: &str| report_line(&text, head);
        // The changes lead, biggest saving first; everything else follows by cost, and
        // lines without a figure come last in verdict order.
        let order: Vec<(bool, &str)> = rows
            .iter()
            .map(|r| (r.change, r.head.split(" on").next().unwrap()))
            .collect();
        let names: Vec<&str> = order.iter().map(|(_, n)| *n).collect();
        let starts = |n: &str, prefix: &str| n.starts_with(prefix);
        let expected = [
            "Auto memory",
            "Artifact tool",
            "Skills synced from claude.ai",
            "Main-agent prompt cache TTL",
            "Skill example",
            "MCP server docs",
            "System prompt and core tools",
            "Agent tool",
            "Skill tools:deploy",
            "/synthetic/home/.claude/CLAUDE.md",
            "Subagent prompt cache TTL",
            "Auto-compact window (claude-opus-5-5) 1M",
            "Fast mode",
            "Auto-compact window (claude-opus-5-5) 200k",
            "Idle compaction",
            "Workflow tool",
            "/repo/CLAUDE.md",
            "MCP tool loading",
        ];
        assert_eq!(names.len(), expected.len(), "{names:#?}");
        for (name, prefix) in names.iter().zip(expected) {
            assert!(starts(name, prefix), "{name} vs {prefix}: {names:#?}");
        }
        assert!(order[..6].iter().all(|(change, _)| *change));
        assert!(order[6..].iter().all(|(change, _)| !*change));
        // Details open every line with what it does; by default it stays one line.
        assert!(report_lines(&plain).iter().all(|r| r.what.is_empty()));
        let what: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r.head.as_str(), r.what.as_str()))
            .collect();
        assert!(what.iter().all(|(_, w)| w.ends_with('.')), "{what:#?}");
        for (head, want) in [
            (
                "Artifact tool",
                "Lets Claude publish an HTML or Markdown file to claude.ai as an artifact: a private, interactive page you can share.",
            ),
            (
                "Skills synced from claude.ai",
                "Skills from your claude.ai account, which Claude Code syncs into every session.",
            ),
            // Neither curated nor shipped: it says so rather than guess.
            ("Skill example", "A skill with no recorded description."),
            (
                "Fast mode",
                "Fast mode serves responses faster at a premium price per token.",
            ),
        ] {
            assert_eq!(line(head).what, want, "{head}");
        }
        let has = |head: &str, mark: &str, evidence: &str| {
            let row = line(head);
            assert_eq!(
                (row.head.as_str(), row.mark.as_str(), row.evidence.as_str()),
                (head, mark, evidence)
            );
        };
        assert!(text.contains("\n  2 changes recommended and 4 worth considering.\n"));
        has(
            "Main-agent prompt cache TTL 5m -> 1h saves ~$25/mo ± $1.25 2%",
            "change",
            "Replayed at 5m and 1h, assuming the same sessions, extrapolated from the 99.5% of requests with measured usage to the rest, assuming they resemble each other; 1h costs less. Set in /synthetic/home/.claude/settings.json:promptCacheTtl.",
        );
        // A change names the edit `doctor` offers for it.
        assert_eq!(
            line("Main-agent prompt cache TTL").setting,
            r#""promptCacheTtl": "1h" in /synthetic/home/.claude/settings.json"#
        );
        has(
            "Artifact tool on -> off saves $90/mo 6%",
            "change",
            "Never used; it cost $90 over the window. Turn off: \"enableArtifact\": false.",
        );
        // Used, but above the limit per use: worth considering off, with the setting inline.
        has(
            "Skill example on -> typed only saves $12/mo 1%",
            "consider",
            "The model used it 2 times, $6 each; it cost $12 over the window. Hide it from the model: \"skillOverrides\": {\"example\": \"user-invocable-only\"}.",
        );
        // Turning syncing off removes typed uses too, so every use counts.
        has(
            "Skills synced from claude.ai on -> off saves $30/mo 2%",
            "consider",
            "Used 2 times, $15 each; it cost $30 over the window. Turn off: \"syncClaudeAiSkills\": false.",
        );
        has(
            "Skill tools:deploy costs $6 <1%",
            "info",
            "Never used. No setting turns it off: turn off its plugin with /plugin, or remove the skill from the added directory.",
        );
        has(
            "Agent tool on costs $40 3%",
            "keep",
            "Used 310 times, $0.13 each; it cost $40 over the window. Turn off: \"permissions\": {\"deny\": [\"Agent\"]}.",
        );
        // One use's share would repeat the total, so it is left out.
        assert_eq!(
            line("MCP server docs").evidence,
            "Used once; it cost $7.50 over the window. Turn off: \"permissions\": {\"deny\": [\"mcp__docs\"]}. Server override: mcpServers.docs.alwaysLoad in the MCP configuration that declares it (.mcp.json or that scope's configuration)."
        );
        // A setting that is not changed names where it is set.
        assert_eq!(
            line("Agent tool").setting,
            "/synthetic/home/.claude/settings.json:permissions.deny"
        );
        has(
            "Subagent prompt cache TTL 1h saves $4.20/mo <1%",
            "keep",
            "Replayed at 5m and 1h, assuming the same sessions; 1h costs less than 5m. Set in /synthetic/home/.claude/settings.json:promptCacheTtl.",
        );
        has(
            "Workflow tool on",
            "unknown",
            "Not enough evidence: none of its requests could be priced; never used. Turn off: \"enableWorkflows\": false.",
        );
        has(
            "Idle compaction on",
            "unknown",
            "Not enough evidence: on and off cost the same on recorded idle gaps, so neither is cheaper. Set in /synthetic/home/.claude/settings.json:idleCompaction.",
        );
        assert!(text.contains(
            "Not enough evidence: too few recorded compactions to model a different window. Its requests carried a median of 150,000 tokens, and 1 in 10 carried more than 180,000."
        ));
        has(
            "/repo/CLAUDE.md costs unknown",
            "info",
            "No setting turns it off.",
        );
        // A zero-cost fixed piece is hidden; the system prompt and Bash are summed.
        has(
            "System prompt and core tools costs $213 14%",
            "info",
            "Includes core tools such as Bash and Read, and the skill and subagent lists. No setting turns these off.",
        );
        // Auto memory over the limit per use is worth considering off, as a skill is,
        // without changing the exit code.
        has(
            "Auto memory on -> off saves $150/mo 10%",
            "consider",
            "It cost more than --max-cost-per-use per read or write. Measured: used 3 times (1 read, 2 writes). Set \"autoMemoryEnabled\": false in /synthetic/home/.claude/settings.json.",
        );
        has(
            "Fast mode off",
            "keep",
            "On a Claude plan, fast mode is paid only from usage credits, at its API rates, even with plan usage left. Set in /synthetic/home/.claude/settings.json:fastMode.",
        );
        // Sources cite the guidance the evidence quotes, so only details list them.
        assert!(
            text.contains("\n  Sources: https://code.claude.com/docs/en/fast-mode#requirements")
        );
        assert!(!plain.contains("Sources:"));
        // Pieces no longer sent and those without a verdict get no line of their own.
        assert!(!plain.contains("Retired") && !plain.contains("Skill list framing"));
        // The walk offers the same changes in the list's order: biggest saving first.
        let listed = changes(&report);
        assert!(!text.contains("Changes to make"));
        let edits: Vec<&str> = listed
            .lines()
            .filter(|l| l.starts_with("      "))
            .map(str::trim)
            .collect();
        assert_eq!(
            edits,
            [
                r#""autoMemoryEnabled": false"#,
                r#""enableArtifact": false"#,
                r#""syncClaudeAiSkills": false"#,
                r#""promptCacheTtl": "1h""#,
                r#""skillOverrides": {"example": "user-invocable-only"}"#,
                r#""permissions": {"deny": ["mcp__docs"]} (added to the list)"#,
            ],
            "{listed}"
        );
    }

    #[test]
    fn plan_rules_precede_the_setting() {
        let mut report = two_profiles();
        // An unset main TTL, which Claude Code resolves by how the request is paid for.
        report.scan.profiles[0].ttl.main = TtlControl {
            value: None,
            owner: format!("{ROOT}/settings.json:promptCacheTtl"),
            reason: TTL_BILLING_UNKNOWN.into(),
            varies: false,
        };
        let ttl = ttl_mut(&mut report);
        ttl.base.current = None;
        ttl.base.action = Action::Advisory;
        let background = |feature, on: bool, profile: &str| {
            let mut base = check_base(Action::Keep, Some(on), Some(on));
            base.profile = profile.into();
            base.profiles = vec![profile.into()];
            base.owner = format!("{ROOT}/settings.json:promptSuggestionEnabled");
            CheckResult::Background(BackgroundCheck {
                base,
                feature,
                directories: vec![],
                reason: "user setting".into(),
                scope: None,
                off: "\"promptSuggestionEnabled\": false".into(),
                off_file: Some(format!("{ROOT}/settings.json")),
                generations: Some(0),
                cost: cost(Some(0.0), 0.0),
                cost_estimate: None,
                lost_warming: cost(Some(0.0), 0.0),
                lost_estimate: None,
            })
        };
        report.checks.extend([
            background(BackgroundFeature::PromptSuggestions, true, "profile"),
            background(BackgroundFeature::SessionRecaps, true, "profile"),
            {
                let mut window = check_base(Action::Keep, Some(1_000_000), Some(1_000_000));
                window.profile = "another-profile".into();
                window.owner =
                    "/synthetic/second-profile/config.toml:model_auto_compact_token_limit".into();
                compaction_check(window)
            },
        ]);
        let summary = text(&report, None, true);
        assert!(summary.contains("\n  Dollars are public API prices, not an invoice: the closest public measure of what a request uses of a Claude or ChatGPT plan's limits, and what Claude's usage credits bill past them. Nothing was changed.\n"), "{summary}");
        let rows = report_lines(&summary);
        let evidence = |head: &str| {
            let rows: Vec<&str> = rows
                .iter()
                .filter(|r| r.head.starts_with(head))
                .map(|r| r.evidence.as_str())
                .collect();
            assert_eq!(rows.len(), 1, "{head}: {summary}");
            rows[0].to_string()
        };
        // Each rule sits before the setting that changes the line, so the line still ends with it.
        assert!(evidence("Main-agent prompt cache TTL").ends_with(
            " While it is unset, Claude Code gives the main conversation 1h on a Claude plan within its included usage, and 5m once usage credits or an API key pay for it. Set in /synthetic/home/.claude/settings.json:promptCacheTtl."
        ));
        // A set TTL is the same on any plan.
        assert!(!evidence("Subagent prompt cache TTL").contains("plan"));
        assert!(evidence("Fast mode fast -> off").contains(
            " On a ChatGPT plan, Fast uses included usage 2.5 times as fast as Standard and Ultrafast 8 times, which can exceed the API premium priced here. Set in /synthetic/second-profile/config.toml:service_tier."
        ));
        assert!(
            evidence("Fast mode off")
                .starts_with("On a Claude plan, fast mode is paid only from usage credits")
        );
        assert!(evidence("Prompt suggestions").ends_with(
            " Claude Code skips suggestions while a plan is near its usage limit, which this estimate cannot see. Set \"promptSuggestionEnabled\": false in /synthetic/home/.claude/settings.json to turn it off."
        ));
        assert!(!evidence("Session recaps").contains("plan"));
        let codex_window: Vec<&str> = rows
            .iter()
            .filter(|r| {
                r.head.starts_with("Auto-compact window") && r.evidence.contains("second-profile")
            })
            .map(|r| r.evidence.as_str())
            .collect();
        assert_eq!(codex_window.len(), 1, "{summary}");
        assert!(codex_window[0].contains(" ChatGPT credits charge cache writes as ordinary input, below the API write rate priced here on GPT-5.6 and later. Set in "));
        // No Claude window carries the ChatGPT rule.
        assert_eq!(
            rows.iter()
                .filter(|r| r.evidence.contains("ChatGPT credits charge cache writes"))
                .count(),
            1
        );
        for source in [
            "https://code.claude.com/docs/en/prompt-caching#which-ttl-each-request-gets",
            "https://code.claude.com/docs/en/costs#background-token-usage",
            "https://learn.chatgpt.com/docs/pricing",
            "https://code.claude.com/docs/en/fast-mode#requirements",
        ] {
            assert!(
                summary
                    .lines()
                    .any(|l| l.trim_start().starts_with("Sources: ") && l.contains(source)),
                "{source}: {summary}"
            );
        }
        // Details give each rule its own field, with its source.
        let details = text(&report, None, true);
        assert!(
            details.contains(&format!("\n  {PRICES_MEAN} Nothing was changed.\n")),
            "{details}"
        );
        assert!(details.contains("\n    Plan: On a ChatGPT plan, Fast uses included usage 2.5 times as fast as Standard and Ultrafast 8 times, which can exceed the API premium priced here. https://learn.chatgpt.com/docs/pricing\n"), "{details}");
        assert!(!details.contains("subscription"));
        assert!(!summary.contains("subscription"));
    }

    #[test]
    fn a_window_change_under_the_percent_override_makes_one_setting_own_each_trigger() {
        use crate::doctor::{Edit, How, Op, Patch, fixes};
        use crate::model::{Change, Reowned};
        use serde_json::json;
        let file = format!("{ROOT}/settings.json");
        let key = crate::controls::PERCENT_KEY;
        let variable = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";
        let edit = |path: &[&str], value: serde_json::Value, op: Op| Edit {
            path: path.iter().map(|p| p.to_string()).collect(),
            value,
            op,
        };
        let row = |model: &str, window: i64| {
            edit(
                &["modelSettings", model, "autoCompactWindow"],
                json!(window),
                Op::Set,
            )
        };
        let remove = |variable: &str| edit(&["env", variable], json!(null), Op::Remove);
        let change = |owner: &str, value: Option<i64>| {
            let (source, setting) = crate::controls::locate(owner).expect("a window setting");
            Change {
                source,
                setting,
                value,
            }
        };
        let plan = |changes: Vec<Change>| Reowned {
            changes,
            unkept: vec![],
        };
        let how = |owner: String, reowned: Reowned| {
            let mut report = synthetic();
            let mut window = check_base(Action::Change, Some(200_000), Some(300_000));
            window.owner = owner;
            let mut check = compaction_check(window);
            if let CheckResult::Compaction(c) = &mut check {
                c.reowned = Some(reowned);
            }
            report.checks = vec![check];
            report.context.clear();
            fixes(&report, false)
                .into_iter()
                .find(|f| f.name.starts_with("Auto-compact window"))
                .expect("a window fix")
                .how
        };
        let row_of = |model: &str| format!("{file}:modelSettings.{model}.autoCompactWindow");
        let removed = change(&format!("{file}:env.{key}"), None);

        // Alone on its line, the window stays where its owner holds it.
        let shared = format!("{file}:autoCompactWindow");
        assert_eq!(
            how(
                shared.clone(),
                plan(vec![change(&shared, Some(300_000)), removed.clone()])
            ),
            How::Edit(Patch {
                file: file.clone(),
                harness: Harness::Claude,
                edits: vec![
                    edit(&["autoCompactWindow"], json!(300_000), Op::Set),
                    remove(key),
                ],
            })
        );

        // Beside another model, the target and the other model each get their own row.
        let beside = vec![
            change(&row_of("claude-opus-5-5"), Some(300_000)),
            change(&row_of("claude-haiku-5-5"), Some(150_000)),
            removed.clone(),
        ];
        assert_eq!(
            how(shared.clone(), plan(beside.clone())),
            How::Edit(Patch {
                file: file.clone(),
                harness: Harness::Claude,
                edits: vec![
                    row("claude-opus-5-5", 300_000),
                    row("claude-haiku-5-5", 150_000),
                    remove(key),
                ],
            })
        );

        // A window variable in the file comes out as a removal there.
        let lifted = format!("{file}:env.{variable}");
        let mut with_window = beside.clone();
        with_window.insert(1, change(&lifted, None));
        assert_eq!(
            how(lifted.clone(), plan(with_window.clone())),
            How::Edit(Patch {
                file: file.clone(),
                harness: Harness::Claude,
                edits: vec![
                    row("claude-opus-5-5", 300_000),
                    remove(variable),
                    row("claude-haiku-5-5", 150_000),
                    remove(key),
                ],
            })
        );

        // A copy the shell exports is by hand, so the whole change is.
        let mut shell_window = with_window.clone();
        shell_window.insert(1, change(variable, None));
        let How::Manual { step, reason } = how(lifted.clone(), plan(shell_window)) else {
            panic!("expected a step by hand");
        };
        assert!(
            reason.contains(&format!("the {variable} environment variable sets it")),
            "{reason}"
        );
        assert!(step.contains(&format!("remove env.{variable}")), "{step}");
        assert!(step.contains(&format!("; remove {variable}")), "{step}");

        // Two plans that pin each other's target leave the second by hand, so accepting both
        // never reverts the first.
        let mut report = synthetic();
        let pinned = |model: &str, other: &str| {
            let mut window = check_base(Action::Change, Some(200_000), Some(100_000));
            window.owner = row_of(model);
            let mut check = compaction_check(window);
            if let CheckResult::Compaction(c) = &mut check {
                c.model = Some(model.into());
                c.models = vec![Some(model.into())];
                c.reowned = Some(plan(vec![
                    change(&row_of(model), Some(100_000)),
                    change(&row_of(other), Some(150_000)),
                    removed.clone(),
                ]));
            }
            check
        };
        report.checks = vec![
            pinned("claude-opus-5-5", "claude-haiku-5-5"),
            pinned("claude-haiku-5-5", "claude-opus-5-5"),
        ];
        report.context.clear();
        let windows: Vec<How> = fixes(&report, false)
            .into_iter()
            .filter(|f| f.name.starts_with("Auto-compact window"))
            .map(|f| f.how)
            .collect();
        assert!(matches!(windows[0], How::Edit(_)), "{windows:?}");
        assert!(
            matches!(&windows[1], How::Manual { reason, .. } if reason.contains("wants a different value for the same setting")),
            "{windows:?}"
        );

        // The shell's override is left by hand, naming every step.
        let mut shell = beside.clone();
        shell.insert(2, change(key, None));
        let How::Manual { step, reason } = how(shared.clone(), plan(shell.clone())) else {
            panic!("expected a step by hand");
        };
        assert!(
            reason.contains(&format!("the {key} environment variable sets it")),
            "{reason}"
        );
        assert!(step.contains("set \"modelSettings\""), "{step}");
        assert!(step.contains(&format!("remove env.{key}")), "{step}");
        assert!(step.ends_with(&format!("; remove {key}")), "{step}");

        // A plan that can't keep another model's trigger says so, by hand as well.
        let haiku = "claude-haiku-5-5: it would compact at 150000 instead of 75000";
        let How::Manual {
            step: unkept_step,
            reason,
        } = how(
            shared.clone(),
            Reowned {
                changes: shell,
                unkept: vec![haiku.into()],
            },
        )
        else {
            panic!("expected a step by hand");
        };
        assert_eq!(unkept_step, step);
        assert!(
            reason.starts_with(&format!("the {key} environment variable sets it")),
            "{reason}"
        );
        assert!(
            reason.ends_with(&format!(
                "; changing this window removes {key}, and {haiku}"
            )),
            "{reason}"
        );

        // A model whose trigger a project file owns is pinned there by hand.
        let project = "/synthetic/project/.claude/settings.json:modelSettings.claude-haiku-5-5.autoCompactWindow";
        let How::Manual { step, .. } = how(
            shared.clone(),
            plan(vec![
                change(&row_of("claude-opus-5-5"), Some(300_000)),
                change(project, Some(150_000)),
                removed,
            ]),
        ) else {
            panic!("expected a step by hand");
        };
        assert!(
            step.ends_with(&format!("; set {project} to 150000")),
            "{step}"
        );

        // T3 keeps an instance's variables as named entries, so the step names the entry.
        let instance = format!("{ROOT}/t3.json:providerInstances.claudeAgent.environment.{key}");
        let How::Manual { step, .. } = how(shared, plan(vec![change(&instance, None)])) else {
            panic!("expected a step by hand");
        };
        assert_eq!(
            step,
            format!(
                "remove the {key} entry from {ROOT}/t3.json:providerInstances.claudeAgent.environment"
            )
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
                    op: crate::doctor::Op::Set,
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
        report.checks = vec![
            idle_check(on, PeriodCounts::default(), None),
            idle_check(off, PeriodCounts::default(), None),
        ];
        report.context.clear();
        let idle = fixes(&report, false);
        assert_eq!(idle[0].how, set(&["idleCompaction"], json!(true)));
        assert_eq!(
            by_hand(idle[1].how.clone()),
            "Idle compaction wants a different value for the same setting"
        );

        // Auto memory that another file may turn on for some runs is turned off by hand.
        let mut report = synthetic();
        assert_eq!(
            how(&report, "Auto memory"),
            set(&["autoMemoryEnabled"], json!(false))
        );
        for check in &mut report.checks {
            if let CheckResult::Memory(m) = check {
                m.overrides = vec!["/synthetic/work.config.toml".into()];
            }
        }
        assert_eq!(
            by_hand(how(&report, "Auto memory")),
            "/synthetic/work.config.toml may set memories too"
        );

        // A switch that a file outranking the user's sets stays by hand; a deny rule
        // adds to every file's rules, so nothing outranks it.
        let w = Workspace::new();
        let project = w.home.join("settings.json");
        std::fs::write(
            &project,
            r#"{"enableArtifact": true, "permissions": {"deny": ["Read"]}}"#,
        )
        .unwrap();
        let project = project.display().to_string();
        let mut report = synthetic();
        report.scan.profiles[0].outranking_files = vec![project.clone()];
        let all = fixes(&report, false);
        let edits = |fix: &crate::doctor::Fix| match &fix.how {
            How::Edit(patch) => patch.edits[0].path.join("."),
            How::Manual { step, .. } => step.clone(),
        };
        let artifact = all.iter().find(|f| edits(f).contains("enableArtifact"));
        assert_eq!(
            by_hand(artifact.expect("an Artifact fix").how.clone()),
            format!("{project} sets it too, which outranks {file}")
        );
        let deny = all.iter().find(|f| edits(f) == "permissions.deny");
        assert!(matches!(deny.expect("a deny fix").how, How::Edit(_)));
    }

    #[test]
    fn subagent_default_card_cites_the_guidance_past_high_and_gates_on_claude_code() {
        use crate::doctor::Edit;
        use crate::model::Owned;
        use serde_json::json;
        let file = format!("{ROOT}/settings.json");
        let variable = format!("{file}:env.CLAUDE_CODE_SUBAGENT_EFFORT");
        // The card for a subagent effort set by `owner`, with the latest request on `version`.
        let card = |harness: Harness, level: Option<&str>, owner: &str, version: &str| {
            let mut report = synthetic();
            report.checks.clear();
            report.context.clear();
            let p = &mut report.scan.profiles[0];
            p.harness = harness;
            p.agents.effort = level.map(|value| Owned {
                value: value.into(),
                owner: owner.into(),
            });
            for r in &mut report.scan.requests {
                r.version = Some(version.into());
            }
            advice(&report, ReportOptions::default())
                .into_iter()
                .find(|i| i.name == "Subagent default")
        };
        let said = |card: &Item| format!("{} {}", card.text, card.sources.join(" "));

        // Unset, subagents follow the main session, which its own card covers.
        assert!(card(Harness::Claude, None, "", "2.1.316").is_none());

        // Past high: the provider's guidance, and the edit to high where the variable is set.
        let c = card(Harness::Claude, Some("max"), &variable, "2.1.316").expect("a card");
        assert!(!c.quiet);
        assert_eq!(c.value, "max -> high");
        let text = said(&c);
        assert!(
            text.starts_with("Subagents default to max on the main session's model"),
            "{text}"
        );
        assert!(text.contains("prone to overthinking"), "{text}");
        assert!(
            text.contains("The maintainer's pick for subagents is high"),
            "{text}"
        );
        assert!(text.contains(CLAUDE_MODEL_GUIDE), "{text}");
        assert!(!text.contains("ignores"), "{text}");
        let Some(How::Edit(patch)) = c.how else {
            panic!("expected an edit: {:?}", c.how)
        };
        assert_eq!(patch.file, file);
        assert_eq!(
            patch.edits,
            [Edit {
                path: vec!["env".into(), "CLAUDE_CODE_SUBAGENT_EFFORT".into()],
                value: json!("high"),
                op: crate::doctor::Op::Set,
            }]
        );

        // At or below high, a quiet card with no caution and no question.
        for level in ["high", "low"] {
            let c = card(Harness::Claude, Some(level), &variable, "2.1.316").expect("a card");
            assert!(c.quiet && c.how.is_none() && c.value.is_empty(), "{level}");
            assert!(!c.text.contains("model guide"), "{}", c.text);
        }

        // A shell variable outranks the file, so the change is a step by hand.
        let c = card(
            Harness::Claude,
            Some("xhigh"),
            "CLAUDE_CODE_SUBAGENT_EFFORT",
            "2.1.316",
        )
        .expect("a card");
        assert!(matches!(c.how, Some(How::Manual { .. })), "{:?}", c.how);

        // Before 2.1.316, Claude Code ignores it, so the card recommends upgrading.
        let c = card(Harness::Claude, Some("medium"), &variable, "2.1.315").expect("a card");
        assert!(!c.quiet && c.how.is_none(), "{:?}", c.how);
        assert!(
            c.text
                .contains("The latest session ran Claude Code 2.1.315, which ignores")
                && c.text.contains("Upgrade Claude Code to 2.1.316 or later"),
            "{}",
            c.text
        );
        assert!(c.sources.iter().any(|s| s == CLAUDE_CHANGELOG));
        // Past high, the edit stays on offer beside the upgrade.
        let c = card(Harness::Claude, Some("max"), &variable, "2.1.300").expect("a card");
        assert!(c.text.contains("ignores") && c.text.contains("prone to overthinking"));
        assert!(matches!(c.how, Some(How::Edit(_))), "{:?}", c.how);

        // A T3 instance's own environment entry is a step by hand naming its list.
        let entry = format!(
            "{ROOT}/t3/settings.json:providerInstances.claudeAgent.environment.CLAUDE_CODE_SUBAGENT_EFFORT"
        );
        let c = card(Harness::Claude, Some("max"), &entry, "2.1.316").expect("a card");
        let Some(How::Manual { step, .. }) = &c.how else {
            panic!("expected a step: {:?}", c.how)
        };
        assert_eq!(
            step,
            &format!(
                "set the CLAUDE_CODE_SUBAGENT_EFFORT entry in {ROOT}/t3/settings.json:providerInstances.claudeAgent.environment to high"
            )
        );

        // Codex: its own guidance, the edit in config.toml, and no version gate.
        let toml = format!("{file}:agents.default_subagent_reasoning_effort");
        let c = card(Harness::Codex, Some("xhigh"), &toml, "0.1.0").expect("a card");
        let text = said(&c);
        assert!(text.contains("OpenAI's reasoning guide"), "{text}");
        assert!(!text.contains("ignores"), "{text}");
        let Some(How::Edit(patch)) = c.how else {
            panic!("expected an edit: {:?}", c.how)
        };
        assert_eq!(
            patch.edits[0].path,
            ["agents", "default_subagent_reasoning_effort"]
        );

        // Profiles sharing the setting and model get one card, naming each that runs a
        // Claude Code too old to read it.
        let mut report = synthetic();
        report.checks.clear();
        report.context.clear();
        let mut other = crate::test_support::make_profile("other", ROOT);
        other.root = format!("{ROOT}/other");
        report.scan.profiles.push(other);
        for p in &mut report.scan.profiles {
            p.agents.effort = Some(Owned {
                value: "medium".into(),
                owner: variable.clone(),
            });
        }
        for r in &mut report.scan.requests {
            r.version = Some("2.1.316".into());
        }
        let mut old = report.scan.requests[0].clone();
        old.id = "other-request".into();
        old.profiles = vec!["other".into()].into();
        old.version = Some("2.1.315".into());
        report.scan.requests.push(old);
        let cards: Vec<Item> = advice(&report, ReportOptions::default())
            .into_iter()
            .filter(|i| i.name.starts_with("Subagent default"))
            .collect();
        assert_eq!(cards.len(), 1, "{cards:?}");
        assert!(
            cards[0]
                .text
                .contains("other's latest session ran Claude Code 2.1.315")
                && !cards[0].text.contains("ran Claude Code 2.1.316"),
            "{}",
            cards[0].text
        );

        // Profiles sharing the setting but not the subagent model get a card each, named by
        // profile, so neither's model is shown for the other.
        let mut report = synthetic();
        report.checks.clear();
        report.context.clear();
        let mut other = crate::test_support::make_profile("other", ROOT);
        other.root = format!("{ROOT}/other");
        report.scan.profiles.push(other);
        for (p, model) in report
            .scan
            .profiles
            .iter_mut()
            .zip(["claude-haiku-5-5", "claude-sonnet-5-5"])
        {
            p.agents.effort = Some(Owned {
                value: "max".into(),
                owner: variable.clone(),
            });
            p.agents.model = Some(Owned {
                value: model.into(),
                owner: "CLAUDE_CODE_SUBAGENT_MODEL".into(),
            });
        }
        let cards: Vec<Item> = advice(&report, ReportOptions::default())
            .into_iter()
            .filter(|i| i.name.starts_with("Subagent default"))
            .collect();
        assert_eq!(cards.len(), 2, "{cards:?}");
        assert_ne!(cards[0].name, cards[1].name);
        assert!(
            cards[0].text.contains("on claude-haiku-5-5")
                && cards[1].text.contains("on claude-sonnet-5-5")
        );

        // A profile whose subagent model can't be read gets its own card, keeping the note
        // and never claiming the main session's model.
        let mut report = synthetic();
        report.checks.clear();
        report.context.clear();
        let mut other = crate::test_support::make_profile("other", ROOT);
        other.root = format!("{ROOT}/other");
        report.scan.profiles.push(other);
        for p in &mut report.scan.profiles {
            p.agents.effort = Some(Owned {
                value: "max".into(),
                owner: variable.clone(),
            });
        }
        let why = "T3 instance claudeAgent sets CLAUDE_CODE_SUBAGENT_MODEL with a value token-max cannot read";
        report.scan.profiles[1].agents.unresolved = Some(why.into());
        let cards: Vec<Item> = advice(&report, ReportOptions::default())
            .into_iter()
            .filter(|i| i.name.starts_with("Subagent default"))
            .collect();
        assert_eq!(cards.len(), 2, "{cards:?}");
        assert_ne!(cards[0].name, cards[1].name);
        assert!(
            cards[0].text.contains("on the main session's model")
                && !cards[0].text.contains("cannot read"),
            "{}",
            cards[0].text
        );
        assert!(
            cards[1]
                .text
                .contains("on a model token-max cannot resolve")
                && cards[1].text.contains(&capitalize(why)),
            "{}",
            cards[1].text
        );
    }

    #[test]
    fn effort_lines_are_costs_and_vibes_suggests_the_maintainers_pick_with_the_guidance() {
        use crate::doctor::{Edit, Op, fixes};
        use crate::effort::{EffortInput, effort_checks};
        use serde_json::json;
        let file = format!("{ROOT}/settings.json");
        // One profile's line for a control beside the recorded pairs, its `vibes` card if any,
        // and whether `doctor` offers anything.
        let line = |harness: Harness, control: EffortControl| {
            let mut report = synthetic();
            report.checks.clear();
            report.context.clear();
            report.scan.profiles[0].harness = harness;
            report.scan.profiles[0].effort = Some(control);
            let checks = effort_checks(&EffortInput {
                scan: &report.scan,
                catalog: &prices(),
                since: None,
                now: NOV1 + DAY_MS,
            });
            report.checks = checks.into_iter().map(CheckResult::Effort).collect();
            let text = text(&report, None, true);
            // Its card is never listed as a setting to keep.
            assert!(
                text.contains("INFO  main-effort") || text.contains("ADVISORY  main-effort"),
                "{text}"
            );
            // The provider's guidance is `vibes`' card, and no part of the measured report.
            assert!(!text.contains("model guide") && !text.contains("reasoning guide"));
            let card = advice(&report, ReportOptions::default())
                .into_iter()
                .find(|i| i.name == "Main-session default");
            let line = report_line(&text, "Main-session effort");
            (line, card, fixes(&report, false).is_empty())
        };
        let control = |model: &str, level: &str, owner: &str| EffortControl {
            model: Some(model.into()),
            model_owner: format!("{file}:model"),
            level: Some(level.into()),
            owner: owner.into(),
            effective: Some(level.into()),
            lowered: None,
            caps: vec![],
            default: None,
            unresolved: None,
            picked: None,
            provider: None,
        };
        let owner = format!("{file}:env.CLAUDE_CODE_EFFORT_LEVEL");
        let toml = "/synthetic/home/.codex/config.toml";

        // Every known level is a cost with no verdict, never offered.
        for (harness, model, level) in [
            (Harness::Claude, "claude-opus-5-5", "max"),
            (Harness::Claude, "claude-opus-5-5", "xhigh"),
            (Harness::Claude, "claude-opus-5-5", "medium"),
            (Harness::Claude, "claude-opus-5", "low"),
            (Harness::Codex, "gpt-6.1-sol", "max"),
            (Harness::Codex, "gpt-6.1-sol", "ultra"),
        ] {
            let (row, _, none) = line(harness, control(model, level, &owner));
            assert_eq!(row.mark, "info", "{level}");
            assert!(
                row.evidence.contains("there is no verdict"),
                "{}",
                row.evidence
            );
            assert!(none, "{level} is offered");
        }
        let (row, warning, _) = line(Harness::Claude, control("claude-opus-5-5", "max", &owner));
        assert!(
            row.evidence
                .contains("claude-opus-5-5 at an unrecorded effort 4 times"),
            "{}",
            row.evidence
        );
        assert!(row.evidence.contains("Enterprise per-role cap"));
        // Off the pick, `vibes` suggests it, with each provider's own guidance past high.
        let said = |card: &Item| format!("{} {}", card.text, card.sources.join(" "));
        let card = warning.expect("max is off the pick");
        assert_eq!(
            card.value,
            "claude-opus-5-5 at max -> claude-opus-5-5 at medium"
        );
        assert!(!card.quiet);
        let warning = said(&card);
        assert!(
            warning.starts_with("Defaults to claude-opus-5-5 at max. The maintainer's pick is claude-opus-5-5 at medium, and claude-opus-5-5 at high is fine too. Anthropic's model guide says"),
            "{warning}"
        );
        assert!(warning.contains("prone to overthinking"));
        assert!(warning.contains("Ran claude-opus-5-5 at an unrecorded effort 4 times"));
        assert!(warning.contains("what the pick would cost is not measured"));
        assert!(warning.contains(&format!(
            "The maintainer's pick of a default. Anthropic's guidance: {CLAUDE_MODEL_GUIDE} {CLAUDE_EFFORT_DOCS}"
        )));
        // The edit sets the variable that decides the level, in the file that sets it.
        match card.how {
            Some(How::Edit(patch)) => assert_eq!(
                patch.edits,
                [Edit {
                    path: vec!["env".into(), "CLAUDE_CODE_EFFORT_LEVEL".into()],
                    value: json!("medium"),
                    op: Op::Set,
                }]
            ),
            other => panic!("{other:?}"),
        }
        let (_, warning, _) = line(Harness::Claude, control("claude-opus-5-5", "xhigh", &owner));
        assert!(said(&warning.expect("xhigh is off the pick")).contains("test the next model up"));
        // An accepted default has a quiet card naming the pick, with no change and no warning.
        let (_, warning, _) = line(Harness::Claude, control("claude-opus-5-5", "high", &owner));
        let card = warning.expect("an accepted default has a card");
        assert!(card.quiet && card.value.is_empty() && card.how.is_none());
        assert!(!said(&card).contains("model guide says"), "{}", said(&card));
        // Below high there is no provider guidance, but the pick is still suggested.
        let (_, warning, _) = line(Harness::Claude, control("claude-opus-5", "low", &owner));
        let card = warning.expect("low is off the pick");
        assert!(!card.quiet && !said(&card).contains("model guide says"));
        assert_eq!(
            card.value,
            "claude-opus-5 at low -> claude-opus-5-5 at medium"
        );
        // Codex cites OpenAI's guide, never Anthropic's.
        let (row, warning, _) = line(Harness::Codex, control("gpt-6.1-sol", "max", toml));
        let warning = said(&warning.expect("Codex max is off the pick"));
        assert!(
            warning.contains("OpenAI's reasoning guide keeps max"),
            "{warning}"
        );
        assert!(warning.contains(OPENAI_REASONING) && !warning.contains("Anthropic"));
        assert!(!row.evidence.contains("Enterprise"));
        let (_, warning, _) = line(Harness::Codex, control("gpt-6.1-sol", "ultra", toml));
        let card = warning.expect("ultra is off the pick");
        assert_eq!(card.value, "gpt-6.1-sol at ultra -> gpt-6.2-sol at high");
        // A Codex level with no model named or recorded is no default to weigh.
        let (_, warning, _) = line(
            Harness::Codex,
            EffortControl {
                model: None,
                ..control("gpt-6.1-sol", "xhigh", toml)
            },
        );
        assert!(warning.is_none());
        // Sol 6.2 at xhigh is accepted, so OpenAI's xhigh caution is not shown against it.
        let (_, warning, _) = line(Harness::Codex, control("gpt-6.2-sol", "xhigh", toml));
        let card = warning.expect("an accepted default has a card");
        assert!(card.quiet && !said(&card).contains("only when your evals"));

        // A level a cap lowered names the cap's owner when the cap owns it.
        let (row, _, _) = line(
            Harness::Claude,
            EffortControl {
                level: Some("high".into()),
                effective: Some("low".into()),
                ..control(
                    "claude-opus-5-5",
                    "low",
                    &format!("{file}:modelSettings.claude-opus-5-5.maxEffortLevel"),
                )
            },
        );
        assert!(
            row.evidence.ends_with(&format!(
                "Set in {file}:modelSettings.claude-opus-5-5.maxEffortLevel."
            )),
            "{}",
            row.evidence
        );

        // An unresolved default is unknown, and never a change.
        let (row, warning, none) = line(
            Harness::Claude,
            EffortControl {
                effective: None,
                default: None,
                unresolved: Some("T3 picks the model and effort for each thread".into()),
                ..control("claude-opus-5-5", "max", &owner)
            },
        );
        assert_eq!(row.mark, "unknown");
        assert!(
            row.evidence.starts_with("Not enough evidence: T3 picks"),
            "{}",
            row.evidence
        );
        // An unresolved default has no card.
        assert!(none && warning.is_none());

        // Two homes sharing a default but not its cap get a line, and a cap to raise, each.
        let mut report = synthetic();
        let capped = |id: &str| {
            let mut p = report.scan.profiles[0].clone();
            p.id = id.into();
            p.effort = Some(EffortControl {
                caps: vec![crate::model::Owned {
                    value: "low".into(),
                    owner: format!("/{id}/settings.json:maxEffortLevel"),
                }],
                ..control("claude-opus-5-5", "high", &owner)
            });
            p
        };
        report.scan.profiles = vec![capped("a"), capped("b")];
        let checks = effort_checks(&EffortInput {
            scan: &report.scan,
            catalog: &prices(),
            since: None,
            now: NOV1 + DAY_MS,
        });
        assert_eq!(checks.len(), 2);
    }

    #[test]
    fn tool_blocks_are_covered_by_a_server_block_which_leaves_them_no_line() {
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
        let rendered = text(&report, None, true);
        let line = report_line(&rendered, "MCP tool mcp__docs__write");
        assert_eq!(
            (line.mark.as_str(), line.head.as_str()),
            (
                "change",
                "MCP tool mcp__docs__write on -> off saves $5/mo <1%"
            )
        );
        assert!(line.evidence.contains("Included in its server's cost."));
        assert_eq!(
            line.setting,
            r#"mcp_servers.docs.disabled_tools = ["write"] (added to the list) in /synthetic/.codex/config.toml"#
        );
        let patch = changes(&report);
        assert!(patch.contains("disabled_tools = [\"write\"]"));
        assert!(!patch.contains("enabled = false"));
        // The server's exposure note is evidence: once in its line's evidence and once in
        // its card under details, and not at all by default.
        assert!(!text(&report, None, false).contains("Exposure:"));
        let (lines, cards) = rendered
            .split_once("\n  EVIDENCE\n")
            .expect("details have cards");
        assert_eq!(lines.matches("Exposure:").count(), 1);
        assert_eq!(cards.matches("Exposure:").count(), 1);
        assert!(lines.contains("\n  1 change recommended.\n"));
        assert!(!rendered.contains("Available checks favor current settings"));
        assert!(cards.find("MCP server docs:").unwrap() < cards.find("Exposure:").unwrap());

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
        let rendered = text(&conditional, None, true);
        let advisory: Vec<_> = report_lines(&rendered)
            .into_iter()
            .filter(|r| r.mark == "unknown")
            .collect();
        assert_eq!(advisory[0].head, "MCP server docs on costs $20 1%");
        assert_eq!(
            advisory[1].head,
            "MCP tool mcp__docs__write on costs $5 <1%"
        );
        assert!(advisory[..2].iter().all(|r| {
            r.evidence
                .contains("tier sensitivity could reverse keep or off")
        }));
        assert!(changes(&conditional).is_empty());
        let detailed = text(&conditional, None, true);
        assert_eq!(
            detailed.matches("conditional tool-tier estimate").count(),
            3
        );

        report.context[0].verdict = Some(Verdict::Off);
        let rendered = text(&report, None, true);
        let patch = changes(&report);
        assert!(patch.contains("enabled = false"));
        assert!(!patch.contains("disabled_tools"));
        // The server's off covers its tools, so they have no line of their own; details
        // still list each in the server's breakdown.
        let (lines, cards) = rendered
            .split_once("\n  EVIDENCE\n")
            .expect("details have cards");
        assert!(
            report_lines(lines)
                .iter()
                .all(|r| !r.head.starts_with("MCP tool mcp__")),
            "{lines}"
        );
        assert!(cards.contains("MCP tool mcp__docs__write (included in server total)"));
        assert!(!rendered.contains("Not enough evidence"));
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
    fn tools_without_their_own_switch_are_part_of_their_servers_line() {
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
        let rendered = text(&report, None, true);
        // A tool with no switch of its own is counted in its server's line, and only the
        // details' breakdown names it.
        let (lines, cards) = rendered
            .split_once("\n  EVIDENCE\n")
            .expect("details have cards");
        assert!(
            report_lines(lines)
                .iter()
                .all(|r| !r.head.starts_with("MCP tool mcp__docs__read")),
            "{lines}"
        );
        assert!(cards.contains("MCP tool mcp__docs__read (included in server total)"));
        assert!(cards.contains("No per-tool block is generated"));
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
        // Guidance is evidence: none by default, and once in the lines' evidence and once in
        // the cards under details.
        let plain = text(&report, None, false);
        assert!(!plain.contains("Claude MCP loading:"));
        assert!(!plain.contains("mcpServers.docs.alwaysLoad"));
        let details = text(&report, None, true);
        let (lines, cards) = details
            .split_once("\n  EVIDENCE\n")
            .expect("details have cards");
        assert!(
            report_line(lines, "ChatGPT apps")
                .evidence
                .contains("These transcripts do not establish that id mapping")
        );
        for rendered in [lines, cards] {
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
        }
    }

    #[test]
    fn instruction_files_cost_per_line_and_warn_over_the_providers_size_guidance() {
        let out = Out {
            text: String::new(),
            options: ReportOptions::default(),
            arrow: "->",
            home: None,
            profiles: IndexMap::default(),
            removed: Default::default(),
            spend: Spend {
                usd: 0.0,
                margin: None,
                floor: 0.0,
                partial: false,
                since: Some(0),
                until: 30 * DAY as Millis,
            },
        };
        let file = |harness, name: &str, bytes, lines| {
            let mut c = context_piece(ContextKind::Instructions, name, None, Some(4.0), None);
            c.harness = harness;
            c.instruction_size = Some(crate::model::InstructionSize { bytes, lines });
            c.hundred_lines = Some(crate::model::Estimate {
                usd: 4.0 * (100.0 / lines as f64).min(1.0),
                margin: None,
            });
            c
        };
        // Every file is a measured line with no verdict, priced per 100 lines.
        let claude = |lines| file(Harness::Claude, "/repo/CLAUDE.md", 100, lines);
        let row = instruction_row(&claude(200), &out);
        assert_eq!((row.mark, row.value.as_str()), (Mark::Info, "200 lines"));
        assert!(
            row.evidence.starts_with("Each 100 lines costs about $2/mo"),
            "{}",
            row.evidence
        );
        // A file now under 100 lines quotes what the whole file cost, whatever 100 lines
        // of its longer revisions cost.
        let mut short = claude(50);
        short.hundred_lines = Some(crate::model::Estimate {
            usd: 3.0,
            margin: None,
        });
        let row = instruction_row(&short, &out);
        assert!(
            row.evidence
                .starts_with("Its 50 lines cost about $4/mo, which removing them saves"),
            "{}",
            row.evidence
        );
        // The price of 100 lines comes from the revisions its requests carried, not from
        // the latest revision's length.
        let mut shortened = claude(100);
        shortened.hundred_lines = Some(crate::model::Estimate {
            usd: 1.0,
            margin: None,
        });
        let row = instruction_row(&shortened, &out);
        assert_eq!(row.value, "100 lines");
        assert!(
            row.evidence.starts_with("Each 100 lines costs about $1/mo"),
            "{}",
            row.evidence
        );
        // Claude's guidance is per CLAUDE.md, from 200 lines; other files it loads have none.
        let warning = instruction_warning(&claude(200), &[], "", &out).unwrap();
        assert!(
            warning
                .text
                .starts_with("200 lines; Claude Code suggests under 200"),
            "{}",
            warning.text
        );
        assert!(warning.text.contains("Each 100 lines costs about $2/mo"));
        assert!(instruction_warning(&claude(199), &[], "", &out).is_none());
        let other = file(Harness::Claude, "/repo/AGENTS.md", 100, 900);
        assert_eq!(instruction_row(&other, &out).mark, Mark::Info);
        assert!(instruction_warning(&other, &[], "", &out).is_none());
        // Rendered bytes count headers and wrappers, so only reaching the budget can mean truncation.
        let mut profile = crate::test_support::make_profile("profile", "/codex");
        let agents = |bytes| file(Harness::Codex, "AGENTS.md", bytes, 1);
        let warn = |c: &ContextCost, profiles: &[Profile], out: &Out<'_>| {
            instruction_warning(c, profiles, "", out).map(|w| w.text)
        };
        let profiles = std::slice::from_ref(&profile);
        assert_eq!(warn(&agents(32_767), profiles, &out), None);
        let default = warn(&agents(32_768), profiles, &out).unwrap();
        assert!(
            default.contains("32,768 bytes Codex loads by default"),
            "{default}"
        );
        assert!(
            default.contains("command-line override is not recorded"),
            "{default}"
        );
        // Profiles with different budgets give no warning.
        let mut other = profile.clone();
        other.id = "other".into();
        other
            .settings
            .insert("project_doc_max_bytes".into(), Scalar::Number(65536.0));
        let mut both = agents(40_000);
        both.profiles.push("other".into());
        assert_eq!(warn(&both, &[profile.clone(), other.clone()], &out), None);
        // The same budget from different sources names each of them.
        other
            .settings
            .insert("project_doc_max_bytes".into(), Scalar::Number(32768.0));
        other.instruction_budget_file = Some("/other/config.toml".into());
        let mixed = warn(&both, &[profile.clone(), other], &out).unwrap();
        assert!(
            mixed.contains("Codex loads by default and /other/config.toml sets"),
            "{mixed}"
        );
        // A configured budget names the file that sets it, not the default.
        profile
            .settings
            .insert("project_doc_max_bytes".into(), Scalar::Number(65536.0));
        profile.instruction_budget_file = Some("/etc/codex/config.toml".into());
        let profiles = std::slice::from_ref(&profile);
        assert_eq!(warn(&agents(40_000), profiles, &out), None);
        let set = warn(&agents(70_000), profiles, &out).unwrap();
        assert!(
            set.contains("65,536 bytes /etc/codex/config.toml sets"),
            "{set}"
        );
        assert!(!set.contains("default"), "{set}");
        // A project or named profile that can set it leaves no warning.
        profile.instruction_budget_unresolved =
            Some("/repo/.codex/config.toml can set project_doc_max_bytes for some runs".into());
        let profiles = std::slice::from_ref(&profile);
        assert_eq!(warn(&agents(70_000), profiles, &out), None);
        // With nothing priced, the line says only that no setting turns it off.
        let mut unpriced = claude(200);
        unpriced.estimated_usd = None;
        unpriced.hundred_lines = None;
        assert_eq!(
            instruction_row(&unpriced, &out).evidence,
            "No setting turns it off."
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
        // slash ok: the name given above is passed through
        assert_eq!(notes, ["~/CLAUDE.md: 200 recorded lines, 401 UTF-8 bytes."]);
        c.harness = Harness::Codex;
        c.instruction_size = Some(crate::model::InstructionSize {
            bytes: 32_769,
            lines: 1,
        });
        let mut profile = crate::test_support::make_profile("profile", "/codex");
        let notes = context_guidance(&c, std::slice::from_ref(&profile), "AGENTS.md");
        assert_eq!(
            notes,
            [
                "AGENTS.md: 1 recorded line, 32,769 UTF-8 bytes, including any headers or wrappers. Codex reads up to 32,768 bytes of its source files, before headers and wrappers (project_doc_max_bytes)."
            ]
        );
        profile
            .settings
            .insert("project_doc_max_bytes".into(), Scalar::Number(65536.0));
        let notes = context_guidance(&c, &[profile], "AGENTS.md");
        assert!(
            notes[0].ends_with("Codex reads up to 65,536 bytes of its source files, before headers and wrappers (project_doc_max_bytes).")
        );
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
        let text = text(&report, None, true);
        let line = |head: &str| report_line(&text, head);
        assert_eq!(
            line("ChatGPT apps").head,
            "ChatGPT apps on -> off saves $30/mo 2%"
        );
        assert!(
            line("ChatGPT apps").setting == "[/synthetic/.codex/config.toml] features.apps = false",
            "{text}"
        );
        assert!(
            line("ChatGPT apps")
                .evidence
                .starts_with("Never used; it cost $30 over the window. Turn off: features.apps = false. Per-app controls:")
        );
        // One line per cap setting, with tools no cap governs together, ranked by cost.
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
                    "Tool output unset costs $50 3%".to_string(),
                    format!(
                        "{cap} Its largest result was 112,000 characters. tool_output_token_limit is unset."
                    )
                ),
                (
                    "unknown".into(),
                    "Bash output unset costs $20 1%".into(),
                    format!(
                        "{cap} Its largest result was 30,500 characters. bashOutputMaxChars is unset, so about 30,000 characters."
                    )
                ),
                (
                    "info".into(),
                    "Other tool output costs $8 1%".into(),
                    "From Read. Its largest result was 2,000 characters. No cap is checked for it."
                        .into()
                ),
                (
                    "unknown".into(),
                    "MCP output 50,000 tokens costs $6 <1%".into(),
                    format!(
                        "{cap} Its largest result was 900 characters. MAX_MCP_OUTPUT_TOKENS is 50,000 tokens."
                    )
                ),
            ]
        );
        // Each cap line names its setting.
        assert_eq!(
            report_line(&text, "Bash output").setting,
            "[/synthetic/home/.claude/settings.json] bashOutputMaxChars"
        );

        // Codex switches go to config.toml as TOML.
        let snippet = changes(&report);
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
        let text_unwrapped = |details| text(&report, None, details);
        for (width, details) in [28, 40, 80]
            .into_iter()
            .flat_map(|w| [(w, false), (w, true)])
        {
            let text = text(&report, Some(width), details);
            let at = |s: &str| text.find(s).expect(s);
            if details {
                assert!(at("Recommended changes") < at("Advisory findings"));
                assert!(at("Advisory findings") < at("Keep current settings"));
            }
            for line in text.lines() {
                assert!(
                    unicode_width::UnicodeWidthStr::width(line) <= width,
                    "{line}"
                );
            }
            let compact = compact(&text);
            // Wrapping only moves line breaks, so nothing is dropped or repeated.
            assert_eq!(compact, super::tests::compact(&text_unwrapped(details)));
            if details {
                for value in &values {
                    assert!(compact.contains(&super::tests::compact(value)), "{value}");
                }
                assert!(compact.contains("$12.34") && compact.contains("$24.68"));
            }
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
        // The words lines carry, without a line's leading verdict word or dot. Color indents
        // continuation lines less, so lines wrap differently and only the words compare.
        let words = |text: &str| {
            text.lines()
                .filter(|l| !l.ends_with("no\u{a0}verdict"))
                .flat_map(|l| {
                    // A continuation line is indented past the verdict, so a word such as
                    // "unknown" that opens it is text.
                    let continued = l.len() - l.trim_start().len() >= WORD_CELLS;
                    let l = l.trim_start().replace('→', "->");
                    let l = ["● ", "○ ", "* ", "o "]
                        .iter()
                        .find_map(|d| l.strip_prefix(d))
                        .unwrap_or(&l);
                    ["change", "consider", "keep", "unknown", "info"]
                        .iter()
                        .filter(|_| !continued)
                        .find_map(|w| l.strip_prefix(w).filter(|r| r.starts_with(' ')))
                        .unwrap_or(l)
                        .split_whitespace()
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        // Details color their lines as the summary does, and only add styles to the same
        // words in the evidence cards.
        let plain = text(&report, Some(120), true);
        let styled = rendered(
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
        let styled = unruled(&strip_ansi(&styled));
        let (styled_lines, styled_cards) = styled.split_once("\n  EVIDENCE\n").unwrap();
        let (plain_lines, plain_cards) = plain.split_once("\n  EVIDENCE\n").unwrap();
        assert_eq!(styled_cards, plain_cards);
        assert_eq!(words(styled_lines), words(plain_lines));
        // The summary's color replaces each verdict word with a dot in that color,
        // dim and hollow when there is no verdict, and adds a legend.
        let styled = rendered(
            &report,
            ReportOptions {
                color: true,
                unicode: true,
                ..Default::default()
            },
        );
        let plain = text(&report, None, false);
        // A line with a verdict colors only its dot; one without is dim throughout.
        for (dot, line) in [
            ("\x1b[31m●\x1b[0m ", "Artifact tool"),
            ("\x1b[33m●\x1b[0m ", "Skill example"),
            ("\x1b[32m●\x1b[0m ", "Agent tool"),
            ("\x1b[2m○ ", "Workflow tool"),
            ("\x1b[2m○ ", "System prompt and core tools"),
            ("\x1b[32m●\x1b[0m ", "Fast mode"),
        ] {
            assert!(
                styled.contains(&format!("{dot}{line} ")),
                "{line}: {styled}"
            );
        }
        assert!(styled.contains("\x1b[31m●\u{a0}\x1b[0m\x1b[2mchange"));
        // The legend wraps between its parts on a narrow terminal.
        let narrow = rendered(
            &report,
            ReportOptions {
                width: Some(40),
                color: true,
                ..Default::default()
            },
        );
        let legend = &narrow[narrow.find("\x1b[31m*\u{a0}\x1b[0m").unwrap()..];
        let legend: Vec<&str> = legend.lines().take(2).collect();
        assert!(
            legend.iter().all(|l| cells(&strip_ansi(l)) <= 40),
            "{legend:?}"
        );
        assert!(legend[1].contains("no\u{a0}verdict"), "{legend:?}");
        // A heading's rule stays on its row, or is left out where it doesn't fit.
        let narrow = strip_ansi(&narrow);
        assert!(
            narrow.contains("\n  CHANGES  biggest saving first ---"),
            "{narrow}"
        );
        assert!(
            narrow.contains("\n  EVERYTHING ELSE  biggest cost first\n"),
            "{narrow}"
        );
        assert!(
            narrow
                .lines()
                .all(|l| l.trim().is_empty() || !l.trim().chars().all(|c| c == '-')),
            "{narrow}"
        );
        assert_eq!(words(&unruled(&strip_ansi(&styled))), words(&plain));
        assert_eq!(
            text(&report, None, true)
                .matches("Identical fixed costs remain unknown")
                .count(),
            2
        );
    }

    #[test]
    fn partly_priced_pieces_show_a_tilde_and_their_margin() {
        let mut report = synthetic();
        let margins = [
            ("Artifact", Margin::Usd(4.0)),
            ("Agent", Margin::Unmeasured),
            ("Claude Code system prompt", Margin::Usd(2.0)),
            ("Bash", Margin::Usd(1.0)),
        ];
        for (name, margin) in margins {
            let c = report.context.iter_mut().find(|c| c.name == name).unwrap();
            c.margin = Some(margin);
        }
        // A fixed piece nothing priced leaves the sum a lower bound.
        report.context.push(context_piece(
            ContextKind::Tool,
            "Read",
            None,
            None,
            Some(3),
        ));
        let text = text(&report, None, true);
        let artifact = report_line(&text, "Artifact tool");
        assert_eq!(
            artifact.head,
            "Artifact tool on -> off saves ~$90/mo ± $4 6%"
        );
        assert!(
            artifact
                .evidence
                .starts_with("Never used; it cost ~$90 ± $4 over the window."),
            "{}",
            artifact.evidence
        );
        // The per-use figure is a share of the one beside it, which carries the margin.
        let agent = report_line(&text, "Agent tool");
        assert_eq!(agent.head, "Agent tool on costs ~$40, margin unknown 3%");
        assert!(agent.evidence.starts_with(
            "Used 310 times, $0.13 each; it cost ~$40, margin unknown over the window."
        ));
        // Pieces on the same requests add their margins.
        let core = report_line(&text, "System prompt and core tools");
        assert_eq!(
            core.head,
            "System prompt and core tools costs at least ~$213 ± $3 14%"
        );
        assert!(core.evidence.starts_with("1 piece could not be priced. "));
        assert!(text.contains("A ~ marks a figure extrapolated"));
        // `doctor` offers the change with the same margin.
        let offered = recommendations(&report, false);
        let artifact = offered
            .iter()
            .find(|r| r.name.starts_with("Artifact"))
            .unwrap();
        assert_eq!(artifact.margin, Some(Margin::Usd(4.0)));
    }

    #[test]
    fn subagent_lines_are_evidence_that_never_count_as_changes() {
        let mut report = synthetic();
        let before = text(&report, None, false);
        let cost = |usd: f64| crate::prices::known_cost(Usd::from_f64(usd));
        report.subagents.push(SubagentCost {
            harness: Harness::Claude,
            profiles: vec!["profile".into()],
            kind: "Explore".into(),
            native: false,
            fork: false,
            built_in: true,
            runs: 3,
            requests: 10,
            priced: 9,
            cost: Cost {
                total_usd: None,
                ..cost(2.0)
            },
            estimate: Some(Estimate {
                usd: 2.2,
                margin: Some(Margin::Usd(0.3)),
            }),
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
            reprice_estimate: Some(Estimate {
                usd: 0.5,
                margin: None,
            }),
            model_setting: "model: in /synthetic/.claude/agents/Explore.md".into(),
            effort_setting: "effort: in /synthetic/.claude/agents/Explore.md".into(),
            replacement: Replacement {
                missing: vec![ROOT_AGENTS.into()],
                ..Replacement::default()
            },
        });
        let summary = text(&report, None, true);
        let first = |text: &str| {
            text.lines()
                .find(|l| l.contains("recommended"))
                .map(str::to_string)
        };
        assert_eq!(first(&summary), first(&before));
        let row = report_line(&summary, "Explore subagent");
        assert_eq!(
            (row.mark.as_str(), row.head.as_str()),
            ("info", "Explore subagent costs ~$2.20 ± $0.30 <1%")
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
        // The model guide's tip is `vibes`' cited card, apart from the measured report.
        assert!(!summary.contains("Anthropic's model guide"), "{summary}");
        let cards = advice(&report, ReportOptions::default());
        let card = cards.iter().find(|i| i.name == "Explore subagent").unwrap();
        assert_eq!(
            card.text,
            "Ran on claude-opus-5-5 (90%), claude-haiku-4-5 (10%) and cost ~$2.20 ± $0.30. On claude-haiku-5-5 the same recorded tokens would cost $0.50, assuming the same turns. Anthropic's model guide notes that the built-in Explore runs on the session's model, and suggests a user agent named Explore with model: haiku to keep exploration cheap."
        );
        assert_eq!(card.sources, [CLAUDE_MODEL_GUIDE, CLAUDE_SUBAGENT_DOCS]);
        assert_eq!(card.value, "inherit -> haiku");
        assert!(!card.quiet);
        // The file replaces a system prompt Anthropic doesn't publish, so it is never written.
        let Some(How::Manual { step, reason }) = &card.how else {
            panic!("{:?}", card.how)
        };
        assert_eq!(
            step,
            "add a new .md agent file to /synthetic/home/.claude/agents with the frontmatter name: Explore; description: when to use it; model: haiku; disallowedTools: Write, Edit; omitClaudeMd: true; and a system prompt of your own below it"
        );
        assert!(reason.contains("doesn't publish"), "{reason}");
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
    fn fable_lines_show_weekly_spend_and_warn_with_the_plan_rule() {
        let mut report = synthetic();
        let before = text(&report, None, false);
        let cost = |known: f64, total: Option<f64>| Cost {
            known_usd: Usd::from_f64(known),
            total_usd: total.map(Usd::from_f64),
            unknown: vec![],
            assumptions: vec![],
        };
        let day = 86_400_000;
        let end = NOV1 + 30 * day;
        report.fable.push(FableUse {
            profiles: vec!["profile".into()],
            requests: 5,
            unrecorded: 2,
            shared: 1,
            weeks: vec![
                FableWeek {
                    start: end - 7 * day,
                    end,
                    requests: 4,
                    cost: cost(412.4, Some(412.4)),
                    estimate: Some(Estimate {
                        usd: 412.4,
                        margin: None,
                    }),
                },
                FableWeek {
                    start: end - 10 * day,
                    end: end - 7 * day,
                    requests: 1,
                    cost: cost(3.0, None),
                    estimate: None,
                },
            ],
            estimate: Some(Estimate {
                usd: 515.5,
                margin: Some(Margin::Usd(40.0)),
            }),
        });
        let summary = text(&report, None, true);
        let first = |text: &str| {
            text.lines()
                .find(|l| l.contains("recommended"))
                .map(str::to_string)
        };
        assert_eq!(first(&summary), first(&before));
        let row = report_line(&summary, "Fable weekly use");
        assert_eq!(
            (row.mark.as_str(), row.head.as_str()),
            ("info", "Fable weekly use costs ~$516 ± $40 34%")
        );
        for part in [
            "5 Fable requests at API prices, per 7 days back from this report: $412 (4 requests, to 2026-12-01), an unknown amount (1 request, 3 days to 2026-11-24).",
            "no share of it is shown",
            "they cover only this profile's recorded sessions",
            "paid with usage credits after the cap was reached",
            "1 of them is also attributed to another Claude profile that signs in to Anthropic and reads the same transcripts, so whose they were is unknown.",
            "2 requests recorded no model, for the request or an advisor call, and may have run on Fable.",
        ] {
            assert!(row.evidence.contains(part), "{part} in {}", row.evidence);
        }
        // The plan rule is a cited `vibes` card apart from the line, which ends with its setting.
        assert!(
            row.evidence
                .ends_with("reached. The weekly reset time is not recorded, and the limit is not published in tokens or dollars, so no share of it is shown. 1 of them is also attributed to another Claude profile that signs in to Anthropic and reads the same transcripts, so whose they were is unknown. 2 requests recorded no model, for the request or an advisor call, and may have run on Fable. Model setting: /model, or model in settings.json."),
            "{}",
            row.evidence
        );
        assert!(!row.evidence.contains("half the weekly usage limit"));
        assert!(
            !summary.contains("half the weekly usage limit"),
            "{summary}"
        );
        let cards = advice(&report, ReportOptions::default());
        let card = cards.iter().find(|i| i.name == "Fable weekly use").unwrap();
        assert_eq!(
            card.text,
            "Cost ~$516 ± $40 at API prices over 5 Fable requests from 2026-11-21 to 2026-12-01. These figures are partial: the cap counts Fable use on every Claude surface, they cover only this profile's recorded sessions, and they may include requests paid with usage credits after the cap was reached. The weekly reset time is not recorded, and the limit is not published in tokens or dollars, so no share of it is shown. 1 of them is also attributed to another Claude profile that signs in to Anthropic and reads the same transcripts, so whose they were is unknown. 2 requests recorded no model, for the request or an advisor call, and may have run on Fable. On a Claude Max or Ultra plan or a Team or seat-based Enterprise premium seat, Fable models can use up to half the weekly usage limit, and use it faster than other models; on Pro or a Team or seat-based Enterprise standard seat, they run on usage credits; on usage-based Enterprise, they bill at API rates."
        );
        assert_eq!(card.sources, [FABLE_PLANS]);
        // --details keeps each span's cost with its unknowns, the caveats and the sources.
        let details = text(&report, None, true);
        for part in [
            "Fable weekly use",
            "to 2026-12-01, 4 requests",
            "3 days to 2026-11-24, 1 request",
            "they cover only this profile's recorded sessions",
            "1 of them is also attributed to another Claude profile",
            CLAUDE_PRICING,
        ] {
            assert!(details.contains(part), "{part} in {details}");
        }
        report.fable[0].requests = 0;
        report.fable[0].unrecorded = 0;
        report.fable[0].shared = 0;
        report.fable[0].weeks.clear();
        let summary = text(&report, None, true);
        let row = report_line(&summary, "Fable weekly use");
        assert_eq!(row.head, "Fable weekly use");
        assert_eq!(
            row.evidence,
            "No Fable requests recorded in the window. Model setting: /model, or model in settings.json."
        );
        // With no Fable use, there is nothing for the plan rule to advise about.
        let cards = advice(&report, ReportOptions::default());
        assert!(
            !cards.iter().any(|i| i.name.starts_with("Fable")),
            "{cards:?}"
        );
    }

    #[test]
    fn t3_and_shared_advisor_lines_name_whose_calls_they_count() {
        let mut report = synthetic();
        let mut t3 = crate::test_support::make_t3_profile("t3", "/synthetic/t3");
        t3.settings_file = "/synthetic/t3/settings.json".into();
        report.scan.profiles.push(t3);
        let used = |line: AdvisorLine, profiles: &[&str], usd: f64| AdvisorUse {
            profiles: profiles.iter().map(|p| p.to_string()).collect(),
            line,
            requests: 1,
            calls: 2,
            models: vec![Share {
                name: "claude-opus-5-5".into(),
                count: 2,
            }],
            cost: Cost::default(),
            estimate: Some(Estimate { usd, margin: None }),
            shared: 0,
        };
        report.advisor = vec![
            used(
                AdvisorLine::T3 {
                    owner: "/synthetic/t3/settings.json:advisorModel".into(),
                },
                &["t3"],
                1.5,
            ),
            used(
                AdvisorLine::Shared {
                    owners: vec![
                        format!("{ROOT}/settings.json:advisorModel"),
                        "/synthetic/t3/settings.json:advisorModel".into(),
                    ],
                },
                &["profile", "t3"],
                0.5,
            ),
        ];
        let details = text(&report, None, true);
        let lines: Vec<_> = report_lines(&details)
            .into_iter()
            .filter(|l| l.head.starts_with("Advisor model"))
            .collect();
        assert_eq!(lines.len(), 2, "{details}");
        assert!(
            lines[0]
                .evidence
                .contains("T3 Code threads made these calls"),
            "{}",
            lines[0].evidence
        );
        assert!(
            lines[1].evidence.contains("so whose they were is unknown"),
            "{}",
            lines[1].evidence
        );
        assert!(details.contains("shared, profile attribution unknown"));
    }

    #[test]
    fn the_advisor_card_suggests_one_rank_up_and_the_line_prices_recorded_calls() {
        let mut report = synthetic();
        let file = format!("{ROOT}/settings.json");
        let set = |report: &mut Report,
                   model: Option<&str>,
                   blocked: Option<&str>,
                   main: Option<&str>| {
            let p = &mut report.scan.profiles[0];
            p.settings_file = file.clone();
            p.advisor = Some(AdvisorControl {
                model: model.map(Into::into),
                owner: format!("{file}:advisorModel"),
                blocked: blocked.map(Into::into),
                gateway: None,
            });
            p.effort = Some(crate::model::EffortControl {
                model: main.map(Into::into),
                model_owner: format!("{file}:model"),
                level: None,
                owner: String::new(),
                effective: None,
                lowered: None,
                caps: vec![],
                default: None,
                unresolved: main.is_none().then(|| "no setting names the model".into()),
                picked: None,
                provider: None,
            });
        };
        report.advisor.push(AdvisorUse {
            profiles: vec!["profile".into()],
            line: AdvisorLine::Settings,
            shared: 0,
            requests: 0,
            calls: 0,
            models: vec![],
            cost: Cost::default(),
            estimate: Some(Estimate {
                usd: 0.0,
                margin: None,
            }),
        });
        let card = |report: &Report| {
            advice(report, ReportOptions::default())
                .into_iter()
                .find(|i| i.name.starts_with("Advisor model"))
                .unwrap()
        };
        // A Sonnet main gets Opus, offered as one edit to the file that would set it.
        set(&mut report, None, None, Some("claude-sonnet-5-5"));
        let sonnet = card(&report);
        assert_eq!(
            (sonnet.value.as_str(), sonnet.quiet),
            ("none -> opus", false)
        );
        assert!(
            sonnet.text.contains("Opus ranks one above claude-sonnet-5-5, the main-session default (/synthetic/home/.claude/settings.json:model)."),
            "{}",
            sonnet.text
        );
        assert!(
            sonnet
                .text
                .contains("No advisor call is recorded in the window.")
        );
        assert!(!sonnet.text.contains("Fable access"));
        assert_eq!(sonnet.sources[1], crate::advisor::DOCS);
        match &sonnet.how {
            Some(How::Edit(patch)) => {
                assert_eq!(patch.file, file);
                assert_eq!(
                    patch
                        .edits
                        .iter()
                        .map(|e| e.text(Harness::Claude))
                        .collect::<Vec<_>>(),
                    ["\"advisorModel\": \"opus\""]
                );
            }
            other => panic!("{other:?}"),
        }
        // A main model a project sets, in `model` or its env, gets no edit: the user's file
        // would pair the advisor with every project's model. The user's own env does.
        let project = "/synthetic/repo/.claude/settings.json";
        for key in ["model", "env.ANTHROPIC_MODEL"] {
            report.scan.profiles[0].effort.as_mut().unwrap().model_owner =
                format!("{project}:{key}");
            let c = card(&report);
            assert_eq!((c.how, c.quiet), (None, false));
            assert!(
                c.text.contains(&format!(
                    "{project} sets that main model for its project only"
                )),
                "{}",
                c.text
            );
        }
        report.scan.profiles[0].effort.as_mut().unwrap().model_owner =
            format!("{file}:env.ANTHROPIC_MODEL");
        let c = card(&report);
        assert!(matches!(c.how, Some(How::Edit(_))));
        assert!(c.text.contains(
            "A project whose own settings pick another main model takes this advisor too"
        ));
        // A model the latest session ran, with no model set or a null one, still gets the
        // edit, saying a session's own choice may run another model.
        for owner in [
            crate::effort::RECORDED.to_string(),
            format!("{project}:model{}", crate::effort::NULL_MODEL),
        ] {
            report.scan.profiles[0].effort.as_mut().unwrap().model_owner = owner;
            let c = card(&report);
            assert!(matches!(c.how, Some(How::Edit(_))), "{:?}", c.how);
            assert_eq!((c.quiet, c.value.as_str()), (false, "none -> opus"));
            assert!(
                c.text.contains("No setting names the main model, so a session's /model or --model choice may run another one")
                    && c.text.contains("A project whose own settings pick another main model"),
                "{}",
                c.text
            );
        }
        // An Opus main gets Fable, with what Fable needs.
        set(&mut report, None, None, Some("claude-opus-5-5"));
        let opus = card(&report);
        assert_eq!(opus.value, "none -> fable");
        assert!(opus.text.contains("Fable access"), "{}", opus.text);
        // No edit is offered when none applies: a Fable main, an old one, an unknown one,
        // or an advisor that would not load.
        for (main, blocked, says) in [
            (
                Some("claude-fable-5-2"),
                None,
                "nothing ranks above claude-fable-5-2",
            ),
            (
                Some("claude-sonnet-4-5"),
                None,
                "attaches none to claude-sonnet-4-5",
            ),
            (None, None, "unknown, since no setting names the model."),
            (
                Some("claude-sonnet-5-5"),
                Some(
                    "DISABLE_TELEMETRY, set in the environment, turns off the feature-flag fetch the advisor needs",
                ),
                "since none would load: DISABLE_TELEMETRY",
            ),
        ] {
            set(&mut report, None, blocked, main);
            let c = card(&report);
            assert!(c.text.contains(says), "{says} in {}", c.text);
            assert_eq!((c.how, c.quiet, c.value.as_str()), (None, true, ""));
        }
        // Unset and never called, doctor has no line for it.
        assert!(!text(&report, None, false).contains("Advisor model"));
        // Set and called: doctor's line gives the calls' cost with no verdict, and the card
        // advises nothing unless something stops the advisor loading.
        report.advisor[0] = AdvisorUse {
            profiles: vec!["profile".into()],
            line: AdvisorLine::Settings,
            shared: 0,
            requests: 3,
            calls: 4,
            models: vec![Share {
                name: "claude-opus-5-5".into(),
                count: 4,
            }],
            cost: Cost::default(),
            estimate: Some(Estimate {
                usd: 2.5,
                margin: None,
            }),
        };
        set(&mut report, Some("opus"), None, Some("claude-sonnet-5-5"));
        let details = text(&report, None, true);
        // --details names the calls and the cost behind the line.
        assert!(
            details.contains("Calls: 4 advisor calls on 3 requests"),
            "{details}"
        );
        let row = report_line(&details, "Advisor model");
        assert_eq!(row.mark, "info");
        assert!(
            row.head == "Advisor model opus costs $2.50 <1%",
            "{}",
            row.head
        );
        assert_eq!(
            row.evidence,
            "4 advisor calls on 3 requests ran on claude-opus-5-5, costing $2.50 at API prices over the window. Set to opus."
        );
        let c = card(&report);
        assert_eq!((c.how, c.quiet), (None, true));
        assert!(
            c.text.starts_with(
                "Set to opus in /synthetic/home/.claude/settings.json. 4 advisor calls"
            )
        );
        // A set advisor the main model can't take is advice with no edit, naming where the
        // main model comes from.
        for (advisor, main, says) in [
            (
                "opus",
                "claude-sonnet-4-5",
                "but Claude Code attaches no advisor to claude-sonnet-4-5, the main-session default (/synthetic/home/.claude/settings.json:model).",
            ),
            (
                "opus",
                "claude-fable-5-2",
                "pairs claude-fable-5-2, the main-session default (/synthetic/home/.claude/settings.json:model), only with a Fable advisor of its version or later.",
            ),
            (
                "claude-fable-5-1",
                "claude-fable-5-2",
                "only with a Fable advisor of its version or later.",
            ),
            (
                "claude-sonnet-5-5",
                "claude-opus-5-5",
                "but Claude Code's pairing table doesn't take it as an advisor for claude-opus-5-5",
            ),
        ] {
            set(&mut report, Some(advisor), None, Some(main));
            let c = card(&report);
            assert!(c.text.contains(says), "{says} in {}", c.text);
            assert_eq!((c.how, c.quiet), (None, false));
        }
        // A pairing the table lists is quiet, as is an alias, whose version moves with
        // Claude Code, except under a Fable main, which takes the Fable alias alone.
        for (advisor, main) in [
            ("opus", "claude-opus-5-5"),
            ("sonnet", "claude-opus-5-5"),
            ("claude-opus-5", "claude-opus-5-5"),
            ("fable", "claude-fable-5-2"),
        ] {
            set(&mut report, Some(advisor), None, Some(main));
            assert!(card(&report).quiet, "{advisor} over {main}");
        }
        set(
            &mut report,
            Some("opus"),
            Some(
                "CLAUDE_CODE_DISABLE_ADVISOR_TOOL, set in the environment, turns the advisor tool off",
            ),
            Some("claude-sonnet-5-5"),
        );
        let c = card(&report);
        assert_eq!((c.how, c.quiet), (None, false));
        assert!(
            c.text.contains(
                "but none would load from these settings: CLAUDE_CODE_DISABLE_ADVISOR_TOOL"
            )
        );
        // Profiles reading one settings file share a card, unless their main-session
        // defaults differ.
        set(&mut report, None, None, Some("claude-sonnet-5-5"));
        let mut other = report.scan.profiles[0].clone();
        other.id = "other".into();
        report.scan.profiles.push(other);
        let cards = |report: &Report| {
            advice(report, ReportOptions::default())
                .into_iter()
                .filter(|i| i.name.starts_with("Advisor model"))
                .map(|i| i.value)
                .collect::<Vec<_>>()
        };
        assert_eq!(cards(&report), ["none -> opus"]);
        report.scan.profiles[1].effort.as_mut().unwrap().model = Some("claude-opus-5-5".into());
        assert_eq!(cards(&report), ["none -> opus", "none -> fable"]);
        report.scan.profiles.pop();
        // A T3 instance reading the same file takes the advisor under its own thread models.
        assert!(!card(&report).text.contains("T3 Code threads"));
        let mut t3 = crate::test_support::make_t3_profile("t3", ROOT);
        t3.settings_file = file.clone();
        report.scan.profiles.push(t3);
        let c = card(&report);
        assert!(
            c.text.contains(&format!(
                "T3 Code threads read {file} too, and T3 picks each thread's main model"
            )),
            "{}",
            c.text
        );
        assert!(!c.text.contains("launch arguments"), "{}", c.text);
        assert!(matches!(c.how, Some(How::Edit(_))));
        // An instance whose launch settings are unresolved may not read the file at all.
        report.scan.profiles[1]
            .t3
            .as_mut()
            .unwrap()
            .settings_unresolved = true;
        let c = card(&report);
        assert!(
            c.text
                .contains("would use it, unless an instance's launch arguments"),
            "{}",
            c.text
        );
        // A disabled instance runs no threads.
        report.scan.profiles[1].t3.as_mut().unwrap().enabled = false;
        assert!(!card(&report).text.contains("T3 Code threads"));
    }

    #[test]
    fn a_symlink_to_the_user_settings_file_is_not_a_project_file() {
        let w = crate::test_support::Workspace::new();
        std::fs::write(w.claude.join("settings.json"), "{}").unwrap();
        let user = w.claude.join("settings.json").display().to_string();
        let project = |dir: &std::path::Path| {
            format!("{}:model", dir.join(".claude/settings.json").display())
        };
        // The latest session ran in a directory that links to the home, so its project file
        // is the user's.
        #[cfg(unix)]
        {
            let link = w.home.join("link");
            std::os::unix::fs::symlink(&w.home, &link).unwrap();
            assert_eq!(project_file(&project(&link), &user), None);
        }
        let repo = w.home.join("repo");
        std::fs::create_dir_all(repo.join(".claude")).unwrap();
        std::fs::write(repo.join(".claude/settings.json"), "{}").unwrap();
        assert!(project_file(&project(&repo), &user).is_some());
    }

    #[test]
    fn code_intelligence_cards_sharing_a_settings_file_name_their_plugins_directory() {
        use crate::lsp::{LanguageUse, Lsp, LspUse};
        let mut report = synthetic();
        let card = |plugins_dir: &str| LspUse {
            profiles: vec!["profile".into()],
            sessions: 1,
            languages: vec![LanguageUse {
                lsp: Lsp::of("main.rs").unwrap(),
                sessions: 1,
                shared: 0,
                projects: vec![],
                installed: false,
                installed_for: vec![],
                off: None,
            }],
            settings_file: format!("{ROOT}/settings.json"),
            root: ROOT.into(),
            plugins_dir: Some(plugins_dir.into()),
            cache_dir: None,
            named: false,
            unresolved: false,
            denied: None,
            unread: None,
        };
        report.lsp = vec![card(&format!("{ROOT}/plugins"))];
        let names = |report: &Report| -> Vec<String> {
            advice(report, ReportOptions::default())
                .into_iter()
                .map(|i| i.name)
                .filter(|n| n.contains("code intelligence"))
                .collect()
        };
        let plain = names(&report);
        assert_eq!(plain.len(), 1);
        assert!(!plain[0].contains("plugins in"), "{plain:?}");
        // A T3 instance split off for its unresolved settings shares the directory: no tag.
        report.lsp.push(card(&format!("{ROOT}/plugins")));
        assert!(names(&report).iter().all(|n| !n.contains("plugins in")));
        report.lsp[1] = card("/synthetic/cache");
        let both = names(&report);
        assert_eq!(both.len(), 2);
        assert_ne!(both[0], both[1]);
        assert!(
            both[1].ends_with("(plugins in /synthetic/cache)"),
            "{both:?}"
        );

        // A `doctor` line denying the LSP tool in the card's settings gives it an after-state
        // that says the plugin would give only diagnostics; one for another file doesn't.
        report.lsp.truncate(1);
        let deny = |file: String| crate::doctor::Fix {
            name: "LSP".into(),
            value: "on -> off".into(),
            suggested: false,
            saving: None,
            margin: None,
            verb: "saves",
            share: None,
            what: String::new(),
            evidence: String::new(),
            origins: vec![super::Origin::Check(7)],
            group: None,
            removal: vec![],
            how: How::Edit(crate::doctor::Patch {
                file,
                harness: Harness::Claude,
                edits: vec![crate::doctor::Edit {
                    path: vec!["permissions".into(), "deny".into()],
                    value: serde_json::Value::String("LSP".into()),
                    op: crate::doctor::Op::Append,
                }],
            }),
        };
        let card = |fixes: &[crate::doctor::Fix]| {
            format_report(&report, fixes, ReportOptions::default())
                .vibes
                .into_iter()
                .find(|i| i.name.contains("code intelligence"))
                .unwrap()
        };
        let after = card(&[deny(format!("{ROOT}/settings.json"))]).after;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].origins, [super::Origin::Check(7)]);
        assert!(
            after[0].item.text.contains(" turns off the LSP tool,"),
            "{}",
            after[0].item.text
        );
        assert!(
            card(&[deny("/synthetic/other.json".into())])
                .after
                .is_empty()
        );
        // A deny written through a link to the card's settings file is the same file.
        #[cfg(unix)]
        {
            let w = crate::test_support::Workspace::new();
            let settings = w.home.join("settings.json");
            std::fs::write(&settings, "{}").unwrap();
            let link = w.home.join("linked.json");
            std::os::unix::fs::symlink(&settings, &link).unwrap();
            let fixes = [deny(link.to_string_lossy().into_owned())];
            report.lsp[0].settings_file = settings.to_string_lossy().into_owned();
            let linked = format_report(&report, &fixes, ReportOptions::default())
                .vibes
                .into_iter()
                .find(|i| i.name.contains("code intelligence"))
                .unwrap();
            assert_eq!(linked.after.len(), 1);
        }
    }

    #[test]
    fn the_memory_card_advises_off_only_where_most_sessions_ran_in_a_repository() {
        use crate::doctor::{Edit, Op, Patch};
        let mut report = synthetic();
        fn memory(report: &mut Report) -> &mut MemoryCheck {
            report
                .checks
                .iter_mut()
                .find_map(|c| match c {
                    CheckResult::Memory(m) => Some(m),
                    _ => None,
                })
                .unwrap()
        }
        let card = |report: &Report| {
            advice(report, ReportOptions::default())
                .into_iter()
                .find(|i| i.name == "Auto memory")
                .unwrap()
        };
        let most = SessionPlaces {
            sessions: 4,
            instructed: 3,
            unknown: 0,
        };
        // doctor keeps it on for its cost, so the card asks with doctor's own edit.
        let m = memory(&mut report);
        (m.base.action, m.base.recommended, m.places) = (Action::Keep, Some(true), most);
        let item = card(&report);
        assert_eq!((item.value.as_str(), item.quiet), ("on -> off", false));
        assert_eq!(
            item.how,
            Some(How::Edit(Patch {
                file: format!("{ROOT}/settings.json"),
                harness: Harness::Claude,
                edits: vec![Edit {
                    path: vec!["autoMemoryEnabled".into()],
                    value: serde_json::json!(false),
                    op: Op::Set,
                }],
            }))
        );
        assert_eq!(
            item.text,
            "3 of 4 main sessions (75%) in the window ran in a repository with a CLAUDE.md or AGENTS.md. A lasting fact about a project belongs in that file, where it is reviewed and every host and teammate sees it, not saved silently on one machine. doctor's line measured $150 at API prices over the window, used 3 times (1 read, 2 writes); judging only cost, it offers no change. Its cost per read or write is within --max-cost-per-use."
        );
        assert!(item.sources[0].starts_with("The maintainer's pick"));
        // doctor already suggests it off: its walk asks, and the card advises nothing more.
        let m = memory(&mut report);
        (m.base.action, m.base.recommended) = (Action::Advisory, Some(false));
        let item = card(&report);
        assert!(item.quiet && item.how.is_none() && item.value.is_empty());
        assert!(
            item.text.contains(", and already suggests turning it off for its cost per use, so doctor asks and this card does not."),
            "{}",
            item.text
        );
        // A change saving too little for doctor's walk to ask is the card's to offer.
        let m = memory(&mut report);
        let measured = m.base.monthly_savings_usd.replace(0.5);
        let item = card(&report);
        assert_eq!((item.value.as_str(), item.quiet), ("on -> off", false));
        assert!(matches!(item.how, Some(How::Edit(_))), "{:?}", item.how);
        assert!(
            item.text.ends_with(", and already suggests turning it off for its cost per use, but its walk does not ask about a change saving under $1/mo."),
            "{}",
            item.text
        );
        memory(&mut report).base.monthly_savings_usd = measured;
        // Half is not most, and a session nothing settles counts as without.
        let m = memory(&mut report);
        (m.base.action, m.base.recommended) = (Action::Keep, Some(true));
        m.places = SessionPlaces {
            sessions: 4,
            instructed: 2,
            unknown: 1,
        };
        let item = card(&report);
        assert!(item.quiet && item.how.is_none());
        assert!(
            item.text.starts_with("2 of 4 main sessions (50%) in the window ran in a repository with a CLAUDE.md or AGENTS.md, counting as without 1 session whose transcript recorded loading none and whose directory is gone or unrecorded. Outside a repository, memory may be the only place a fact is kept, so turning it off is not advised."),
            "{}",
            item.text
        );
        // Unknown whether it is on: the advice stands, as the step doctor gives by hand.
        let m = memory(&mut report);
        (m.base.action, m.base.current, m.base.recommended) = (Action::Advisory, None, None);
        m.unresolved = Some("a named config profile sets memories".into());
        m.places = most;
        let item = card(&report);
        assert_eq!(
            (item.value.as_str(), item.quiet),
            ("may be on -> off", false)
        );
        assert!(
            matches!(item.how, Some(How::Manual { .. })),
            "{:?}",
            item.how
        );
        // A total that leaves a part out is quoted as a floor, naming what it leaves out.
        let m = memory(&mut report);
        m.unresolved = None;
        (m.base.action, m.base.current, m.base.recommended) =
            (Action::Keep, Some(true), Some(true));
        m.unpriced_reads = 2;
        let item = card(&report);
        assert!(
            item.text.contains("doctor's line measured at least $150 at API prices over the window, used 3 times (1 read, 2 writes); that leaves out memory read back by 2 requests that recorded no context, which could not be priced; judging only cost"),
            "{}",
            item.text
        );
        memory(&mut report).unpriced_reads = 0;
        // Off already: nothing to advise.
        let m = memory(&mut report);
        (m.base.action, m.base.current, m.base.recommended) =
            (Action::Keep, Some(false), Some(false));
        m.unresolved = None;
        let item = card(&report);
        assert!(item.quiet && item.how.is_none() && item.text.starts_with("Off. 3 of 4 "));
    }

    #[test]
    fn only_the_built_in_explore_off_haiku_gets_an_agent_file_step_by_hand() {
        let line = |harness, kind: &str, model: &str| SubagentCost {
            harness,
            profiles: vec!["profile".into()],
            kind: kind.into(),
            native: false,
            fork: kind == "fork",
            built_in: true,
            runs: 1,
            requests: 1,
            priced: 1,
            cost: Cost::default(),
            estimate: None,
            models: vec![Share {
                name: model.into(),
                count: 1,
            }],
            efforts: vec![],
            sources: vec![],
            cheapest: None,
            reprice: None,
            reprice_estimate: None,
            model_setting: String::new(),
            effort_setting: String::new(),
            replacement: Replacement {
                missing: vec![ROOT_AGENTS.into()],
                ..Replacement::default()
            },
        };
        let cards = |lines: Vec<SubagentCost>| {
            let mut report = synthetic();
            report.subagents = lines;
            // Only the subagent cards; the synthetic report's other lines have their own.
            advice(&report, ReportOptions::default())
                .into_iter()
                .filter(|i| i.name.contains("subagent"))
                .collect::<Vec<_>>()
        };
        assert_eq!(cards(vec![line(Harness::Codex, "explorer", "gpt-5")]), []);
        assert_eq!(
            cards(vec![line(Harness::Claude, "log-reader", "claude-opus-5-5")]),
            []
        );
        assert_eq!(
            cards(vec![line(Harness::Claude, "Explore", "claude-haiku-5-5")]),
            []
        );
        // An agent file named Explore replaces the built-in, which the guide describes.
        let mut custom = line(Harness::Claude, "Explore", "claude-opus-5-5");
        custom.built_in = false;
        assert_eq!(cards(vec![custom]), []);
        let explore = cards(vec![line(Harness::Claude, "Explore", "claude-opus-5-5")]);
        assert_eq!(explore.len(), 1);
        assert!(
            explore[0]
                .text
                .starts_with("Ran on claude-opus-5-5 and cost an unknown amount."),
            "{explore:?}"
        );
        assert!(
            explore[0]
                .text
                .contains("user agent named Explore with model: haiku")
        );
        // Each directory still missing a file is named; the files there now are said, not asked for.
        let (missing, project) = (
            "/synthetic/home/.claude-work/agents",
            "/synthetic/project/.claude/agents/Explore.md",
        );
        let mut partial = line(Harness::Claude, "Explore", "claude-opus-5-5");
        partial.replacement = Replacement {
            missing: vec![ROOT_AGENTS.into(), missing.into()],
            present: vec![project.into()],
            forced: false,
        };
        let card = &cards(vec![partial.clone()])[0];
        let Some(How::Manual { step, .. }) = &card.how else {
            panic!("{card:?}")
        };
        assert!(
            step.starts_with(&format!(
                "add a new .md agent file to {ROOT_AGENTS} and {missing} with the frontmatter name: Explore;"
            )),
            "{step}"
        );
        assert!(
            card.text.ends_with(&format!(
                " {project} now replaces the built-in, so later runs there follow it."
            )),
            "{}",
            card.text
        );
        assert!(!card.quiet);
        // Once every run has its file there is nothing left to do.
        partial.replacement.missing.clear();
        let card = &cards(vec![partial.clone()])[0];
        assert_eq!(
            (card.quiet, card.how.as_ref(), card.value.as_str()),
            (true, None, "")
        );
        // A forced subagent model drops the file's model, so no file is asked for there.
        let mut forced = line(Harness::Claude, "Explore", "claude-opus-5-5");
        forced.replacement = Replacement {
            forced: true,
            ..Replacement::default()
        };
        let card = &cards(vec![forced.clone()])[0];
        assert_eq!(
            (card.quiet, card.how.as_ref(), card.value.as_str()),
            (false, None, "")
        );
        assert!(
            card.text.ends_with(
                " CLAUDE_CODE_SUBAGENT_MODEL_FORCE is set, which drops an agent file's model, so the tip does not apply while it is."
            ),
            "{}",
            card.text
        );
        // Force in one scope leaves the step for the runs where a file would work.
        forced.replacement.missing = vec![ROOT_AGENTS.into()];
        let card = &cards(vec![forced])[0];
        assert!(matches!(card.how, Some(How::Manual { .. })), "{card:?}");
        assert_eq!(card.value, "inherit -> haiku");
        assert!(
            card.text
                .contains("is set for some runs, which drops an agent file's model, so the tip does not apply to them while it is."),
            "{}",
            card.text
        );
    }

    #[test]
    fn subagents_count_with_the_session_that_spawned_them() {
        let mut report = synthetic();
        // Two main sessions, a subagent of the first and a subagent that subagent spawned.
        for (r, session) in
            report
                .scan
                .requests
                .iter_mut()
                .zip(["main", "other", "child", "grandchild"])
        {
            r.session = session.into();
        }
        let spawn = |session: &str, parent: &str| crate::model::Subagent {
            harness: Harness::Claude,
            session: session.into(),
            parent: Some(parent.into()),
            kind: "Explore".into(),
            built_in: Some(true),
            fork: false,
            native: false,
            model: None,
            effort: None,
        };
        report.scan.subagents = vec![spawn("grandchild", "child"), spawn("child", "main")];
        assert_eq!(sessions(&report.scan), 2);
        let summary = text(&report, None, true);
        assert!(
            summary.contains(" requests in 2 sessions from "),
            "{summary}"
        );
        // A subagent whose parent left no request in the window still counts once.
        report.scan.requests.remove(0);
        assert_eq!(sessions(&report.scan), 2);
    }

    #[test]
    fn empty_histories_give_a_next_step() {
        let mut report = synthetic();
        report.scan.snapshots.clear();
        report.scan.requests.clear();
        report.scan.coverage = ScanCoverage::default();
        report.checks.clear();
        report.context.clear();
        let summary = text(&report, None, true);
        assert!(summary.contains("No requests found since 2026-11-01"));
        assert!(summary.contains("No transcript histories found."));
        assert!(summary.contains("No changes recommended or worth considering."));
        assert!(!summary.contains("CHANGES") && !summary.contains("EVERYTHING ELSE"));
        assert!(!summary.contains("What each piece of context costs"));
    }

    /// The report the CLI builds from the synthetic Claude fixture with an explicit 5m TTL.
    fn synthetic_report() -> Report {
        synthetic_report_with(r#"{"promptCacheTtl":"5m"}"#).0
    }

    /// The report the CLI builds from the synthetic Claude fixture with these settings,
    /// and the settings file's path.
    fn synthetic_report_with(settings: &str) -> (Report, String) {
        let w = Workspace::new();
        w.fixture(w.claude.join("projects/project/main.jsonl"), "claude.jsonl");
        let file = w.claude.join("settings.json");
        std::fs::write(&file, settings).unwrap();
        let mut options = w.scan_options();
        options.discovery.claude_dirs = vec![w.claude.to_string_lossy().into_owned()];
        let report = analyze(AnalyzeOptions {
            scan: scan(&options).unwrap(),
            catalog: &prices(),
            since: None,
            now: instant("2026-11-02T00:00:00Z"),
            max_cost_per_use_usd: 1.0,
            reading: None,
        })
        .unwrap();
        (report, file.to_string_lossy().into_owned())
    }

    /// The synthetic report with `settings`, the profile's billing answered as `billing`.
    fn answered_report(settings: &str, billing: Billing) -> Report {
        let w = Workspace::new();
        w.fixture(w.claude.join("projects/project/main.jsonl"), "claude.jsonl");
        std::fs::write(w.claude.join("settings.json"), settings).unwrap();
        let mut options = w.scan_options();
        let root = w.claude.to_string_lossy().into_owned();
        options.discovery.claude_dirs = vec![root];
        // Keyed by the root discovery reports, as doctor's question does.
        let root = crate::discovery::discover(&options.discovery)
            .unwrap()
            .profiles[0]
            .root
            .clone();
        options.discovery.billing.insert(root, billing);
        analyze(AnalyzeOptions {
            scan: scan(&options).unwrap(),
            catalog: &prices(),
            since: None,
            now: instant("2026-11-02T00:00:00Z"),
            max_cost_per_use_usd: 1.0,
            reading: None,
        })
        .unwrap()
    }

    #[test]
    fn an_answered_billing_prices_the_main_ttl_against_its_one_default() {
        for billing in [Billing::Plan, Billing::Api] {
            let default = billing.main_ttl();
            let report = answered_report("{}", billing);
            let ttl = main_ttl(&report);
            assert_eq!(ttl.base.current, Some(default), "{billing:?}");
            let winner = ttl.base.recommended.unwrap();
            let summary = text(&report, None, true);
            let line = report_line(&summary, "Main-agent prompt cache TTL");
            assert!(
                line.head.starts_with(&format!(
                    "Main-agent prompt cache TTL {} (default)",
                    default.as_str()
                )),
                "{summary}"
            );
            assert!(!line.head.contains("up to"), "{summary}");
            assert!(
                line.evidence.contains("which you answered pay"),
                "{summary}"
            );
            if winner == default {
                assert_eq!(ttl.base.action, Action::Keep, "{billing:?}");
            } else {
                assert_eq!(ttl.base.action, Action::Change, "{billing:?}");
                assert!(ttl.base.savings_usd.is_some(), "{billing:?}");
            }
            let overflow = ttl
                .base
                .uncertainty
                .iter()
                .any(|u| u == crate::discovery::TTL_PLAN_DEFAULT);
            assert_eq!(overflow, billing == Billing::Plan);
            // A pinned TTL is compared with the answered default alone.
            let other = if default == Ttl::OneHour { "5m" } else { "1h" };
            let report = answered_report(&format!(r#"{{"promptCacheTtl":"{other}"}}"#), billing);
            let profile = &report.scan.profiles[0];
            // Its line still notes the answered default, and a plan's overflow onto credits.
            let summary = text(&report, None, true);
            let line = report_line(&summary, "Main-agent prompt cache TTL");
            assert!(
                line.evidence.contains("which you answered pay"),
                "{summary}"
            );
            assert_eq!(
                line.evidence.contains("paid with usage credits and get 5m"),
                billing == Billing::Plan,
                "{summary}"
            );
            let baseline =
                crate::defaults::check(&CheckResult::Ttl(main_ttl(&report).clone()), Some(profile))
                    .unwrap();
            assert_eq!(baseline.default, default.as_str(), "{billing:?}");
            assert!(
                !matches!(baseline.saving, crate::defaults::Saving::UpTo(_)),
                "{billing:?}"
            );
        }
    }

    #[test]
    fn an_answered_billing_line_keeps_an_extrapolated_figures_margin() {
        // Three cache writes ten minutes apart, one on a model with no public price, so the
        // TTL replay extrapolates over it, as in unpriced_requests_are_extrapolated_over.
        let line = |n: u32, minute: u32, model: &str, tokens: u32| {
            format!(
                r#"{{"type":"assistant","sessionId":"session-b","timestamp":"2026-11-01T01:{minute:02}:00Z","version":"2.1.314","entrypoint":"sdk-ts","message":{{"id":"message-b{n}","model":"{model}","usage":{{"input_tokens":0,"output_tokens":20,"cache_read_input_tokens":0,"cache_creation_input_tokens":{tokens},"cache_creation":{{"ephemeral_5m_input_tokens":{tokens},"ephemeral_1h_input_tokens":0}},"service_tier":"standard","speed":"standard","inference_geo":"global","server_tool_use":{{"web_search_requests":0,"web_fetch_requests":0}}}}}}}}"#
            )
        };
        for (settings, billing) in [
            ("{}", Billing::Api),
            (r#"{"promptCacheTtl":"5m"}"#, Billing::Plan),
        ] {
            let w = Workspace::new();
            let file = w.claude.join("projects/project/main.jsonl");
            w.fixture(&file, "claude.jsonl");
            let mut text_ = std::fs::read_to_string(&file).unwrap();
            for (n, minute, model, tokens) in [
                (1, 0, "claude-opus-5-5", 2000),
                (2, 10, "claude-opus-5-5", 2400),
                (3, 20, "private-internal", 2800),
            ] {
                text_ += &line(n, minute, model, tokens);
                text_ += "\n";
            }
            std::fs::write(&file, text_).unwrap();
            std::fs::write(w.claude.join("settings.json"), settings).unwrap();
            let mut options = w.scan_options();
            options.discovery.claude_dirs = vec![w.claude.to_string_lossy().into_owned()];
            let root = crate::discovery::discover(&options.discovery)
                .unwrap()
                .profiles[0]
                .root
                .clone();
            options.discovery.billing.insert(root, billing);
            let report = analyze(AnalyzeOptions {
                scan: scan(&options).unwrap(),
                catalog: &prices(),
                since: None,
                now: instant("2026-11-02T00:00:00Z"),
                max_cost_per_use_usd: 1.0,
                reading: None,
            })
            .unwrap();
            let ttl = main_ttl(&report);
            assert!(matches!(ttl.base.margin, Some(Margin::Usd(m)) if m > 0.0));
            // 5m, unset at the API default or pinned under a plan, against a 1h winner.
            assert_eq!(ttl.base.current, Some(Ttl::FiveMinutes), "{billing:?}");
            assert_eq!(ttl.base.recommended, Some(Ttl::OneHour), "{billing:?}");
            let summary = text(&report, None, true);
            let row = report_line(&summary, "Main-agent prompt cache TTL");
            assert!(
                row.head.contains("~$") && row.head.contains(" ± "),
                "{summary}"
            );
            let baseline = crate::defaults::check(
                &CheckResult::Ttl(ttl.clone()),
                Some(&report.scan.profiles[0]),
            );
            match billing {
                // Unset at its answered default, so nothing to compare.
                Billing::Api => assert_eq!(baseline, None),
                Billing::Plan => {
                    let note = baseline.unwrap().text();
                    assert!(
                        note.starts_with("default 1h, costs ~$") && note.contains(" ± "),
                        "{note}"
                    );
                }
            }
        }
    }

    fn main_ttl(report: &Report) -> &TtlCheck {
        report
            .checks
            .iter()
            .find_map(|c| match c {
                CheckResult::Ttl(t) if t.bucket == Bucket::Main => Some(t),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn an_unset_main_ttl_is_pinned_to_the_replay_winner() {
        use crate::doctor::{Edit, How, Patch, fixes};
        let (report, file) = synthetic_report_with("{}");
        let ttl = report
            .checks
            .iter()
            .find_map(|c| match c {
                CheckResult::Ttl(t) if t.bucket == Bucket::Main => Some(t),
                _ => None,
            })
            .unwrap();
        assert_eq!((ttl.base.action, ttl.base.current), (Action::Change, None));
        // Either billing default may be in force, so no single saving is claimed.
        assert_eq!(ttl.base.savings_usd, None);
        let winner = ttl.base.recommended.unwrap();
        let other = if winner == Ttl::OneHour { "5m" } else { "1h" };
        let winner = winner.as_str();
        let gap = per_month(
            ttl.one_hour_savings_usd.abs(),
            ttl.base.since,
            ttl.base.until,
        );
        let summary = text(&report, None, true);
        let line = report_line(&summary, "Main-agent prompt cache TTL");
        assert_eq!(line.mark, "change");
        assert!(
            line.head.starts_with(&format!(
                "Main-agent prompt cache TTL unset -> {winner} saves up to {gap}/mo"
            )),
            "{summary}"
        );
        assert!(
            line.evidence.contains(&format!(
                "; {winner} costs {gap} less every 30 days, saved whenever the unset default would be {other}, and setting it keeps {winner} however usage is paid. While it is unset,"
            )),
            "{summary}"
        );
        assert!(recommendations(&report, false).iter().any(|l| {
            l.name == "Main-agent prompt cache TTL" && l.value == format!("unset -> {winner}")
        }));
        let fix = fixes(&report, false)
            .into_iter()
            .find(|f| f.name.starts_with("Main-agent prompt cache TTL"))
            .unwrap();
        assert_eq!(
            fix.how,
            How::Edit(Patch {
                file,
                harness: Harness::Claude,
                edits: vec![Edit {
                    path: vec!["promptCacheTtl".into()],
                    value: serde_json::json!(winner),
                    op: crate::doctor::Op::Set,
                }],
            })
        );
    }

    #[test]
    fn unpriced_requests_are_extrapolated_over_with_a_margin() {
        // A second session of three cache writes ten minutes apart, one on a model with no
        // public price, so its replay and the host's spend extrapolate over that one.
        let line = |n: u32, minute: u32, model: &str, tokens: u32| {
            format!(
                r#"{{"type":"assistant","sessionId":"session-b","timestamp":"2026-11-01T01:{minute:02}:00Z","version":"2.1.314","entrypoint":"sdk-ts","message":{{"id":"message-b{n}","model":"{model}","usage":{{"input_tokens":0,"output_tokens":20,"cache_read_input_tokens":0,"cache_creation_input_tokens":{tokens},"cache_creation":{{"ephemeral_5m_input_tokens":{tokens},"ephemeral_1h_input_tokens":0}},"service_tier":"standard","speed":"standard","inference_geo":"global","server_tool_use":{{"web_search_requests":0,"web_fetch_requests":0}}}}}}}}"#
            )
        };
        let run = |unpriced: bool| {
            let w = Workspace::new();
            let file = w.claude.join("projects/project/main.jsonl");
            w.fixture(&file, "claude.jsonl");
            let mut text = std::fs::read_to_string(&file).unwrap();
            text += &line(1, 0, "claude-opus-5-5", 2000);
            text += "\n";
            text += &line(2, 10, "claude-opus-5-5", 2400);
            text += "\n";
            if unpriced {
                text += &line(3, 20, "private-internal", 2800);
                text += "\n";
            }
            std::fs::write(&file, text).unwrap();
            std::fs::write(w.claude.join("settings.json"), r#"{"promptCacheTtl":"5m"}"#).unwrap();
            let mut options = w.scan_options();
            options.discovery.claude_dirs = vec![w.claude.to_string_lossy().into_owned()];
            analyze(AnalyzeOptions {
                scan: scan(&options).unwrap(),
                catalog: &prices(),
                since: None,
                now: instant("2026-11-02T00:00:00Z"),
                max_cost_per_use_usd: 1.0,
                reading: None,
            })
            .unwrap()
        };
        let (whole, partial) = (run(false), run(true));
        let (whole_ttl, ttl) = (ttl_ref(&whole), ttl_ref(&partial));
        let coverage = ttl.base.coverage.unwrap();
        assert!(coverage.priced >= 2 && coverage.priced < coverage.total);
        // The unpriced part is not left out of the verdict, and the figure is marked.
        assert_eq!(ttl.base.recommended, whole_ttl.base.recommended);
        assert!(matches!(ttl.base.margin, Some(Margin::Usd(m)) if m > 0.0));
        assert_eq!(whole_ttl.base.margin, None);
        let spend = Spend::new(&partial);
        assert!(!spend.partial && matches!(spend.margin, Some(Margin::Usd(_))));
        assert_eq!(Spend::new(&whole).margin, None);
        let summary = text(&partial, None, true);
        let title = summary.lines().next().unwrap();
        assert!(title.contains(", ~$") && title.contains(" ± "), "{title}");
        assert!(
            summary.contains("A ~ marks a figure extrapolated"),
            "{summary}"
        );
        let row = report_line(&summary, "Main-agent prompt cache TTL");
        assert!(
            row.head.contains("~$") && row.head.contains(" ± "),
            "{row:?}"
        );
        assert!(!row.evidence.contains("Not enough evidence"), "{row:?}");
        assert!(row.evidence.contains("extrapolated from the"), "{row:?}");
        let full = text(&whole, None, true);
        assert!(!full.contains("~$") && !full.contains('±'), "{full}");
    }

    /// The report's main-agent TTL check.
    fn ttl_ref(report: &Report) -> &TtlCheck {
        report
            .checks
            .iter()
            .find_map(|c| match c {
                CheckResult::Ttl(t) if t.bucket == Bucket::Main => Some(t),
                _ => None,
            })
            .expect("a main-agent claude-cache-ttl check")
    }

    #[test]
    fn an_unowned_t3_check_says_why_in_a_sentence() {
        let mut report = synthetic_report();
        let ttl = ttl_mut(&mut report);
        ttl.base.action = Action::Advisory;
        ttl.base.recommended = None;
        ttl.base.uncertainty.push(crate::t3::UNOWNED.into());
        let summary = text(&report, None, true);
        let line = report_line(&summary, "Main-agent prompt cache TTL");
        assert!(
            line.evidence.starts_with(
                "Not enough evidence: the T3 instance is disabled or the file that owns this setting for it is unresolved, so its past costs do not establish a setting to change."
            ),
            "{summary}"
        );
    }

    #[test]
    fn a_cost_is_a_share_of_the_host_total_and_a_saving_of_its_monthly_rate() {
        // $300 over a 30-day host window: a profile that spent $10 in one day of it is 3%,
        // however short its own history, while $10 every 30 days is 3% of the rate.
        let spend = Spend {
            usd: 300.0,
            margin: None,
            floor: 0.0,
            partial: false,
            since: Some(0),
            until: 30 * DAY as Millis,
        };
        assert_eq!(spend.share(10.0), Some(10.0 / 300.0));
        assert_eq!(spend.of_month(10.0), Some(10.0 / 300.0));
        let week = Spend {
            until: 15 * DAY as Millis,
            ..spend
        };
        assert_eq!(week.share(10.0), Some(10.0 / 300.0));
        assert_eq!(week.of_month(10.0), Some(10.0 / 600.0));
        let none = Spend { usd: 0.0, ..spend };
        assert_eq!((none.share(1.0), none.of_month(1.0)), (None, None));
    }

    #[test]
    fn percents_of_partly_priced_spend_say_their_base() {
        let mut report = synthetic_report();
        let priced = text(&report, None, false);
        assert!(priced.contains("each percent is of this host's spend over the window, $"));
        for group in &mut report.historical {
            group.cost.total_usd = None;
            group.estimate = Some(Estimate {
                usd: 2.0,
                margin: Some(Margin::Usd(0.5)),
            });
        }
        let summary = text(&report, None, false);
        assert!(
            summary.contains(" this host's spend over the window, ~$2 ± $0.50, extrapolated from the requests that could be priced."),
            "{summary}"
        );
        // A profile with no priced request adds only its known part, a lower bound.
        for group in &mut report.historical {
            group.estimate = None;
        }
        let summary = text(&report, None, false);
        assert!(
            summary.contains(", since a profile that priced none of its requests adds only what could be priced, so a line's true share may be smaller."),
            "{summary}"
        );
        assert!(summary.lines().next().unwrap().contains(", at least $"));
    }

    #[test]
    fn a_spend_partly_extrapolated_keeps_its_margin_beside_the_lower_bound() {
        let spend = |usd, margin, floor, partial| Spend {
            usd,
            margin,
            floor,
            partial,
            since: None,
            until: 0,
        };
        assert_eq!(spend(100.0, None, 0.0, false).text(), "$100");
        assert_eq!(
            spend(100.0, Some(Margin::Usd(4.0)), 0.0, false).text(),
            "~$100 ± $4"
        );
        assert_eq!(spend(100.0, None, 100.0, true).text(), "at least $100");
        // One profile extrapolated, another with nothing priced: never a bare "at least".
        assert_eq!(
            spend(130.0, Some(Margin::Usd(4.0)), 30.0, true).text(),
            "~$100 ± $4 plus at least $30"
        );
        // A profile with nothing priced and no known charge still marks the total partial.
        assert_eq!(
            spend(100.0, Some(Margin::Usd(4.0)), 0.0, true).text(),
            "~$100 ± $4 plus requests with no known price"
        );
    }

    #[test]
    fn changes_rank_by_saving_and_the_rest_by_cost() {
        let line = |name: &str, mark, verb: &'static str, share: f64| {
            let mut row = Row::new(mark, name.into(), String::new());
            row.amount = Some((verb, String::new()));
            row.share = Some(share);
            row
        };
        let mut rows = [
            // A kept TTL's comparison saving is not what it costs.
            line("kept TTL", Mark::Keep, "saves", 0.9),
            line("cheap feature", Mark::Keep, "costs", 0.1),
            line("dear feature", Mark::Keep, "costs", 0.5),
            // A change shown only by its cost has no measured saving.
            line("catalog skill", Mark::Change, "costs", 0.8),
            line("small change", Mark::Consider, "saves", 0.2),
            line("big change", Mark::Change, "saves up to", 0.4),
        ];
        rows.sort_by(by_share);
        let order: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            order,
            [
                "dear feature",
                "big change",
                "small change",
                "cheap feature",
                "kept TTL",
                "catalog skill"
            ]
        );
    }

    #[test]
    fn the_opening_note_explains_only_what_the_credit_shows() {
        let partial = "Each percent is of a spend to which a profile that priced none of its requests adds only what could be priced, so a true share may be smaller.";
        let exact =
            "Compared with defaults, your settings save about $40/mo, 9% of this host's spend.";
        let sampled = "Compared with defaults, your settings save ~$40/mo ± $6.";
        assert_eq!(opening_note(exact, false), "");
        assert_eq!(opening_note(exact, true), partial);
        // No percent to qualify.
        assert_eq!(opening_note(sampled, true), TILDE);
        assert_eq!(
            opening_note(&sampled.replace('.', ", 9% of this host's spend."), true),
            format!("{TILDE} {partial}")
        );
    }

    #[test]
    fn the_headline_counts_both_kinds_of_change() {
        assert_eq!(
            headline_text(0, 0),
            "No changes recommended or worth considering."
        );
        assert_eq!(
            headline_text(0, 14),
            "No changes recommended; 14 worth considering."
        );
        assert_eq!(headline_text(1, 0), "1 change recommended.");
        assert_eq!(
            headline_text(2, 1),
            "2 changes recommended and 1 worth considering."
        );
    }

    fn instant(text: &str) -> Millis {
        text.parse::<jiff::Timestamp>().unwrap().as_millisecond()
    }

    /// The headline's words, without its styles.
    fn headline_text(changes: usize, considered: usize) -> String {
        headline(changes, considered)
            .into_iter()
            .map(|(text, _)| text)
            .collect()
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
        advisory.base.profiles = vec![other.id.clone()];
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
        let eligible = period(PeriodStatus::Eligible, true);
        let unknown = PeriodKind {
            reason: "unresolved synthetic setting",
            ..period(PeriodStatus::Unknown, true)
        };
        idle.after_recommendations.periods =
            counts(std::iter::repeat_n(eligible, 100).chain([unknown]));
        idle.current_settings.periods = PeriodCounts::default();
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
        assert!(rendered(&report, ReportOptions::default()).contains("--details"));
        let styled = rendered(
            &report,
            ReportOptions {
                details: true,
                width: Some(120),
                color: true,
                ..Default::default()
            },
        );
        assert!(styled.contains("\x1b["));
        // Color swaps each line's verdict word for a dot; the cards keep every word.
        let cards = |text: &str| text.split_once("\n  EVIDENCE\n").unwrap().1.to_string();
        assert_eq!(cards(&unruled(&strip_ansi(&styled))), cards(&text));
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
        let text = text(&report, None, true);
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
        // The vanished directory has no line; only the details' cards keep every check.
        let (lines, _) = text
            .split_once("\n  EVIDENCE\n")
            .expect("details have cards");
        assert!(
            !lines.contains("working directory no longer exists"),
            "{lines}"
        );
    }

    #[test]
    fn one_setting_resolved_apart_for_groups_of_directories_gets_a_line_that_names_each() {
        let mut report = synthetic_report();
        let group = |scope: Option<&str>, directories: &[Option<&str>]| {
            let mut base = check_base(Action::Advisory, None, None);
            base.owner = format!("{ROOT}/settings.json:promptSuggestionEnabled");
            CheckResult::Background(BackgroundCheck {
                base,
                feature: BackgroundFeature::PromptSuggestions,
                directories: directories.iter().map(|d| d.map(Into::into)).collect(),
                reason: "synthetic".into(),
                scope: scope.map(Into::into),
                off: "\"promptSuggestionEnabled\": false".into(),
                off_file: Some(format!("{ROOT}/settings.json")),
                generations: None,
                cost: cost(None, 0.0),
                cost_estimate: None,
                lost_warming: cost(None, 0.0),
                lost_estimate: None,
            })
        };
        report.checks.extend([
            group(
                None,
                &[Some("/synthetic/home/a"), Some("/synthetic/home/b")],
            ),
            group(
                None,
                &[Some("/synthetic/home/c"), Some("/synthetic/home/d")],
            ),
            group(
                Some("working directory changed"),
                &[Some("/synthetic/home/e")],
            ),
            group(Some("working directory missing"), &[Some("/gone"), None]),
            group(Some("working directory missing"), &[Some("/other")]),
        ]);
        let text = text(&report, None, false);
        let mut tags: Vec<String> = report_lines(&text)
            .into_iter()
            .filter(|r| r.head.starts_with("Prompt suggestions"))
            .filter_map(|r| Some(r.setting[..=r.setting.find(']')?].to_string()))
            .collect();
        tags.sort();
        // Each line gets only as much as it needs to differ from the rest, and none repeats.
        assert_eq!(
            tags,
            [
                "[2 directories, such as /synthetic/home/a]",
                "[2 directories, such as /synthetic/home/c]",
                // The fixture's own line, from a transcript with no working directory.
                "[working directory absent]",
                "[working directory changed]",
                "[working directory missing, /other]",
                "[working directory missing, 2 directories]",
            ],
            "{text}"
        );
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
        // A percent of so short a window can be absurd; only its presence is checked.
        let head = report_line(&text(&report, None, false), "Main-agent prompt cache TTL").head;
        let percent = head
            .strip_prefix("Main-agent prompt cache TTL 5m -> 1h saves $720/mo ")
            .unwrap_or_else(|| panic!("{head}"));
        assert!(percent.ends_with('%'), "{head}");

        let name = "google-drive-comments";
        report.checks.clear();
        let skill = ContextCost {
            harness: Harness::Claude,
            largest_characters: None,
            skill: None,
            details: vec![],
            instruction_size: None,
            hundred_lines: None,
            description: None,
            shares: vec![],
            pretended_requests: None,
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
            margin: None,
            calls: Some(0),
            last_used: None,
            current: true,
            cost_per_use_usd: None,
            verdict: Some(Verdict::Off),
        };
        report.context = vec![skill.clone()];
        // A change names its whole edit and file, so it can be copied from redirected output.
        let setting =
            report_line(&text(&report, None, false), "Skill google-drive-comments").setting;
        let edit = format!(r#""skillOverrides": {{"{name}": "user-invocable-only"}} in "#);
        let file = std::path::Path::new(&setting[edit.len().min(setting.len())..]);
        assert!(setting.starts_with(&edit), "{setting}");
        // slash ok: Path::ends_with compares components
        assert!(file.ends_with(".claude/settings.json"), "{setting}");
        // An offered change's saving scales to 30 days, however short the window, and its
        // evidence keeps the window's total.
        let line = report_line(&text(&report, None, true), "Skill google-drive-comments");
        let percent = line
            .head
            .strip_prefix("Skill google-drive-comments on -> typed only saves $150/mo ")
            .unwrap_or_else(|| panic!("{}", line.head));
        assert!(percent.ends_with('%'), "{}", line.head);
        assert!(line.evidence.starts_with("The model never used it"));
        assert!(
            line.evidence.contains("it cost $5 over the window"),
            "{}",
            line.evidence
        );
        // First sent during the window, so the window cannot rule out a use.
        let mut late = report.clone();
        late.context[0].verdict = Some(Verdict::Consider);
        assert!(text(&late, None, true).contains(
            "The model never used it; it cost $5 over the window. It was first sent during the window, so the window cannot rule out a use. Hide it from the model:"
        ));
        late.scan.snapshots[0].skipped_lines = 1;
        late.scan.snapshots[0].profiles = late.context[0].profiles.clone();
        // An unreadable line that names it, or sits beside a result with no call.
        late.scan.snapshots[0]
            .unreadable
            .observe(format!(r#"{{"skill":"{name}"}}"#).as_bytes());
        assert!(text(&late, None, true).contains(
            "The model never used it; it cost $5 over the window. An unreadable transcript line names it, so it may have been used. Hide it from the model:"
        ));
        late.scan.snapshots[0].unreadable = Default::default();
        late.scan.snapshots[0].unreadable.unmatched_results = 1;
        assert!(text(&late, None, true).contains(
            "The model never used it; it cost $5 over the window. A cut transcript record could hold a use: a tool result there has no recorded call. Hide it from the model:"
        ));
        // A red row beside an unreadable line states the one miss it accepts, in `--details`.
        let mut red = late.clone();
        red.context[0].verdict = Some(Verdict::Off);
        red.scan.snapshots[0].unreadable = Default::default();
        assert!(text(&red, None, true).contains(
            "1 unreadable transcript line does not name it or leave a tool result without a call; a call cut off before its result was written would not show."
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
        // A cost without a change stays the window's total.
        let head = report_line(&text(&report, None, false), "System prompt").head;
        assert!(
            head.strip_prefix("System prompt and core tools costs $1 ")
                .is_some_and(|p| p.ends_with('%')),
            "{head}"
        );
        let details = text(&report, Some(200), true);
        assert!(details.contains("\n  1 change recommended.\n"), "{details}");
        assert!(!details.contains("No applicable"));
    }

    #[test]
    fn doctor_groups_features_by_kind_and_why_they_are_offered() {
        use crate::doctor::{Reason, fixes};
        let mut report = synthetic_report();
        report.checks.clear();
        let profile = report.scan.profiles[0].id.clone();
        let piece = |kind, name: &str, control: Vec<&str>, calls, verdict| ContextCost {
            harness: Harness::Claude,
            largest_characters: None,
            skill: None,
            details: vec![],
            instruction_size: None,
            hundred_lines: None,
            description: None,
            shares: vec![],
            pretended_requests: None,
            profiles: vec![profile.clone()],
            kind,
            name: name.into(),
            control: Some(Control {
                path: control.into_iter().map(String::from).collect(),
                value: ControlValue::Text(name.into()),
                append: false,
            }),
            requests: 10,
            priced: 10,
            conditional_requests: 0,
            tier_unknown_requests: 0,
            assumptions: vec![],
            tokens: Some(100),
            cost_usd: Usd::from_f64(0.01),
            estimated_usd: Some(0.01),
            margin: None,
            calls: Some(calls),
            last_used: None,
            current: true,
            cost_per_use_usd: None,
            verdict: Some(verdict),
        };
        // A redundant catalog copy is off however often it was used.
        let copy = ContextCost {
            skill: Some(crate::model::CatalogSkill {
                path: Some("/synthetic/b/SKILL.md".into()),
                retained_path: Some("/synthetic/kept/SKILL.md".into()),
                ..Default::default()
            }),
            ..piece(
                ContextKind::Skill,
                "b",
                vec!["skills", "b"],
                4,
                Verdict::Off,
            )
        };
        report.context = vec![
            copy,
            piece(
                ContextKind::Skill,
                "a",
                vec!["skillOverrides", "a"],
                0,
                Verdict::Off,
            ),
            piece(
                ContextKind::McpServer,
                "docs",
                vec!["permissions", "deny"],
                0,
                Verdict::Off,
            ),
            piece(
                ContextKind::McpServer,
                "web",
                vec!["permissions", "deny"],
                0,
                Verdict::Consider,
            ),
            piece(
                ContextKind::Tool,
                "Artifact",
                vec!["permissions", "deny"],
                3,
                Verdict::Consider,
            ),
        ];
        let groups: Vec<(String, Reason, String)> = fixes(&report, false)
            .into_iter()
            .map(|f| {
                let g = f.group.expect("a feature is grouped");
                (g.label, g.reason, g.member)
            })
            .collect();
        let want = [
            ("Skills the model never used", Reason::Unused, "a"),
            ("Duplicate copies of skills", Reason::Duplicate, "b"),
            ("Never-used MCP servers", Reason::Unused, "docs"),
            (
                "MCP servers with no recorded use",
                Reason::Unrecorded,
                "web",
            ),
            ("Tools over $1 per use", Reason::Costly, "Artifact"),
        ];
        for (label, reason, member) in want {
            assert!(
                groups.contains(&(label.into(), reason, member.into())),
                "{label}: {groups:?}"
            );
        }
    }
}
