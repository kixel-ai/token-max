//! Auto memory: Claude Code's MEMORY.md and system prompt section, and Codex's memory
//! summary. Each profile gets one line with its total cost and uses, advised off whatever
//! it cost: notes the agent writes for itself skip the review that checked-in
//! `CLAUDE.md` or `AGENTS.md` gets, so durable guidance belongs there instead.

use crate::collections::HashMap;

use crate::context::CODEX_TOKENS_PER_CHARACTER;
use crate::exposure::{MEMORY_SECTION, calibrate, cohort};
use crate::model::{
    Action, Agent, Category, CheckBase, Component, ContextCost, ContextKind, Evidence, Harness,
    Linkage, MemoryAccess, MemoryCheck, Millis, Profile, Request, Scan,
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
        .map(|c| (c.id.as_str(), c))
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
    // What each main-agent request whose context was recorded carried, latest last.
    let mut seen: Vec<(Millis, bool)> = vec![];
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
        if r.agent == Agent::Main {
            seen.push((r.time, !memory.is_empty()));
        }
    }
    let observed = seen.iter().max_by_key(|(time, _)| *time).map(|&(_, on)| on);
    // Claude Code always adds its prompt section while memory is on, so the latest
    // request settles it. Codex adds a summary only once one has been written.
    let on = match profile.harness {
        Harness::Claude => observed.unwrap_or(control.value),
        Harness::Codex => control.value || observed == Some(true),
    };
    let rows: Vec<&ContextCost> = input
        .context
        .iter()
        .filter(|c| c.kind == ContextKind::Memory && c.profiles.contains(&profile.id))
        .collect();
    // Requests whose context went unrecorded may have carried memory too. Codex
    // carries none while its memories are off, and they are off by default.
    let unrecorded = mine
        .iter()
        .filter(|r| !recorded_requests.contains(&*r.id))
        .count();
    let unrecorded = match profile.harness {
        Harness::Claude => unrecorded,
        Harness::Codex if on => unrecorded,
        Harness::Codex => 0,
    };
    let context_usd = rows
        .iter()
        .map(|c| c.estimated_usd)
        .sum::<Option<f64>>()
        .filter(|_| !rows.is_empty() || unrecorded == 0);
    let uses: Vec<_> = scan
        .exposure
        .memory
        .iter()
        .filter(|m| m.harness == profile.harness && m.profiles.contains(&profile.id))
        .collect();
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
            Some(m.characters? as f64 * ratio * output_rate(input.catalog, r, r.parts.first()?)?)
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
    if observed.is_some_and(|o| o != control.value) {
        uncertainty.push(format!(
            "the latest recorded request {} memory, unlike {}",
            if on { "carried" } else { "did not carry" },
            control.owner
        ));
    }
    // Claude Code on against an off setting: a project setting or the session's
    // environment turned it on, which this scan does not resolve per directory.
    let (off, off_file) = if profile.harness == Harness::Claude && on && !control.value {
        (
            "\"autoMemoryEnabled\": false in any project .claude/settings.json or .claude/settings.local.json that turns it on".to_string(),
            None,
        )
    } else {
        (control.off.clone(), control.off_file.clone())
    };
    let (action, recommended) = if on {
        (Action::Advisory, Some(false))
    } else {
        (Action::Keep, Some(false))
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
            current: Some(on),
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
            coverage: None,
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
        off,
        off_file,
        context_usd,
        writes_usd,
        reads,
        writes,
    }
}
