//! Auto memory: Claude Code's MEMORY.md and system prompt section, and Codex's memory
//! summary. Each profile gets one line with its total cost and uses, judged like a skill:
//! off when never read back or written, worth considering off above the cost per use.

use crate::collections::HashMap;

use crate::context::CODEX_TOKENS_PER_CHARACTER;
use crate::exposure::{MEMORY_SECTION, calibrate, cohort};
use crate::model::{
    Action, Agent, Category, CheckBase, Component, ContextCost, ContextKind, Estimate, Evidence,
    Harness, Linkage, Margin, MemoryAccess, MemoryCheck, Millis, PartKind, Profile, ProfileShare,
    Request, Scan, SessionPlaces, Usd,
};
use crate::prices::{Catalog, Sample, output_rate};

/// The providers' pages on auto memory and Codex memories.
pub(crate) const SOURCES: [&str; 2] = [
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
    /// Above this cost per read or write, memory is worth considering off.
    pub max_cost_per_use_usd: f64,
}

/// One check per line of profiles sharing the setting's owner: a change while auto memory
/// is on and never used, advisory above the cost per use, keep otherwise.
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
    let owner = |p: &Profile| {
        let m = p.memory.as_ref().expect("filtered on a memory control");
        format!("{}\n{}", m.owner, m.off_file.as_deref().unwrap_or_default())
    };
    let controlled = scan.profiles.iter().filter(|p| p.memory.is_some());
    crate::rows::groups(controlled, owner)
        .iter()
        .map(|line| {
            let control = line.lead.memory.as_ref().expect("a memory control");
            check(input, line, control, &requests, &components, &calibration)
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
    line: &crate::rows::Group<'_>,
    control: &crate::model::MemoryControl,
    requests: &HashMap<&str, &Request>,
    components: &HashMap<&str, &Component>,
    calibration: &HashMap<String, f64>,
) -> MemoryCheck {
    let scan = input.scan;
    let profile = line.lead;
    let mine: Vec<&Request> = scan
        .requests
        .iter()
        .filter(|r| r.harness == profile.harness && line.covers(&r.profiles))
        .collect();
    // Today's setting decides whether memory is on, as it does for every other check.
    // Each main-agent session's latest recorded request shows what that session
    // loaded; only sessions in the directory the setting was read at are compared.
    let mut sessions: HashMap<&str, (Millis, bool)> = HashMap::default();
    let mut unmeasured = 0;
    let mut recorded_requests = crate::collections::HashSet::default();
    // Recorded main-agent requests, and those of them that carried memory.
    let (mut main_recorded, mut main_carried) = (0u64, 0u64);
    for e in &scan.exposure.requests {
        let Some(r) = requests.get(&*e.request) else {
            continue;
        };
        if r.harness != profile.harness || !line.covers(&r.profiles) {
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
            main_recorded += 1;
            main_carried += u64::from(!memory.is_empty());
        }
        let here = control.directory.is_none() || control.directory.as_deref() == r.cwd.as_deref();
        if r.agent == Agent::Main && here {
            let seen = sessions.entry(&r.session).or_insert((r.time, false));
            if r.time >= seen.0 {
                *seen = (r.time, !memory.is_empty());
            }
        }
    }
    let places = places(
        line,
        &settled(line, &mine, &scan.profiles),
        &scan.exposure.project_files,
    );
    let on = control.value;
    // Unknown when a setting the scan cannot resolve, such as a Codex --profile
    // selection, could change it.
    // A T3 instance's launch arguments, environment and machine policy can override
    // native settings.
    let unresolved = control.unresolved.clone().or_else(|| {
        profile
            .t3
            .as_ref()
            .filter(|t| t.enabled && t.settings_unresolved)
            .map(|_| "the T3 instance's launch arguments, a secret environment value or machine policy can override the setting, and which apply is unresolved".to_string())
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
    // A memory row covers every profile of its settings file, while memory's own switch
    // can differ by profile, such as a T3 instance's environment; this line takes only
    // the requests its own profiles recorded.
    let rows: Vec<ProfileShare> = input
        .context
        .iter()
        .filter(|c| c.kind == ContextKind::Memory)
        .flat_map(|c| c.shares_within(|profiles| line.covers(profiles)))
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
        .filter(|m| m.harness == profile.harness && line.covers(&m.profiles))
        .collect();
    // Claude Code loads memory into every request while it is on, so recorded requests
    // that carried none ran with it off, and what it costs on is unknown, not nothing.
    let never_on = profile.harness == Harness::Claude
        && current == Some(true)
        && rows.is_empty()
        && mine.len() > unrecorded;
    let (recorded, unrecorded) = ((mine.len() - unrecorded) as u64, unrecorded as u64);
    let (loaded, unpriced_reads) = extrapolate(&rows, recorded, unrecorded);
    let loaded = loaded.filter(|_| !never_on);
    // Under --defaults memory is on, though the settings files turned it off for some
    // recorded requests: what it cost while main-agent requests carried it, the files read
    // back included, since a request without memory reads none, stands for all of them.
    let scaled = (scan.defaults
        && profile.harness == Harness::Claude
        && current == Some(true)
        && main_carried > 0
        && main_carried < main_recorded)
        .then_some((main_carried, main_recorded));
    let loaded = match scaled {
        Some((carried, recorded)) => {
            let scale = recorded as f64 / carried as f64;
            loaded.map(|e| Estimate {
                usd: e.usd * scale,
                margin: e.margin.map(|m| m.scale(scale)),
            })
        }
        None => loaded,
    };
    let context_usd = loaded.map(|e| Usd::from_f64(e.usd));
    let margin = loaded.and_then(|e| e.margin);
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
            Some(Usd::from_f64(m.characters? as f64 * ratio * rate))
        })
        .sum::<Option<Usd>>();
    let writes = uses.len() as u64 - reads;
    let total = context_usd.zip(writes_usd).map(|(a, b)| (a + b).to_f64());
    let (context_usd, writes_usd) = (context_usd.map(Usd::to_f64), writes_usd.map(Usd::to_f64));
    let start = input.since.or(mine.iter().map(|r| r.time).min());
    let monthly = total.map(|usd| crate::report::monthly(usd, start, Some(input.now)));
    let mut uncertainty = vec![];
    if rows
        .iter()
        .any(|c| c.recorded.priced() < c.recorded_requests)
    {
        uncertainty.push(
            "loaded memory is extrapolated from the requests that could be priced to every request that carried it"
                .to_string(),
        );
    }
    if let Some((carried, recorded)) = scaled {
        uncertainty.push(format!(
            "a settings file turns auto memory off, so {carried} of {recorded} recorded main-agent requests carried it; its loaded context and the files read back are scaled from those to all of them, and its writes are theirs"
        ));
    }
    if never_on {
        uncertainty.push(format!(
            "no recorded request carried auto memory, though {} turns it on, so what it costs is unmeasured",
            control.owner
        ));
    } else if recorded == 0 && unrecorded > 0 {
        uncertainty.push(format!(
            "none of {unrecorded} requests recorded their context, so loaded memory is unmeasured"
        ));
    } else if context_usd.is_none() {
        uncertainty.push("no request that carried memory could be priced".into());
    } else if unrecorded > 0 {
        uncertainty.push(format!(
            "{unrecorded} requests did not record their context; memory they loaded is extrapolated from the requests that did, assuming they loaded it as often"
        ));
    }
    if unpriced_reads > 0 && context_usd.is_some() {
        uncertainty.push(format!(
            "{unpriced_reads} requests that did not record their context read memory back, and none of those reads could be priced, so the total leaves them out"
        ));
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
    // The memory rows that hold this line's requests, for what their transcripts and
    // prices leave unsettled.
    let costs: Vec<&ContextCost> = input
        .context
        .iter()
        .filter(|c| c.kind == ContextKind::Memory)
        .filter(|c| !c.shares_within(|profiles| line.covers(profiles)).is_empty())
        .collect();
    let use_unproven = costs.iter().any(|c| {
        crate::context::hidden_use(c, &scan.snapshots, &scan.exposure, &scan.profiles).is_some()
    });
    // A write is priced at its own request's output rate, which may carry no memory.
    let tier_unknown = costs.iter().any(|c| c.tier_unknown_requests > 0)
        || uses
            .iter()
            .filter(|m| m.access == MemoryAccess::Write)
            .filter_map(|m| requests.get(m.request.as_deref()?))
            .any(|r| {
                r.parts
                    .iter()
                    .filter(|p| p.kind == PartKind::Message)
                    .any(|p| crate::prices::conditional_tier(r, p))
            });
    if use_unproven {
        uncertainty.push(
            "a transcript line that could not be read, or was still being written, could hold a read or write"
                .into(),
        );
    }
    if tier_unknown {
        uncertainty.push(
            "a request that carried or wrote memory recorded no service tier, so its price depends on the tier that served it"
                .into(),
        );
    }
    let unmeasured_generation = profile.harness == Harness::Codex
        && (current != Some(false) || control.stored || !uses.is_empty() || !rows.is_empty());
    let lower_bound = unmeasured_generation || unpriced_reads > 0;
    // Judged as a skill is: never used is off, above the limit per use worth considering
    // off. A total that leaves a part out can prove neither that it was free nor that
    // each use stayed under the limit, a price that depends on an unrecorded tier settles
    // no comparison, and a line that could hold a use leaves nonuse unproven.
    let (action, recommended) = match (current, total) {
        (Some(false), _) => (Action::Keep, Some(false)),
        (None, _) | (Some(true), None) => (Action::Advisory, None),
        (Some(true), Some(_)) if tier_unknown => (Action::Advisory, None),
        (Some(true), Some(usd)) => match reads + writes {
            _ if usd <= 0.0 && lower_bound => (Action::Advisory, None),
            _ if usd <= 0.0 => (Action::Keep, Some(true)),
            0 if use_unproven => (Action::Advisory, Some(false)),
            0 => (Action::Change, Some(false)),
            n if usd / n as f64 > input.max_cost_per_use_usd => (Action::Advisory, Some(false)),
            _ if lower_bound => (Action::Advisory, None),
            _ => (Action::Keep, Some(true)),
        },
    };
    MemoryCheck {
        base: CheckBase {
            profile: profile.id.clone(),
            profiles: line.ids.clone(),
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
            // Requests that recorded no context are extrapolated over.
            coverage: (unrecorded > 0).then_some(crate::model::Coverage {
                priced: recorded,
                total: mine.len() as u64,
                ..Default::default()
            }),
            savings_usd: total,
            margin: margin.filter(|_| total.is_some()),
            monthly_savings_usd: monthly,
            break_even: None,
            assumptions: [
                "a read back or a write is a use, as a skill's invocation is; loading the memory index into every request is not",
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
        unmeasured_generation,
        unpriced_reads,
        use_unproven,
        tier_unknown,
        places,
    }
}

/// Where the line's main sessions ran, each at its latest request's directory: in a
/// repository with a project instruction file when its transcript recorded loading one at or
/// above that directory, of the files `loaded` holds for it, or its directory holds one now;
/// unknown when neither settles it.
/// The requests of `mine` whose sessions count toward where a line's sessions ran: for
/// Claude, only those T3 settled to one of its profiles, as for the directory its setting
/// is read at. Codex reads that directory before T3 attributes requests, so keeps them all.
fn settled<'a>(
    line: &crate::rows::Group<'_>,
    mine: &[&'a Request],
    profiles: &[Profile],
) -> Vec<&'a Request> {
    if line.lead.harness != Harness::Claude {
        return mine.to_vec();
    }
    let t3 = crate::discovery::t3_instances(profiles);
    mine.iter()
        .copied()
        .filter(|r| {
            line.members
                .iter()
                .any(|p| crate::discovery::owned_by(r, p, &t3))
        })
        .collect()
}

fn places(
    line: &crate::rows::Group<'_>,
    mine: &[&Request],
    loaded: &crate::collections::IndexMap<crate::model::Text, crate::collections::IndexSet<String>>,
) -> SessionPlaces {
    let mut latest: HashMap<&str, &Request> = HashMap::default();
    for r in mine.iter().filter(|r| r.agent == Agent::Main) {
        let seen = latest.entry(&r.session).or_insert(r);
        if r.time >= seen.time {
            *seen = r;
        }
    }
    let mut places = SessionPlaces {
        sessions: latest.len() as u64,
        ..SessionPlaces::default()
    };
    for (session, r) in latest {
        // A file loaded from where the session ran last, not from where it ran before.
        let recorded = r.cwd.as_deref().is_some_and(|cwd| {
            loaded
                .get(session)
                .is_some_and(|files| files.iter().any(|f| project_file(f, cwd)))
        });
        if recorded {
            places.instructed += 1;
            continue;
        }
        // Each member read the directories its own sessions ran in.
        let now = r.cwd.as_deref().and_then(|cwd| {
            line.members
                .iter()
                .filter_map(|p| p.memory.as_ref()?.instructed.get(cwd).copied())
                .next()
                .flatten()
        });
        match now {
            Some(true) => places.instructed += 1,
            Some(false) => {}
            None => places.unknown += 1,
        }
    }
    places
}

/// Whether `file`, an instruction file a session at `cwd` loaded as its project's, is one
/// its repository checks in: a CLAUDE.md or AGENTS.md at `cwd` or above it, as recorded or,
/// when both directories still exist, once symlinks resolve.
fn project_file(file: &str, cwd: &str) -> bool {
    let path = std::path::Path::new(file);
    crate::discovery::PROJECT_INSTRUCTIONS.iter().any(|parts| {
        let mut base = Some(path);
        for part in parts.iter().rev() {
            match base {
                Some(p) if p.file_name() == Some(std::ffi::OsStr::new(part)) => base = p.parent(),
                _ => return false,
            }
        }
        base.is_some_and(|dir| {
            let resolved = || {
                let dir = crate::discovery::realpath(dir.to_str()?).ok()?;
                let cwd = crate::discovery::realpath(cwd).ok()?;
                Some(std::path::Path::new(&cwd).starts_with(dir))
            };
            !dir.as_os_str().is_empty()
                && (std::path::Path::new(cwd).starts_with(dir) || resolved() == Some(true))
        })
    })
}

/// What memory cost over `recorded` requests that recorded their context and `unrecorded`
/// that did not, from `rows`, its rows' shares on the line; None when no recorded request
/// is known or none that carried memory was priced. Also how many unrecorded requests read
/// memory back with none of those reads priced, which the figure leaves out.
///
/// Recorded requests are extrapolated over the unpriced ones that carried memory, as the
/// context row does. Unrecorded ones keep the memory files they were measured to read back,
/// extrapolated over the unpriced ones that read, and add the context memory loads, at the
/// recorded requests' mean, assuming they loaded it as often.
fn extrapolate(rows: &[ProfileShare], recorded: u64, unrecorded: u64) -> (Option<Estimate>, u64) {
    let (mut whole, mut loaded, mut reads) =
        (Sample::default(), Sample::default(), Sample::default());
    let (mut carried, mut read) = (0, 0);
    for c in rows {
        whole += c.recorded;
        loaded += c.loaded;
        reads += c.unrecorded;
        carried += c.recorded_requests;
        read += c.requests - c.recorded_requests;
    }
    // Reads none of which was priced have nothing to extrapolate from.
    let unpriced_reads = if reads.priced() == 0 { read } else { 0 };
    if recorded == 0 {
        return (None, unpriced_reads);
    }
    let Some(e) = whole.estimate(carried) else {
        return (None, unpriced_reads);
    };
    let (mut usd, mut margin) = (e.usd, e.margin);
    if let Some(r) = reads.estimate(read).filter(|_| unpriced_reads == 0) {
        usd += r.usd;
        margin = Margin::plus(margin, r.margin);
    }
    if unrecorded > 0 {
        let l = loaded
            .estimate(carried)
            .expect("the carriers' loaded context is priced with their whole cost");
        // Every recorded request's loaded context: a priced carrier's own, an unpriced one's
        // at the carriers' mean, as `l` takes it, and zero for one that carried none.
        let mut each = loaded;
        let mean = Usd::from_f64(if carried > 0 {
            l.usd / carried as f64
        } else {
            0.0
        });
        for _ in loaded.priced()..carried {
            each.add(mean);
        }
        for _ in carried..recorded {
            each.add(Usd::ZERO);
        }
        let all = each
            .estimate(recorded + unrecorded)
            .expect("every recorded request is in the sample");
        let scale = unrecorded as f64 / recorded as f64;
        usd += l.usd * scale;
        // The carriers' own margin grows with the extrapolation; the other covers how the
        // unrecorded requests may differ from the recorded ones.
        margin = Margin::plus(
            margin,
            Margin::plus(l.margin.map(|m| m.scale(scale)), all.margin),
        );
    }
    (Some(Estimate { usd, margin }), unpriced_reads)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A share whose recorded requests loaded `costs` where priced, and `extra` more
    /// requests that recorded no context but read memory back for $10 each.
    fn share(recorded: u64, costs: &[f64], extra: u64) -> ProfileShare {
        let mut s = ProfileShare {
            requests: recorded + extra,
            recorded_requests: recorded,
            ..ProfileShare::default()
        };
        for &usd in costs {
            s.sample.add(Usd::from_f64(usd));
            s.recorded.add(Usd::from_f64(usd));
            s.loaded.add(Usd::from_f64(usd));
        }
        for _ in 0..extra {
            s.sample.add(Usd::from_f64(10.0));
            s.unrecorded.add(Usd::from_f64(10.0));
        }
        s
    }

    #[test]
    fn a_project_file_is_a_known_name_at_or_above_the_session() {
        for (file, cwd, project) in [
            ("/repo/AGENTS.md", "/repo/src", true),
            ("/repo/CLAUDE.md", "/repo", true),
            ("/repo/.claude/CLAUDE.md", "/repo/src", true),
            // Below the session, beside it, or another name is no file of its repository.
            ("/repo/src/CLAUDE.md", "/repo", false),
            ("/other/AGENTS.md", "/repo", false),
            ("/repo/CLAUDE.local.md", "/repo", false),
            ("/repo/docs/CLAUDE.md.bak", "/repo/docs", false),
            ("CLAUDE.md", "/repo", false),
        ] {
            assert_eq!(project_file(file, cwd), project, "{file} at {cwd}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_project_file_counts_under_a_symlinked_session_directory() {
        let workspace = crate::test_support::Workspace::new();
        let temp = &workspace.home;
        let repo = temp.join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::os::unix::fs::symlink(&repo, temp.join("alias")).unwrap();
        let path = |p: &str| temp.join(p).to_str().unwrap().to_string();
        // The file is gone; its directory, reached through the alias, is not.
        assert!(project_file(&path("repo/CLAUDE.md"), &path("alias/src")));
        assert!(project_file(&path("alias/AGENTS.md"), &path("repo")));
        assert!(!project_file(&path("repo/src/CLAUDE.md"), &path("alias")));
    }

    #[test]
    fn only_sessions_t3_settled_to_a_claude_line_count_where_they_ran() {
        let native = crate::test_support::make_profile("native", "/home/.claude");
        let t3 = crate::test_support::make_t3_profile("t3", "/home/.claude");
        let profiles = [native.clone(), t3];
        let request = |id: &str, names: &[&str], unsettled: bool| {
            let mut r = crate::test_support::make_request();
            r.id = id.into();
            r.profiles = names
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .into();
            if unsettled {
                r.issues = vec![format!("{}: synthetic", crate::t3::UNSETTLED).into()];
            }
            r
        };
        let requests = [
            request("mine", &["native"], false),
            // Shared with the instance, or marked unsettled: no evidence for the native line.
            request("shared", &["native", "t3"], false),
            request("marked", &["native"], true),
        ];
        let mine: Vec<&Request> = requests.iter().collect();
        let ids = |line: &crate::rows::Group<'_>| {
            settled(line, &mine, &profiles)
                .iter()
                .map(|r| r.id.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&crate::rows::Group::one(&native)), ["mine"]);
        // Codex keeps every request, as its setting is read before attribution.
        let mut codex = native.clone();
        codex.harness = Harness::Codex;
        assert_eq!(
            ids(&crate::rows::Group::one(&codex)),
            ["mine", "shared", "marked"]
        );
    }

    #[test]
    fn sessions_count_where_their_latest_request_ran_and_records_win_over_directories() {
        let mut profile = crate::test_support::make_profile("profile", "/home/.claude");
        profile.memory = Some(crate::model::MemoryControl {
            value: true,
            owner: String::new(),
            off: String::new(),
            off_file: None,
            directory: None,
            unresolved: None,
            overrides: vec![],
            stored: false,
            instructed: [
                ("/repo".to_string(), Some(true)),
                ("/plain".to_string(), Some(false)),
                ("/gone".to_string(), None),
            ]
            .into_iter()
            .collect(),
        });
        let request = |session: &str, time: Millis, cwd: Option<&str>, agent: Agent| {
            let mut r = crate::test_support::make_request();
            r.time = time;
            let meta = std::sync::Arc::make_mut(&mut r.meta);
            meta.session = session.into();
            meta.cwd = cwd.map(Into::into);
            meta.agent = agent;
            r
        };
        let requests = [
            // Moved from a plain directory into the repository: counted where it ended.
            request("moved", 1, Some("/plain"), Agent::Main),
            request("moved", 2, Some("/repo"), Agent::Main),
            request("plain", 1, Some("/plain"), Agent::Main),
            // Gone and unrecorded: nothing settles it.
            request("gone", 1, Some("/gone"), Agent::Main),
            request("nowhere", 1, None, Agent::Main),
            // Gone, but its transcript recorded loading a project file.
            request("recorded", 1, Some("/gone"), Agent::Main),
            // A subagent's requests are no session of their own.
            request("subagent", 1, Some("/repo"), Agent::Subagent),
        ];
        let mine: Vec<&Request> = requests.iter().collect();
        let loaded = [
            ("recorded", "/gone/AGENTS.md"),
            // Loaded where it ran before, then resumed in a plain directory.
            ("plain", "/repo/CLAUDE.md"),
        ]
        .into_iter()
        .map(|(session, file)| (session.into(), [file.to_string()].into_iter().collect()))
        .collect();
        let line = crate::rows::Group::one(&profile);
        let p = places(&line, &mine, &loaded);
        assert_eq!(
            p,
            SessionPlaces {
                sessions: 5,
                instructed: 2,
                unknown: 2,
            }
        );
        // Two of five is not most, though three are unknown or known to be in one.
        assert!(!p.most());
        assert!(
            SessionPlaces {
                sessions: 4,
                instructed: 3,
                unknown: 1
            }
            .most()
        );
        assert!(
            !SessionPlaces {
                sessions: 4,
                instructed: 2,
                unknown: 0
            }
            .most()
        );
        assert!(!SessionPlaces::default().most());
    }

    #[test]
    fn unrecorded_requests_keep_their_reads_and_load_the_recorded_mean() {
        // Two recorded requests loaded $1 each; one unrecorded request read $10 back. It keeps
        // its $10 and loads $1 like them, never scaled on top of the recorded total.
        let e = extrapolate(&[share(2, &[1.0, 1.0], 1)], 2, 1).0.unwrap();
        assert!((e.usd - 13.0).abs() < 1e-9, "{}", e.usd);
        // Every recorded request loaded the same, so the extrapolation has no spread.
        assert_eq!(e.margin, Some(Margin::Usd(0.0)));
        // Priced in full and nothing unrecorded: exact.
        let e = extrapolate(&[share(2, &[1.0, 3.0], 0)], 2, 0).0.unwrap();
        assert_eq!((e.usd, e.margin), (4.0, None));
        // A recorded request's own reads stay in its total, never in the loaded mean.
        let mut s = share(1, &[], 0);
        s.recorded.add(Usd::from_f64(5.0));
        s.loaded.add(Usd::from_f64(2.0));
        let e = extrapolate(&[s], 1, 1).0.unwrap();
        assert!((e.usd - 7.0).abs() < 1e-9, "{}", e.usd);
        // Reads none of which was priced leave the rest standing, a lower bound that names
        // them, rather than making the line unknown.
        let mut s = share(100, &[1.0; 100], 0);
        s.requests += 1;
        let (e, unpriced) = extrapolate(&[s], 100, 1);
        assert!((e.unwrap().usd - 101.0).abs() < 1e-9);
        assert_eq!(unpriced, 1);
    }

    #[test]
    fn unpriced_carriers_count_at_the_carriers_mean_in_the_margin_too() {
        // 500 of 1,000 recorded requests carried memory and one was priced at $1; 1,000 more
        // recorded no context. Both the figure and its margin take the carriers at $1 each.
        let e = extrapolate(&[share(500, &[1.0], 0)], 1000, 1000).0.unwrap();
        assert!((e.usd - 1000.0).abs() < 1e-6);
        assert_eq!(e.margin, Some(Margin::Unmeasured));
        // With every carrier priced, half the recorded requests at $1 and half at zero, the
        // unrecorded half has a measured margin around the same $1,000.
        let e = extrapolate(&[share(500, &[1.0; 500], 0)], 1000, 1000)
            .0
            .unwrap();
        assert!((e.usd - 1000.0).abs() < 1e-6);
        let Some(Margin::Usd(m)) = e.margin else {
            panic!("a measured spread")
        };
        assert!(m > 0.0 && m < 100.0, "{m}");
        // Nothing priced among the carriers, or nothing recorded: unknown.
        assert_eq!(extrapolate(&[share(5, &[], 0)], 10, 0), (None, 0));
        assert_eq!(extrapolate(&[share(0, &[], 3)], 0, 3).0, None);
    }
}
