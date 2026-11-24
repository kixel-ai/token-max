//! What the current settings save against the Claude Code and Codex defaults: each setting's
//! documented default, and what its current value saves against it, priced with the replay
//! its own line already runs.

use crate::model::{
    Action, Bucket, CheckResult, ContextCost, Control, ControlValue, Harness, Margin, Profile,
    Report, T3IdleMode, Ttl,
};
use crate::report::{approx, duration, monthly, tokens};

/// Claude Code's `BASH_MAX_TIMEOUT_MS` when unset, which `BASH_DEFAULT_TIMEOUT_MS`'s default stays under.
const BASH_CAP_DEFAULT: i64 = 600_000;

/// A setting whose current value differs from its default, and what that saves.
#[derive(Clone, Debug, PartialEq)]
pub struct Baseline {
    /// The default as a line shows it, such as `5m` or `on`.
    pub default: String,
    pub saving: Saving,
    /// How far the saving may be off when extrapolated over unpriced requests, per 30 days.
    pub margin: Option<Margin>,
}

/// What a current value saves every 30 days against its default; negative when it costs more.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Saving {
    /// Replayed like the line's own comparison.
    Saves(f64),
    /// Saved only while the default would be the other value, as for a pinned main-agent TTL,
    /// whose default Claude Code picks by how usage is paid.
    UpTo(f64),
    /// What a feature that is off cost per request while it was still sent, over every request.
    Estimated(f64),
    NotMeasured,
}

impl Baseline {
    /// The note under a line, such as "default 1h, saves $410/mo".
    pub fn text(&self) -> String {
        let amount = |verb: &str, usd: f64| {
            if usd < 0.0 {
                format!("costs {verb}{} more", approx(-usd, self.margin, "/mo"))
            } else {
                format!("saves {verb}{}", approx(usd, self.margin, "/mo"))
            }
        };
        let saving = match self.saving {
            Saving::Saves(usd) => amount("", usd),
            Saving::UpTo(usd) => amount("up to ", usd),
            Saving::Estimated(usd) => amount("an estimated ", usd),
            Saving::NotMeasured => "saving not measured".into(),
        };
        format!("default {}, {saving}", self.default)
    }
}

/// What a check's current value saves against its default; None when it is the default, or
/// when the current value is unknown. `profile` is the line's lead profile.
pub fn check(result: &CheckResult, profile: Option<&Profile>) -> Option<Baseline> {
    let baseline = |default: &str, saving| {
        Some(Baseline {
            default: default.into(),
            saving,
            margin: None,
        })
    };
    match result {
        CheckResult::Ttl(t) => {
            let b = &t.base;
            let current = b.current?;
            // Positive when 1h costs less than 5m.
            let one_hour = monthly(t.one_hour_savings_usd, b.since, b.until);
            let gap = if current == Ttl::OneHour {
                one_hour
            } else {
                -one_hour
            };
            // The replay decided, and the current client honors the setting.
            let measured = b.recommended.is_some() && b.action != Action::Advisory;
            let saving = |exact: fn(f64) -> Saving| {
                if measured {
                    exact(gap)
                } else {
                    Saving::NotMeasured
                }
            };
            // Bedrock, Vertex and Foundry default the main conversation to 5m too, whatever
            // sets the current value; on the Anthropic API the default follows how the
            // profile pays, when doctor asked.
            let cloud = profile.is_some_and(|p| p.provider.is_some());
            let margin = b
                .margin
                .filter(|_| measured)
                .map(|m| m.scale(monthly(1.0, b.since, b.until)));
            let default = match profile.and_then(|p| p.billing) {
                Some(billing) if t.bucket == Bucket::Main => billing.main_ttl(),
                None if t.bucket == Bucket::Main && !cloud => {
                    return baseline(
                        "1h on a Claude plan, 5m on usage credits or an API key",
                        saving(Saving::UpTo),
                    )
                    .map(|b| Baseline { margin, ..b });
                }
                _ => Ttl::FiveMinutes,
            };
            if current == default {
                return None;
            }
            baseline(default.as_str(), saving(Saving::Saves)).map(|b| Baseline { margin, ..b })
        }
        CheckResult::Compaction(c) => {
            let current = c.base.current.filter(|_| !c.unset)?;
            let profile = profile?;
            if profile.harness == Harness::Codex {
                return baseline("Codex's per-model limit", Saving::NotMeasured);
            }
            // Each model's tuned window; a smaller current window can't replay the larger
            // default, since the detail its summaries dropped is gone.
            let tuned: Vec<Option<i64>> = c
                .models
                .iter()
                .map(|m| {
                    let m = m.as_deref()?;
                    let undated = crate::controls::strip_dated(m, 8).unwrap_or(m);
                    profile.compaction.tuned.get(undated)?.value
                })
                .collect();
            let default = match tuned.first() {
                Some(&Some(first)) if tuned.iter().all(|t| *t == Some(first)) => {
                    if first == current {
                        return None;
                    }
                    tokens(first)
                }
                _ => "the model's tuned window".into(),
            };
            baseline(&default, Saving::NotMeasured)
        }
        // Off is the default, and a threshold in force cannot be replayed off: the detail
        // its summaries dropped is gone.
        CheckResult::TurnEnd(c) => match c.base.current? {
            0 => None,
            _ => baseline("off", Saving::NotMeasured),
        },
        CheckResult::Idle(i) => {
            if i.base.current != Some(false) {
                return None;
            }
            // On minus off, so what turning it off saves.
            let s = &i.current_settings;
            match s.monthly_delta.filter(|_| s.applicable) {
                Some(delta) => Some(Baseline {
                    default: "on".into(),
                    saving: Saving::Saves(delta.usd),
                    margin: delta.margin,
                }),
                None => baseline("on", Saving::NotMeasured),
            }
        }
        CheckResult::T3Idle(t) => {
            let current = t.base.current?;
            if current == T3IdleMode::OnReturn {
                return None;
            }
            // The current mode's cost minus the default's, so what the default costs more.
            let savings = t
                .current_settings
                .scenarios
                .iter()
                .find(|s| s.mode == T3IdleMode::OnReturn)
                .and_then(|s| s.savings);
            match savings {
                Some(s) => {
                    let s = s.scale(monthly(1.0, t.base.since, t.base.until));
                    Some(Baseline {
                        default: T3IdleMode::OnReturn.as_str().into(),
                        saving: Saving::Saves(-s.usd),
                        margin: s.margin,
                    })
                }
                None => baseline(T3IdleMode::OnReturn.as_str(), Saving::NotMeasured),
            }
        }
        // A cap other than the default changes when commands move to the background, and
        // what checking on them costs is not recorded.
        CheckResult::BashCap(b) => match b.base.current? {
            BASH_CAP_DEFAULT => None,
            _ => baseline(&duration(BASH_CAP_DEFAULT), Saving::NotMeasured),
        },
        CheckResult::Speed(s) => match s.base.current.as_deref()? {
            "standard" => None,
            // What turning it off saves is the premium the current speed costs.
            _ if s.base.recommended.as_deref() == Some("standard") => baseline(
                "off",
                s.base
                    .monthly_savings_usd
                    .map_or(Saving::NotMeasured, |usd| Saving::Saves(-usd)),
            ),
            _ => None,
        },
        CheckResult::Background(b) => match b.base.current? {
            true => None,
            // Its requests would be estimated only where it is on.
            false => baseline("on", Saving::NotMeasured),
        },
        CheckResult::Memory(m) => {
            let current = m.base.current.filter(|_| m.unresolved.is_none())?;
            match (m.harness, current) {
                // What it saved while off was never sent, so it is not measured.
                (Harness::Claude, false) => baseline("on", Saving::NotMeasured),
                (Harness::Codex, true) => baseline(
                    "off",
                    match m.base.monthly_savings_usd {
                        Some(usd) if !m.lower_bound() => Saving::Saves(-usd),
                        _ => Saving::NotMeasured,
                    },
                ),
                _ => None,
            }
        }
        // A different effort changes the output, which the transcripts cannot price.
        CheckResult::Effort(e) => {
            let c = &e.control;
            if c.unresolved.is_some() {
                return None;
            }
            // What the session runs at, after a level and any `maxEffortLevel` cap; `auto`
            // sends none, so the model runs at its own default.
            let effective = c.effective.as_deref()?;
            let unset = c.owner.ends_with("(the model's default)") && c.caps.is_empty();
            match c.default.as_deref() {
                Some(default) if effective == default || effective == "auto" => None,
                Some(default) => baseline(default, Saving::NotMeasured),
                None if unset || effective == "auto" => None,
                None => baseline("the model's own", Saving::NotMeasured),
            }
        }
        CheckResult::Keepalive(_) => None,
    }
}

/// Whether two controls flip the same switch: the same setting, and for a list the same entry.
pub fn same_switch(a: &Control, b: &Control) -> bool {
    a.path == b.path
        && (!a.append
            || match (&a.value, &b.value) {
                (ControlValue::Skill { path: a, .. }, ControlValue::Skill { path: b, .. }) => {
                    a == b
                }
                (a, b) => a == b,
            })
}

/// Whether turning `switch` off also turns off what `control` would: the same switch, or an
/// MCP server's switch over one of its tools.
pub(crate) fn covers(switch: &Control, control: &Control) -> bool {
    if same_switch(switch, control) {
        return true;
    }
    fn path(c: &Control) -> Vec<&str> {
        c.path.iter().map(String::as_str).collect()
    }
    match (
        path(switch).as_slice(),
        &switch.value,
        path(control).as_slice(),
        &control.value,
    ) {
        (
            ["permissions", "deny"],
            ControlValue::Text(rule),
            ["permissions", "deny"],
            ControlValue::Text(tool),
        ) => denied_server(rule).is_some_and(|server| tool_of(tool, server)),
        // A server turned off with `/mcp` in a project, against its rule or its tools'.
        (
            ["projects", _, "disabledMcpServers"],
            ControlValue::Text(name),
            ["permissions", "deny"],
            ControlValue::Text(rule),
        ) => tool_of(rule, &crate::mcp::tool_name(name)),
        // And a rule over the server turns off what `/mcp` would.
        (
            ["permissions", "deny"],
            ControlValue::Text(rule),
            ["projects", _, "disabledMcpServers"],
            ControlValue::Text(name),
        ) => {
            let server = format!("mcp__{}", crate::mcp::tool_name(name));
            *rule == server || *rule == format!("{server}__*")
        }
        (["mcp_servers", id, "enabled"], _, ["mcp_servers", tool_id, "disabled_tools"], _) => {
            id == tool_id
        }
        // An allowlist turns off every tool it leaves out.
        (
            ["mcp_servers", id, "enabled_tools"],
            ControlValue::Text(kept),
            ["mcp_servers", tool_id, "disabled_tools"],
            ControlValue::Text(tool),
        ) => id == tool_id && !kept.lines().any(|k| k == tool),
        _ => false,
    }
}

/// The server a Claude deny rule turns every tool of off: `mcp__<server>` or
/// `mcp__<server>__*`.
fn denied_server(rule: &str) -> Option<&str> {
    let server = rule.strip_prefix("mcp__")?;
    let server = server.strip_suffix("__*").unwrap_or(server);
    (!server.is_empty() && !server.contains("__")).then_some(server)
}

/// Whether the deny rule `rule` names the server `server` (as its tools carry it) or one of
/// its tools, wildcard included.
fn tool_of(rule: &str, server: &str) -> bool {
    rule.strip_prefix("mcp__")
        .and_then(|rest| rest.strip_prefix(server))
        .is_some_and(|rest| rest.is_empty() || rest.starts_with("__"))
}

/// The value a switch that is off shows, such as `off` or `typed only`.
pub fn switch_value(control: &Control) -> String {
    match &control.value {
        ControlValue::Text(v) if control.path[0] == "skillOverrides" => match v.as_str() {
            "user-invocable-only" => "typed only".into(),
            other => other.into(),
        },
        _ => "off".into(),
    }
}

/// The name of the tool, skill or MCP server a switch turns off, as its line names it while on.
pub fn switch_name(control: &Control) -> String {
    let path: Vec<&str> = control.path.iter().map(String::as_str).collect();
    match (path.as_slice(), &control.value) {
        (["enableArtifact"], _) => "Artifact tool".into(),
        (["enableWorkflows"], _) => "Workflow tool".into(),
        (["syncClaudeAiSkills"], _) => "Skills synced from claude.ai".into(),
        (["mcp_servers", id, "disabled_tools"], ControlValue::Text(tool)) => {
            format!("MCP tool {id}.{tool}")
        }
        (["features", "apps"], _) => "ChatGPT apps".into(),
        (["mcp_servers", id, _], _) => format!("MCP server {id}"),
        (["skillOverrides", name], _) => format!("Skill {name}"),
        (["projects", _, "disabledMcpServers"], ControlValue::Text(name)) => {
            format!("MCP server {name}")
        }
        (_, ControlValue::Skill { path, .. }) => {
            let dir = path.rsplit_once(['/', '\\']).map_or("", |(dir, _)| dir);
            let name = dir.rsplit_once(['/', '\\']).map_or(dir, |(_, name)| name);
            format!("Skill {name}")
        }
        (_, ControlValue::Text(rule)) => match rule.strip_prefix("mcp__") {
            Some(rest) if denied_server(rule).is_some() => {
                format!("MCP server {}", rest.trim_end_matches("__*"))
            }
            Some(rest) if rest.contains("__") => format!("MCP tool {rule}"),
            Some(server) => format!("MCP server {server}"),
            None => format!("{rule} tool"),
        },
        _ => path.join("."),
    }
}

/// What the feature a switch turns off does, for its off line when no request in the window
/// carried it to record its own description.
pub fn switch_what(control: &Control) -> String {
    use crate::descriptions::{claude_tool, kind};
    use crate::model::ContextKind;
    let path: Vec<&str> = control.path.iter().map(String::as_str).collect();
    let tool = |name: &str| {
        claude_tool(name).map_or_else(|| kind(ContextKind::Tool).to_string(), str::to_string)
    };
    match (path.as_slice(), &control.value) {
        (["enableArtifact"], _) => tool("Artifact"),
        (["enableWorkflows"], _) => tool("Workflow"),
        (["syncClaudeAiSkills"], _) => {
            "Skills from your claude.ai account, which Claude Code syncs into every session.".into()
        }
        (["features", "apps"], _) => {
            "ChatGPT apps, such as connectors to other services, that Codex can call as tools."
                .into()
        }
        (["mcp_servers", _, "disabled_tools"], ControlValue::Text(_)) => {
            kind(ContextKind::McpTool).into()
        }
        (["mcp_servers", ..], _) | (["projects", _, "disabledMcpServers"], _) => {
            kind(ContextKind::McpServer).into()
        }
        (["skillOverrides", _], _) | (_, ControlValue::Skill { .. }) => {
            kind(ContextKind::Skill).into()
        }
        (_, ControlValue::Text(rule)) => match rule.strip_prefix("mcp__") {
            Some(_) if denied_server(rule).is_some() => kind(ContextKind::McpServer).into(),
            Some(rest) if rest.contains("__") => kind(ContextKind::McpTool).into(),
            Some(_) => kind(ContextKind::McpServer).into(),
            None => tool(rule),
        },
        _ => "A feature this setting turns off.".into(),
    }
}

/// The switch that already turns a piece off: every selected profile on its line has its
/// settings file set it, and no file that outranks theirs sets it too. The latest request may
/// still carry the piece, when the change came after it.
pub fn applied<'a>(report: &'a Report, c: &ContextCost) -> Option<&'a Control> {
    let control = c.control.as_ref()?;
    let mut found = None;
    for p in report
        .scan
        .profiles
        .iter()
        .filter(|p| c.profiles.contains(&p.id))
    {
        let switch = p.switched_off.iter().find(|k| covers(k, control))?;
        if crate::doctor::outranked(p, switch) {
            return None;
        }
        found = Some(switch);
    }
    found
}

/// What turning a feature off saved, estimated from what it cost while it was still sent:
/// its cost per request carrying it, over `requests`, every request its line made in a
/// window of `since..until`. None when it was never priced.
pub fn estimate(
    estimated_usd: Option<f64>,
    carried: u64,
    requests: u64,
    since: Option<i64>,
    until: Option<i64>,
) -> Option<f64> {
    let usd = estimated_usd.filter(|_| carried > 0)?;
    Some(monthly(
        usd / carried as f64 * requests as f64,
        since,
        until,
    ))
}

/// The line under the report's headline: the summed saving of every setting that differs
/// from its default. `share` gives a monthly saving's share of the spend of `host`, such as
/// "this host's".
pub fn headline<'a>(
    baselines: impl IntoIterator<Item = &'a Baseline>,
    share: impl Fn(f64) -> Option<f64>,
    host: &str,
) -> String {
    #[derive(Default)]
    struct Sum {
        settings: usize,
        usd: f64,
        margin: Option<Margin>,
        share: f64,
        unknown_share: bool,
    }
    impl Sum {
        fn add(&mut self, usd: f64, margin: Option<Margin>, share: Option<f64>) {
            self.settings += 1;
            self.usd += usd;
            self.margin = Margin::plus(self.margin, margin);
            match share {
                Some(s) => self.share += s,
                None => self.unknown_share = true,
            }
        }
        /// Such as "save about $40/mo, 9% of this host's spend", or without its verb when
        /// `lead` is false, such as "$40/mo more, 9% of this host's spend".
        fn text(&self, about_word: &str, lead: bool, host: &str) -> String {
            let (verb, usd, more) = if self.usd < 0.0 {
                ("cost", -self.usd, " more")
            } else {
                ("save", self.usd, "")
            };
            let amount = approx(usd, self.margin, "/mo");
            let about_word = if amount.starts_with(['<', '~']) {
                ""
            } else {
                about_word
            };
            let share = if self.unknown_share {
                String::new()
            } else {
                format!(
                    ", {} of {host} spend",
                    crate::report::share_text(self.share.abs())
                )
            };
            let verb = if lead {
                format!("{verb} ")
            } else {
                String::new()
            };
            format!("{verb}{about_word}{amount}{more}{share}")
        }
    }
    let settings = |n: usize| format!("{n} setting{}", if n == 1 { "" } else { "s" });
    // "saves" for one setting, "save" for several.
    let verb = |n: usize, plural: &str| {
        if n == 1 {
            format!("{plural}s")
        } else {
            plural.to_string()
        }
    };
    // Each pinned main TTL's default can differ by profile, so "up to" savings and costs
    // bound separately and never net.
    let (mut measured, mut up_to, mut up_to_more, mut estimated, mut unmeasured) =
        (Sum::default(), Sum::default(), Sum::default(), 0, 0);
    for b in baselines {
        match b.saving {
            Saving::Saves(usd) => measured.add(usd, b.margin, share(usd)),
            Saving::Estimated(usd) => {
                measured.add(usd, b.margin, share(usd));
                estimated += 1;
            }
            Saving::UpTo(usd) if usd < 0.0 => up_to_more.add(usd, b.margin, share(usd)),
            Saving::UpTo(usd) => up_to.add(usd, b.margin, share(usd)),
            Saving::NotMeasured => unmeasured += 1,
        }
    }
    if measured.settings + up_to.settings + up_to_more.settings + unmeasured == 0 {
        return "No setting was found to differ from its Claude Code or Codex default.".into();
    }
    let mut sentences = vec![];
    if measured.settings > 0 {
        let mut text = format!("your settings {}", measured.text("about ", true, host));
        if estimated > 0 {
            text += &match estimated {
                1 => ", including an estimate for 1 feature that is now off".to_string(),
                n => format!(", including estimates for {n} features that are now off"),
            };
        }
        sentences.push(text);
    }
    for (sum, action) in [(&up_to, "save"), (&up_to_more, "cost")] {
        if sum.settings > 0 {
            sentences.push(format!(
                "{} whose default depends on how usage is paid {} up to {}",
                settings(sum.settings),
                verb(sum.settings, action),
                sum.text("", false, host)
            ));
        }
    }
    if unmeasured > 0 {
        let more = if sentences.is_empty() { "" } else { " more" };
        sentences.push(match unmeasured {
            1 => format!("1{more} setting differs by an amount not measured"),
            n => format!("{n}{more} settings differ by amounts not measured"),
        });
    }
    // Each saving holds every other setting constant, so two may count the same requests.
    if measured.settings + up_to.settings + up_to_more.settings > 1 {
        sentences.push("overlapping settings may save less".into());
    }
    let mut text = String::from("Compared with defaults, ");
    for (i, sentence) in sentences.iter().enumerate() {
        if i > 0 {
            text += ". ";
            let mut chars = sentence.chars();
            text.extend(chars.next().map(|c| c.to_ascii_uppercase()));
            text.extend(chars);
        } else {
            text += sentence;
        }
    }
    text + "."
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(saving: Saving) -> Baseline {
        Baseline {
            default: "5m".into(),
            saving,
            margin: None,
        }
    }

    #[test]
    fn a_server_rule_wildcard_or_mcp_toggle_covers_the_server_and_its_tools() {
        let rule = |path: &[&str], value: &str| Control {
            path: path.iter().map(|p| p.to_string()).collect(),
            value: ControlValue::Text(value.into()),
            append: true,
        };
        let deny = |value: &str| rule(&["permissions", "deny"], value);
        let toggle = |name: &str| rule(&["projects", "/repo", "disabledMcpServers"], name);
        for switch in [deny("mcp__docs"), deny("mcp__docs__*"), toggle("docs")] {
            assert!(covers(&switch, &deny("mcp__docs")), "{switch:?}");
            assert!(covers(&switch, &deny("mcp__docs__read")), "{switch:?}");
            assert!(
                !covers(&switch, &deny("mcp__docs_extra__read")),
                "{switch:?}"
            );
            assert!(!covers(&switch, &deny("WebFetch")), "{switch:?}");
        }
        assert!(covers(&deny("mcp__docs__*"), &toggle("docs")));
        assert!(!covers(&deny("mcp__docs__read"), &toggle("docs")));
        assert_eq!(switch_name(&deny("mcp__docs__*")), "MCP server docs");
        assert_eq!(switch_name(&toggle("my docs")), "MCP server my docs");
        assert!(covers(&toggle("my docs"), &deny("mcp__my_docs__read")));
        // A server name may hold `__` itself.
        assert!(covers(&deny("mcp__docs__api__*"), &toggle("docs__api")));
        assert!(covers(&deny("mcp__docs__api"), &toggle("docs__api")));
    }

    #[test]
    fn a_note_names_the_default_and_signs_the_saving() {
        assert_eq!(b(Saving::Saves(410.0)).text(), "default 5m, saves $410/mo");
        assert_eq!(
            b(Saving::Saves(-12.5)).text(),
            "default 5m, costs $13/mo more"
        );
        assert_eq!(b(Saving::UpTo(3.0)).text(), "default 5m, saves up to $3/mo");
        assert_eq!(
            b(Saving::Estimated(151.0)).text(),
            "default 5m, saves an estimated $151/mo"
        );
        assert_eq!(
            b(Saving::NotMeasured).text(),
            "default 5m, saving not measured"
        );
    }

    #[test]
    fn the_headline_sums_what_was_measured_and_counts_the_rest() {
        let share = |usd: f64| Some(usd / 10_000.0);
        assert_eq!(
            headline([], share, "this host's"),
            "No setting was found to differ from its Claude Code or Codex default."
        );
        assert_eq!(
            headline(
                &[b(Saving::Saves(900.0)), b(Saving::Estimated(45.0))],
                share,
                "devbox's"
            ),
            "Compared with defaults, your settings save about $945/mo, 9% of devbox's spend, including an estimate for 1 feature that is now off. Overlapping settings may save less."
        );
        assert_eq!(
            headline(
                &[
                    b(Saving::Saves(100.0)),
                    b(Saving::UpTo(50.0)),
                    b(Saving::NotMeasured),
                    b(Saving::NotMeasured)
                ],
                |_| None,
                "this host's"
            ),
            "Compared with defaults, your settings save about $100/mo. 1 setting whose default depends on how usage is paid saves up to $50/mo. 2 more settings differ by amounts not measured. Overlapping settings may save less."
        );
        // Conditional savings and costs bound apart, since each profile's default may differ.
        assert_eq!(
            headline(
                &[b(Saving::UpTo(100.0)), b(Saving::UpTo(-90.0))],
                |_| None,
                "this host's"
            ),
            "Compared with defaults, 1 setting whose default depends on how usage is paid saves up to $100/mo. 1 setting whose default depends on how usage is paid costs up to $90/mo more. Overlapping settings may save less."
        );
        assert_eq!(
            headline(&[b(Saving::NotMeasured)], share, "this host's"),
            "Compared with defaults, 1 setting differs by an amount not measured."
        );
        assert_eq!(
            headline(&[b(Saving::UpTo(-12.0))], share, "this host's"),
            "Compared with defaults, 1 setting whose default depends on how usage is paid costs up to $12/mo more, <1% of this host's spend."
        );
        assert_eq!(
            headline(&[b(Saving::Saves(-20.0))], share, "this host's"),
            "Compared with defaults, your settings cost about $20/mo more, <1% of this host's spend."
        );
    }

    #[test]
    fn switches_match_by_setting_and_list_entry() {
        let deny = |rule: &str| Control {
            path: vec!["permissions".into(), "deny".into()],
            value: ControlValue::Text(rule.into()),
            append: true,
        };
        let skill = |path: &str, enabled| Control {
            path: vec!["skills".into(), "config".into()],
            value: ControlValue::Skill {
                path: path.into(),
                enabled,
            },
            append: true,
        };
        let overrides = |value: &str| Control {
            path: vec!["skillOverrides".into(), "pdf".into()],
            value: ControlValue::Text(value.into()),
            append: false,
        };
        assert!(same_switch(&deny("Workflow"), &deny("Workflow")));
        assert!(!same_switch(&deny("Workflow"), &deny("Monitor")));
        assert!(same_switch(
            &skill("/s/a/SKILL.md", false),
            &skill("/s/a/SKILL.md", true)
        ));
        assert!(!same_switch(
            &skill("/s/a/SKILL.md", false),
            &skill("/s/b/SKILL.md", false)
        ));
        // A skill override is one setting, whatever it is set to.
        assert!(same_switch(
            &overrides("off"),
            &overrides("user-invocable-only")
        ));
        assert_eq!(
            switch_value(&overrides("user-invocable-only")),
            "typed only"
        );
        // A server's switch covers its tools.
        assert!(covers(&deny("mcp__github"), &deny("mcp__github__issue")));
        assert!(!covers(&deny("mcp__git"), &deny("mcp__github__issue")));
        assert!(!covers(
            &deny("mcp__github__issue"),
            &deny("mcp__github__pr")
        ));
        let codex = |path: &[&str], value| Control {
            path: path.iter().map(|p| p.to_string()).collect(),
            value,
            append: false,
        };
        let tool = || {
            codex(
                &["mcp_servers", "docs", "disabled_tools"],
                ControlValue::Text("read".into()),
            )
        };
        let server = |id: &str| codex(&["mcp_servers", id, "enabled"], ControlValue::Bool(false));
        assert!(covers(&server("docs"), &tool()));
        assert!(!covers(&server("web"), &tool()));
        assert_eq!(switch_value(&overrides("off")), "off");
        assert_eq!(switch_value(&deny("Monitor")), "off");
        assert_eq!(switch_name(&deny("Monitor")), "Monitor tool");
        assert_eq!(switch_name(&deny("mcp__github")), "MCP server github");
        assert_eq!(
            switch_name(&deny("mcp__github__issue")),
            "MCP tool mcp__github__issue"
        );
        assert_eq!(switch_name(&overrides("off")), "Skill pdf");
        assert_eq!(
            switch_name(&skill("/s/imagegen/SKILL.md", false)),
            "Skill imagegen"
        );
    }

    #[test]
    fn an_estimate_scales_the_cost_per_carrying_request_to_every_request() {
        // $3 over 10 requests that carried it, over 40 requests in a 30-day window.
        let day = 86_400_000;
        assert_eq!(
            estimate(Some(3.0), 10, 40, Some(0), Some(30 * day)),
            Some(12.0)
        );
        assert_eq!(estimate(None, 10, 40, Some(0), Some(30 * day)), None);
        assert_eq!(estimate(Some(3.0), 0, 40, Some(0), Some(30 * day)), None);
    }
}
