//! Auto memory: Claude Code's MEMORY.md and system prompt section, and Codex's memory
//! summary. Each profile gets one line with its total cost and uses, advised off whatever
//! it cost: notes the agent writes for itself skip the review that checked-in
//! `CLAUDE.md` or `AGENTS.md` gets, so durable guidance belongs there instead.

use crate::collections::HashMap;

use crate::context::CODEX_TOKENS_PER_CHARACTER;
use crate::exposure::{MEMORY_SECTION, calibrate, cohort};
use crate::model::{
    Action, Agent, Category, CheckBase, Component, ContextCost, ContextKind, Evidence, Harness,
    Linkage, MemoryAccess, MemoryCheck, Millis, PartKind, Profile, Request, Scan,
};
use crate::prices::{Catalog, output_rate};

const DAY: f64 = 86_400_000.0;

const SOURCES: [&str; 2] = [
    "https://code.claude.com/docs/en/memory#auto-memory (2026-11-11)",
    "https://developers.openai.com/codex/memories (2026-11-11)",
];

/// Inputs for the auto memory checks.
pub struct MemoryInput<'a> {
    pub scan: &'a Scan,
    /// Context costs, whose auto memory rows hold its loaded context and reads.
    pub context: &'a [ContextCost],
    pub catalog: &'a Catalog,
    /// The explicit window start; None uses each profile's first request.
    pub since: Option<Millis>,
    /// The window end, which monthly values scale to.
    pub now: Millis,
}

/// One check per profile: advisory while auto memory is on, keep once it is off.
pub fn memory_checks(input: &MemoryInput<'_>) -> Vec<MemoryCheck> {
    let scan = input.scan;
    let calibration: HashMap<String, f64> = calibrate(&scan.exposure, &scan.requests)
        .into_iter()
        .map(|c| (c.cohort, c.tokens_per_character))
        .collect();
    let requests: HashMap<&str, &Request> = scan.requests.iter().map(|r| (&*r.id, r)).collect();
    let components: HashMap<&str, &Component> = scan
        .exposure
        .components
        .iter()
        .map(|c| (&*c.id, c))
        .collect();
    scan.profiles
        .iter()
        .filter_map(|profile| {
            let control = profile.memory.as_ref()?;
            Some(check(
                input,
                profile,
                control,
                &requests,
                &components,
                &calibration,
            ))
        })
        .collect()
}

/// The main-agent request whose working directory auto memory's setting is read at:
/// the latest one, where the user worked most recently.
pub(crate) fn deciding_request<'a>(requests: &[&'a Request]) -> Option<&'a Request> {
    requests
        .iter()
        .copied()
        .filter(|r| r.agent == Agent::Main)
        .max_by_key(|r| r.time)
}

fn check(
    input: &MemoryInput<'_>,
    profile: &Profile,
    control: &crate::model::MemoryControl,
    requests: &HashMap<&str, &Request>,
    components: &HashMap<&str, &Component>,
    calibration: &HashMap<String, f64>,
) -> MemoryCheck {
    let scan = input.scan;
    let mine: Vec<&Request> = scan
        .requests
        .iter()
        .filter(|r| r.harness == profile.harness && r.profiles.contains(&profile.id))
        .collect();
    // Today's setting decides whether memory is on, as it does for every other check.
    // Each main-agent session's latest recorded request shows what that session
    // loaded; only sessions in the directory the setting was read at are compared.
    let mut sessions: HashMap<&str, (Millis, bool)> = HashMap::default();
    let mut unmeasured = 0;
    let mut recorded_requests = crate::collections::HashSet::default();
    for e in &scan.exposure.requests {
        let Some(r) = requests.get(&*e.request) else {
            continue;
        };
        if r.harness != profile.harness || !r.profiles.contains(&profile.id) {
            continue;
        }
        let (ids, recorded) = match (r.harness, e.view.linkage) {
            (Harness::Claude, Linkage::Wire) => (&e.view.components, true),
            (Harness::Codex, Linkage::Chronological) => (&e.view.candidates, true),
            _ => (&e.view.components, false),
        };
        if !recorded {
            continue;
        }
        recorded_requests.insert(&*e.request);
        let memory: Vec<&Component> = ids
            .iter()
            .map(|id| components[&**id])
            .filter(|c| c.category == Category::Memory)
            .collect();
        if r.harness == Harness::Claude
            && !memory.is_empty()
            && !memory.iter().any(|c| c.name == MEMORY_SECTION)
        {
            unmeasured += 1;
        }
        let here = control.directory.is_none() || control.directory.as_deref() == r.cwd.as_deref();
        if r.agent == Agent::Main && here {
            let seen = sessions.entry(&r.session).or_insert((r.time, false));
            if r.time >= seen.0 {
                *seen = (r.time, !memory.is_empty());
            }
        }
    }
    let on = control.value;
    // Unknown when a setting the scan cannot resolve, such as a Codex --profile
    // selection, could change it.
    // A T3 instance's launch arguments and environment can override native settings.
    let unresolved = control.unresolved.clone().or_else(|| {
        profile
            .t3
            .as_ref()
            .filter(|t| !t.ownership_resolved)
            .map(|_| "the T3 instance's launch arguments or environment can override the setting, and which apply is unresolved".to_string())
    });
    let current = unresolved.is_none().then_some(on);
    // Claude Code adds its prompt section whenever memory is on. Codex adds a summary
    // only once one has been written, so its absence shows nothing.
    let disagree = sessions
        .values()
        .filter(|&&(_, carried)| match profile.harness {
            _ if current.is_none() => false,
            Harness::Claude => carried != on,
            Harness::Codex => carried && !on,
        })
        .count();
    let rows: Vec<&ContextCost> = input
        .context
        .iter()
        .filter(|c| c.kind == ContextKind::Memory && c.profiles.contains(&profile.id))
        .collect();
    // Requests whose context went unrecorded may have carried memory too, whatever
    // today's setting says, since it may have been on when they ran.
    let unrecorded = mine
        .iter()
        .filter(|r| !recorded_requests.contains(&*r.id))
        .count();
    let uses: Vec<_> = scan
        .exposure
        .memory
        .iter()
        .filter(|m| m.harness == profile.harness && m.profiles.contains(&profile.id))
        .collect();
    let context_usd = rows
        .iter()
        .map(|c| c.estimated_usd)
        .sum::<Option<f64>>()
        .filter(|_| !rows.is_empty() || unrecorded == 0);
    let reads = uses
        .iter()
        .filter(|m| m.access == MemoryAccess::Read)
        .count() as u64;
    // What the model wrote into memory was its own output, priced at that request's rate.
    let writes_usd = uses
        .iter()
        .filter(|m| m.access == MemoryAccess::Write)
        .map(|m| {
            let r = requests.get(m.request.as_deref()?)?;
            let ratio = calibration
                .get(&cohort(r))
                .copied()
                .or((r.harness == Harness::Codex).then_some(CODEX_TOKENS_PER_CHARACTER))?;
            // The message iteration that wrote the call, not a compaction or advisor
            // one; unpriced when several message iterations bill at different rates.
            let rates = r
                .parts
                .iter()
                .filter(|p| p.kind == PartKind::Message)
                .map(|p| output_rate(input.catalog, r, p))
                .collect::<Option<Vec<f64>>>()?;
            let rate = *rates.first()?;
            rates.iter().all(|&x| x == rate).then_some(())?;
            Some(m.characters? as f64 * ratio * rate)
        })
        .sum::<Option<f64>>();
    let writes = uses.len() as u64 - reads;
    let total = context_usd.zip(writes_usd).map(|(a, b)| a + b);
    let start = input.since.or(mine.iter().map(|r| r.time).min());
    let monthly = match (total, start) {
        (Some(usd), Some(start)) if input.now > start => {
            Some((usd * 30.0 * DAY) / (input.now - start) as f64)
        }
        _ => total,
    };
    let mut uncertainty = vec![];
    if rows.iter().any(|c| c.priced < c.requests) {
        uncertainty.push(
            "loaded memory is scaled from the requests that could be priced to every request that carried it"
                .to_string(),
        );
    }
    if rows.is_empty() && unrecorded > 0 {
        uncertainty.push(format!(
            "none of {unrecorded} requests recorded their context, so loaded memory is unmeasured"
        ));
    } else if unrecorded > 0 {
        uncertainty.push(format!(
            "{unrecorded} requests did not record their context; memory they loaded is unmeasured"
        ));
    } else if context_usd.is_none() {
        uncertainty.push("no request that carried memory could be priced".into());
    }
    if writes > 0 && writes_usd.is_none() {
        uncertainty.push(
            "a memory write's request, model price or token ratio is unknown, so writes are unpriced"
                .into(),
        );
    }
    if unmeasured > 0 {
        uncertainty.push(format!(
            "{unmeasured} requests loaded MEMORY.md without a # Memory section in their system prompt; memory instructions elsewhere in it are unmeasured"
        ));
    }
    if profile.harness == Harness::Codex {
        uncertainty.push("Codex extracts and consolidates memories in requests it records only in telemetry, so their cost is not measurable here".into());
        uncertainty.push("Codex shell reads of the memories folder count as uses, but their output stays on the shell tool's line".into());
    }
    if let Some(reason) = &unresolved {
        uncertainty.push(format!(
            "{reason}, so whether memory is on is unknown; without it, {} leaves it {}",
            control.owner,
            if on { "on" } else { "off" },
        ));
    }
    if disagree > 0 {
        uncertainty.push(format!(
            "{disagree} of {} sessions{} {} memory at their latest request, though {} turns it {}: a session keeps the setting it started with until it restarts, and an environment variable set only where it was launched is not visible here",
            sessions.len(),
            if control.directory.is_some() {
                " in the directory the setting was read at"
            } else {
                ""
            },
            if on { "did not carry" } else { "still carried" },
            control.owner,
            if on { "on" } else { "off" },
        ));
    }
    let (action, recommended) = match current {
        Some(false) => (Action::Keep, Some(false)),
        _ => (Action::Advisory, Some(false)),
    };
    MemoryCheck {
        base: CheckBase {
            profile: profile.id.clone(),
            history_profiles: rows
                .iter()
                .flat_map(|c| c.profiles.iter())
                .cloned()
                .collect::<crate::collections::IndexSet<_>>()
                .into_iter()
                .collect(),
            evidence: Evidence::Estimated,
            action,
            current,
            recommended,
            owner: control.owner.clone(),
            since: start,
            until: Some(input.now),
            window_basis: if input.since.is_none() {
                "available selected usage"
            } else {
                "explicit --since"
            }
            .into(),
            baseline: "the recorded requests without auto memory's loaded context, reads and writes, holding all other work constant".into(),
            requests: rows.iter().map(|c| c.requests).sum(),
            // With requests that recorded no context, the total is a lower bound.
            coverage: (unrecorded > 0).then(|| crate::model::Coverage {
                priced: (mine.len() - unrecorded) as u64,
                total: mine.len() as u64,
            }),
            savings_usd: total,
            monthly_savings_usd: monthly,
            break_even: None,
            assumptions: [
                "recommended off whatever it cost: notes the agent writes for itself are not reviewed, which encourages slop; durable guidance belongs in reviewed, checked-in CLAUDE.md or AGENTS.md",
                "loaded memory is priced like other context at the cache state of its position",
                "memory files read back are priced like other tool output; writes are estimated at the output price from their recorded characters",
                "turning memory off is assumed to change no other work",
            ]
            .map(String::from)
            .into(),
            uncertainty,
            sources: SOURCES.map(String::from).into(),
        },
        harness: profile.harness,
        off: control.off.clone(),
        off_file: control.off_file.clone(),
        context_usd,
        writes_usd,
        reads,
        writes,
        unresolved,
        overrides: control.overrides.clone(),
    }
}
