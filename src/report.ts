import { homedir } from "node:os";
import { wrapText } from "./terminal.js";
import type {
  CacheReplayEvidence,
  CheckResult,
  Cost,
  Report,
} from "./model.js";
import { display, terminalText } from "./safe.js";

/** Text presentation never changes analysis or the JSON report. */
export interface ReportOptions {
  width?: number;
  color?: boolean;
  unicode?: boolean;
  details?: boolean;
}

function usd(value: number): string {
  // Preserve the sign of small changes instead of rounding them to a free $0.00.
  if (value !== 0 && Math.abs(value) < 0.01)
    return value < 0 ? "-<$0.01" : "<$0.01";
  return `${value < 0 ? "-" : ""}$${Math.abs(value).toFixed(2)}`;
}
function money(cost: Cost): string {
  return cost.totalUsd === null
    ? `unknown (${usd(cost.knownUsd)} known subtotal)`
    : usd(cost.totalUsd);
}
function instant(value: number | null): string {
  return value === null ? "unknown" : new Date(value).toISOString();
}
function estimate(value: number | null): string {
  return value === null ? "unknown" : usd(value);
}
function setting(value: string | number | boolean | null): string {
  return value === null
    ? "unknown"
    : typeof value === "number"
      ? `${value} tokens`
      : String(value);
}

/** Render all findings in a wrapping flow, with complete evidence available in details. */
export function formatReport(
  report: Report,
  options: ReportOptions = {},
): string {
  const lines: string[] = [];
  const arrow = options.unicode ? "→" : "->";
  function line(text = "", indent = 0, style = 0) {
    // Escape metadata before wrapping or adding our own ANSI. Long paths are
    // hard-wrapped, never shortened; wide characters use terminal cell widths.
    const wrapped = wrapText(terminalText(text), {
      ...(options.width === undefined ? {} : { width: options.width }),
      indent,
    });
    for (const part of wrapped.split("\n"))
      lines.push(
        options.color && style ? `\x1b[${style}m${part}\x1b[0m` : part,
      );
  }
  function section(title: string) {
    line();
    line(title, 0, 1);
  }
  function field(name: string, value: string) {
    line(`${name}: ${value}`, 2);
  }
  function notes(name: string, values: string[]) {
    for (const value of new Set(values)) field(name, value);
  }
  function cost(name: string, value: Cost) {
    field(name, money(value));
    notes("Unknown", value.unknown);
    notes("Assumption", value.assumptions);
  }
  function witnesses(values: CacheReplayEvidence[]) {
    for (const evidence of values) {
      field(
        "Cache evidence",
        `${evidence.request}/${evidence.part} ${evidence.ttl}: ${evidence.state}, ${evidence.outcome}; ${evidence.reason}`,
      );
      for (const p of evidence.predecessors)
        field(
          "Predecessor",
          `${p.request}/${p.part}: ${p.state}, prefix ${p.prefixEvidence}, cohort ${p.cohortKnown ? "known" : "incomplete"}, ${instant(p.time)}; ${p.reason}`,
        );
      if (!evidence.predecessors.length) field("Predecessor", "unrecorded");
    }
  }
  const profiles = new Map(report.scan.profiles.map((p) => [p.id, p]));
  function profileName(id: string): string {
    const p = profiles.get(id);
    return p ? `${p.harness} ${display(p.root)}` : display(id);
  }
  function shortProfile(id: string): string {
    const p = profiles.get(id);
    return p ? `${p.harness} ${home(p.root)}` : home(id);
  }

  if (!options.details) {
    summary(report, { line, section, profileName: shortProfile, arrow });
    return lines.join("\n") + "\n";
  }

  line("token-max", 0, 1);
  line("Public API-price estimates, not subscription bills.");
  const requests = report.scan.requests;
  const earliest = requests[0]?.time ?? null;
  field(
    "Window",
    `${report.scan.since === null ? "available history" : "explicit --since"}; ${instant(report.scan.since ?? earliest)} ${arrow} ${report.generatedAt}`,
  );
  field("Profiles", `${profiles.size} selected; settings remain read-only`);
  for (const p of profiles.values()) {
    field("Profile", profileName(p.id));
    if (options.details) {
      field("Settings", display(p.settingsFile));
      for (const alias of p.aliases) field("Alias", display(alias));
      for (const history of p.histories) field("History", display(history));
    }
  }
  const coverage = report.scan.coverage;
  field(
    "Coverage",
    `${coverage.requests} selected requests in ${report.scan.snapshots.length} files; ${requests.filter((r) => r.evidence === "measured").length} with measured usage; ${coverage.missingUsage} missing usage${coverage.skippedLines ? `; ${coverage.skippedLines} unreadable transcript lines in ${coverage.skippedFiles} files skipped (truncated or interleaved writes)` : ""}`,
  );
  field(
    "Prices",
    `${report.prices.offline ? "offline/local" : "online"}; ${display(report.prices.source)}; catalog timestamp ${report.prices.fetchedAt ?? "unknown"}`,
  );

  const groups = [
    ["change", "Recommended changes", 36],
    ["advisory", "Advisory findings", 33],
    ["keep", "Keep current settings", 0],
  ] as const;
  const changes = report.checks.filter((c) => c.action === "change").length;
  section(
    changes
      ? `${changes} recommended change${changes === 1 ? "" : "s"}`
      : "No applicable setting changes",
  );
  if (!report.scan.snapshots.length)
    line(
      "No transcript histories found. Select an existing profile with --claude-dir or --codex-dir; run after using that agent.",
    );
  else if (!coverage.requests)
    line(
      "No selected requests. Try an earlier --since or select another profile.",
    );
  else if (!report.checks.length)
    line(
      "No applicable checks for the selected requests. Usage coverage does not establish that every setting is optimal.",
    );
  else if (!changes)
    line(
      report.checks.some((c) => c.action === "advisory")
        ? "Review advisory findings for unresolved settings or missing evidence."
        : "Available checks favor current settings. Coverage limits still apply.",
    );
  if (report.checks.length)
    line("Each check has its own baseline. Do not sum independent savings.");

  for (const [action, title, style] of groups) {
    const results = report.checks.filter((c) => c.action === action);
    if (!results.length) continue;
    section(title);
    for (const result of results) {
      line();
      line(
        `${action.toUpperCase()}  ${result.check} / ${result.bucket}  [${result.evidence}]`,
        0,
        style,
      );
      field("Profile", profileName(result.profile));
      field("Owner", display(result.owner));
      field("Current", setting(result.current));
      field("Suggested", setting(result.recommended));
      field("Reason", reason(result));
      field("Baseline", result.baseline);
      field(
        "Window",
        `${instant(result.since)} ${arrow} ${instant(result.until)}; ${result.requests} requests; ${result.windowBasis}`,
      );
      field(
        result.check === "fast-mode"
          ? "Estimated rate-only saving vs current speed"
          : "Estimated net saving vs baseline",
        `${result.savingsUsd === null ? "unknown" : usd(result.savingsUsd)} over window; ${result.monthlySavingsUsd === null ? "unknown" : usd(result.monthlySavingsUsd)} / 30 days`,
      );
      if (result.check === "claude-cache-ttl") {
        field(
          "Replay cost",
          `5m ${money(result.cost["5m"])}; 1h ${money(result.cost["1h"])}`,
        );
        field(
          "Cache comparison",
          `5m ${money(result.comparisonCost["5m"])}; 1h ${money(result.comparisonCost["1h"])}`,
        );
        if (result.fixedCost.totalUsd === null)
          field(
            "Fixed costs",
            "Identical fixed costs remain unknown and cancel in the cache comparison.",
          );
      } else if (result.check === "idle-compaction") {
        field(
          "Effective idle threshold",
          result.threshold === null ? "unknown" : `${result.threshold} tokens`,
        );
        for (const scenario of [
          result.currentSettings,
          result.afterRecommendations,
        ]) {
          field(
            "Idle comparison",
            `${scenario.baseline}; ${scenario.applicable ? "applicable" : "inapplicable"}; on ${money(scenario.cost.on)}, off ${money(scenario.cost.off)}`,
          );
          field(
            "On minus off",
            `${estimate(scenario.deltaUsd)} over window; ${estimate(scenario.monthlyDeltaUsd)} / 30 days`,
          );
          field(
            "Idle periods",
            `${scenario.periods.length} request/final periods; ${scenario.periods.filter((p) => p.status === "eligible").length} eligible, ${scenario.periods.filter((p) => p.confirmed).length} confirmed; ${scenario.periods.filter((p) => p.status === "censored").length} censored; ${scenario.periods.filter((p) => p.resume === null).length} final periods without observed return`,
          );
        }
        const s = result.afterRecommendations.sensitivity;
        field(
          "Idle sensitivity, on minus off",
          `closed ${estimate(s.closedDeltaUsd)}; open/awake ${estimate(s.openDeltaUsd)}; larger summary/full prefix ${estimate(s.largerSummaryDeltaUsd)}; late trigger ${estimate(s.lateTriggerDeltaUsd)}. The aggregate estimate decides.`,
        );
        field(
          "Extra re-read break-even per resumed summary",
          estimate(s.rereadBreakEvenUsdPerResumedSummary),
        );
      } else if (result.check === "fast-mode") {
        field("Standard-speed replacement", result.replacement);
        field(
          "Observed response-tier premium (rate-only API equivalent)",
          `${money(result.observed.premium)} over selected window; ${estimate(result.observed.monthlyPremiumUsd)} / 30 days`,
        );
        field(
          "Speed coverage",
          `${result.observed.premiumParts} premium, ${result.observed.standardParts} standard, ${result.observed.unknownParts} unknown billed parts`,
        );
        field(
          "Current-setting premium estimate (rate-only)",
          `${money(result.currentEstimate.premium)}; ${result.currentEstimate.pricedParts} priced premium parts, ${result.currentEstimate.unknownParts} unknown parts`,
        );
        field(
          "Speed limits",
          "Recorded cache categories held constant; Claude cache-switch effects excluded. API-price equivalents, not subscription savings. Work held constant; latency value excluded.",
        );
      } else {
        field(
          "Models",
          result.models
            .map((m) => (m === null ? "unknown" : display(m)))
            .join(", "),
        );
        field(
          "Conditional TTL baseline",
          `main ${result.ttls.main ?? "unknown"}; subagent ${result.ttls.subagent ?? "unknown"}; window comparison follows available TTL recommendations`,
        );
        field("Current-window baseline cost", money(result.baselineCost));
        const suggested = result.scenarios.find(
          (s) => s.window === result.recommended,
        );
        field(
          "Suggested-window cost",
          suggested ? money(suggested.cost) : "unknown",
        );
        field(
          "Existing summaries (linked/proxy)",
          money(result.observedSummaryCost),
        );
        if (suggested)
          field(
            "Net sensitivity",
            `larger observed summary/prefix ${estimate(suggested.sensitivity.largerSummarySavingsUsd)}; cold summary ${estimate(suggested.sensitivity.coldSummarySavingsUsd)}; extra re-read break-even ${estimate(suggested.sensitivity.rereadBreakEvenUsd)}. Conditional scenarios; the aggregate estimate decides.`,
          );
      }
      if (result.usageEstimates.length)
        field(
          "Unlinked attempts",
          `${result.usageEstimates.filter((e) => e.evidence === "estimated").length} estimated inputs; ${result.usageEstimates.filter((e) => e.evidence === "unknown").length} unsupported inputs. Original usage and historical billing stay unknown.`,
        );
      if (result.breakEven) field("Break-even", result.breakEven);
      field(
        "Limits",
        `${result.check === "compaction-window" ? "Earlier compaction loses detail and can add re-reads or changed work. " : ""}Fixed-behavior replay with current settings applied to history; monthly values extrapolate the comparison window. Full assumptions and uncertainty: --details.`,
      );
      if (action === "change")
        field(
          "Next",
          `Review evidence with --details, then update ${display(result.owner)} yourself.`,
        );
      if (action === "advisory")
        field(
          "Next",
          "Review evidence and resolve current ownership/coverage before changing a setting; advisory findings do not affect the exit code.",
        );
      if (options.details) {
        for (const id of result.historyProfiles)
          field("History profile", profileName(id));
        if (result.check === "claude-cache-ttl") {
          for (const ttl of ["5m", "1h"] as const) {
            cost(`${ttl} replay`, result.cost[ttl]);
            cost(`${ttl} cache comparison`, result.comparisonCost[ttl]);
            cost(`${ttl} summary proxies`, result.summaryCost[ttl]);
          }
          cost("Identical fixed costs", result.fixedCost);
          field(
            "Cold-prefix 1h replay",
            result.sensitivity.coldPrefix1hUsd === null
              ? "unknown"
              : usd(result.sensitivity.coldPrefix1hUsd),
          );
          field(
            "Cold-prefix 1h cache comparison",
            result.sensitivity.coldPrefix1hComparisonUsd === null
              ? "unknown"
              : usd(result.sensitivity.coldPrefix1hComparisonUsd),
          );
          if (result.sensitivity.unlinked) {
            field(
              "Attempt sensitivity",
              "Conditional cache-only scenarios, not guaranteed bounds; the aggregate estimate decides.",
            );
            for (const ttl of ["5m", "1h"] as const) {
              cost(
                `${ttl}, no billed/cache-affecting attempt`,
                result.sensitivity.unlinked.noBilledAttempt[ttl],
              );
              cost(
                `${ttl}, cold estimated input`,
                result.sensitivity.unlinked.coldCache[ttl],
              );
            }
          }
        } else if (result.check === "idle-compaction") {
          for (const scenario of [
            result.currentSettings,
            result.afterRecommendations,
          ]) {
            field("Idle scenario", scenario.baseline);
            field(
              "Conditional TTL baseline",
              `main ${scenario.ttls.main ?? "unknown"}; outside-main ${scenario.ttls.subagent ?? "unknown"}; project-specific policies are retained in the period evidence`,
            );
            for (const w of scenario.windows)
              field(
                "Window control",
                `${w.owner}; ${w.value ?? "unknown"} tokens; models ${w.models.map((m) => m ?? "unknown").join(", ")}`,
              );
            cost("Idle on", scenario.cost.on);
            cost("Idle off", scenario.cost.off);
            cost(
              "On adjustment to common recorded baseline",
              scenario.adjustment.on,
            );
            cost(
              "Off adjustment to common recorded baseline",
              scenario.adjustment.off,
            );
            for (const size of scenario.byContext)
              field(
                "Idle by context",
                `${size.tokens ?? "unknown"} tokens; ${size.periods} periods; on minus off ${estimate(size.deltaUsd)}`,
              );
            for (const p of scenario.periods) {
              field(
                "Idle period",
                `${p.request}; resume ${p.resume ?? "not observed"}; context ${p.contextTokens ?? "unknown"} tokens; gap ${p.gapMs}ms; ${p.status}; ${p.returnTiming}; ${p.reason}; ${p.outcome}; boundary ${p.event ?? "unobserved"}`,
              );
              field(
                "Activation",
                `${p.activation ?? "unknown"}; ${p.activationBasis}; version ${p.version ?? "unknown"}; client ${p.client ?? "unknown"}; flags ${p.flagState ?? "unknown"}`,
              );
              cost("Idle summary", p.summaryCost);
              if (p.calibration)
                field(
                  "Idle geometry",
                  `${p.calibration.summaryTokens} summary output tokens; ${p.calibration.prefixTokens} full resumed prefix tokens; boundaries ${p.calibration.events.join(", ")}`,
                );
            }
            for (const cohort of scenario.activationCohorts)
              field(
                "Activation cohort",
                `${cohort.version}/${cohort.client}/${cohort.flagState}: ${cohort.applied}/${cohort.eligible} applied (${cohort.fraction})`,
              );
            notes("Idle unknown", scenario.unknown);
          }
        } else if (result.check === "fast-mode") {
          cost("Observed response-tier premium", result.observed.premium);
          cost(
            "Current-setting premium estimate",
            result.currentEstimate.premium,
          );
        } else {
          cost("Current-window baseline", result.baselineCost);
          cost("Existing summaries", result.observedSummaryCost);
          for (const scenario of result.scenarios) {
            field(
              "Candidate window",
              `${scenario.window} tokens; ${scenario.affectedRequests} affected requests; ${scenario.added.length} added compactions`,
            );
            cost("Candidate full cost", scenario.cost);
            cost(
              "Candidate baseline comparison",
              scenario.baselineComparisonCost,
            );
            cost("Candidate comparison", scenario.comparisonCost);
            field(
              "Candidate net saving vs baseline",
              estimate(scenario.savingsUsd),
            );
            field(
              "Candidate sensitivity",
              `larger observed summary/prefix net saving ${estimate(scenario.sensitivity.largerSummarySavingsUsd)}; cold summary net saving ${estimate(scenario.sensitivity.coldSummarySavingsUsd)}; extra re-read break-even ${estimate(scenario.sensitivity.rereadBreakEvenUsd)}`,
            );
            for (const added of scenario.added) {
              field(
                "Added compaction",
                `at ${added.request}; ${added.inputTokens} summary input tokens; ${added.summaryTokens} summary output tokens; ${added.prefixTokens} resumed-prefix tokens`,
              );
              for (const basis of added.basis)
                field(
                  "Geometry basis",
                  `boundary ${basis.event}; resumed request ${basis.request}`,
                );
              cost("Added summary", added.summaryCost);
              cost("Resumed prefix", added.prefixCost);
            }
            witnesses(scenario.cacheEvidence);
          }
        }
        for (const estimate of result.usageEstimates) {
          field(
            "Attempt",
            `${estimate.request}: ${estimate.evidence}; basis ${estimate.basis.join(", ") || "unavailable"}; ${estimate.reason}; output budget ${estimate.maxOutputTokens ?? "unknown"} (not observed output)`,
          );
          notes("Attempt assumption", estimate.assumptions);
        }
        if (result.check !== "idle-compaction" && result.check !== "fast-mode")
          witnesses(result.cacheEvidence);
        notes("Assumption", result.assumptions);
        notes("Uncertainty", result.uncertainty);
        notes("Source", result.sources);
      }
    }
  }

  section("Recorded usage and coverage");
  field(
    "Attribution",
    `source/trigger ${coverage.sourceKnown}/${coverage.requests}; service tier ${coverage.tierKnown}/${coverage.requests}`,
  );
  field(
    "Snapshot",
    `${report.scan.snapshotStartedAt} ${arrow} ${report.scan.snapshotFinishedAt}; ${coverage.deferredFiles} files with unterminated final lines deferred`,
  );
  notes("Coverage limit", coverage.issues);
  if (options.details)
    for (const root of report.scan.roots)
      field(
        "Root",
        `${root.status} ${root.harness} ${display(root.path)} (${root.origin})`,
      );
  for (const group of report.historical) {
    field(
      "Estimated recorded-request cost",
      `${group.profiles.map(profileName).join(", ")}; ${group.requests} requests; ${money(group.cost)}${group.profiles.length > 1 ? "; shared, profile attribution unknown" : ""}`,
    );
    notes("Unknown", group.cost.unknown);
    if (options.details) notes("Pricing assumption", group.cost.assumptions);
  }
  if (!report.historical.length)
    line("Recorded-request cost: unknown; no selected request evidence.");
  const exposure = report.scan.exposure;
  field(
    "Component coverage",
    `${exposure.components.length} revisions; ${exposure.requests.filter((r) => r.linkage === "wire").length}/${exposure.requests.length} requests with wire exposure; ${exposure.invocations.length} invocation observations; ${report.exposure.calibration.length} calibrated model/client cohorts. Candidate exposure stays unpriced; typed/nested visibility is incomplete.`,
  );
  if (report.compactions.length)
    field(
      "Compactions",
      `${report.compactions.filter((c) => c.request !== null).length} linked usage; ${report.compactions.filter((c) => c.evidence === "estimated").length} summary proxies; ${report.compactions.filter((c) => c.evidence === "unknown").length} unknown costs`,
    );
  if (options.details) {
    for (const charge of report.compactions) {
      field(
        "Compaction",
        `${charge.event}; request ${charge.request ?? "unlinked"}; ${charge.evidence}`,
      );
      cost("Summary cost", charge.cost);
      notes("Summary assumption", charge.assumptions);
    }
    notes("Report assumption", report.assumptions);
    notes("Exposure assumption", report.exposure.assumptions);
  }
  line();
  line(
    options.details
      ? "All check evidence shown. --json provides the complete structured report (schema version 1)."
      : "Use --details for all check evidence, assumptions, uncertainty and sources; --json for the complete structured report.",
  );
  return lines.join("\n") + "\n";
}

function reason(result: CheckResult): string {
  if (result.check === "fast-mode") return result.reason;
  if (result.check === "idle-compaction")
    return result.recommended === null
      ? "Idle prerequisites are inapplicable or required cost/eligibility inputs remain unpriced."
      : "Lowest estimated aggregate on/off net cost, including unresumed summaries; sensitivity does not veto the point estimate.";
  if (result.evidence === "guidance")
    return `Provider guidance; dollars ${result.savingsUsd === null ? "unknown" : "estimated separately"}. ${result.assumptions.join("; ")}`;
  if (result.recommended === null)
    return "No priced winner established; missing evidence is unknown, never zero.";
  if (result.action === "advisory")
    return "Conditional replay winner; current effective setting or applicability is unresolved.";
  return result.check === "compaction-window"
    ? "Lowest estimated aggregate net cost, including added summaries and resumed-prefix charges; existing boundaries retained."
    : "Lowest estimated aggregate cache cost under the stated replay assumptions.";
}

const DAY = 86400000;
function home(path: string): string {
  const dir = homedir();
  return path === dir || path.startsWith(dir + "/")
    ? "~" + path.slice(dir.length)
    : path;
}
function about(value: number): string {
  const rounded = Math.abs(value) >= 10 ? Math.round(value) : value;
  return usd(rounded).replace(/\.00$/, "");
}
function days(since: number | null, until: number | null): string {
  if (since === null || until === null) return "the window";
  const count = Math.max(1, Math.round((until - since) / DAY));
  return `${count} day${count === 1 ? "" : "s"}`;
}
function count(value: number): string {
  return value.toLocaleString("en-US");
}

/** Plain name for the setting a check is about. */
function settingName(result: CheckResult): string {
  switch (result.check) {
    case "claude-cache-ttl":
      return result.bucket === "main"
        ? "Main-agent prompt cache TTL"
        : "Subagent prompt cache TTL";
    case "compaction-window": {
      const models = result.models.filter((m): m is string => m !== null);
      return `Auto-compact window${models.length ? ` (${models.join(", ")})` : ""}`;
    }
    case "idle-compaction":
      return "Idle compaction";
    case "fast-mode":
      return "Fast mode";
  }
}
function settingValue(result: CheckResult, value: unknown): string {
  if (value === null || value === undefined) return "unknown";
  if (result.check === "idle-compaction") return value ? "on" : "off";
  if (result.check === "fast-mode")
    return value === "standard" ? "off" : String(value);
  if (typeof value === "number") return `${count(value)} tokens`;
  return String(value);
}

/** One plain sentence on why a check could not reach a verdict. */
function unresolved(result: CheckResult): string {
  if (result.check === "fast-mode") return capitalize(result.reason) + ".";
  if (result.recommended !== null)
    return result.recommended === result.current
      ? "The replay favors the current value, but could not confirm it applies to your current sessions."
      : `The replay favors ${settingValue(result, result.recommended)}, but could not confirm the current setting applies to your sessions.`;
  if (result.requests === 0) return "No requests in this window to compare.";
  if (result.check === "claude-cache-ttl") {
    if (!result.coverage?.priced)
      return "No requests with measured cache usage to compare.";
    return `5m and 1h differ by only ${about(Math.abs(result.oneHourSavingsUsd))} over ${days(result.since, result.until)} on measured requests, too close to call.`;
  }
  if (result.check === "compaction-window") {
    if (result.current === null)
      return result.uncertainty.some((u) =>
        u.includes("default is not established"),
      )
        ? "No window is set for this model and its default is not documented."
        : "The current window setting could not be resolved.";
    const reasons = result.scenarios.flatMap((s) => s.comparisonCost.unknown);
    if (reasons.some((r) => r.includes("compaction summary")))
      return "Too few recorded compactions to model a different window.";
    return "Too many requests lack recorded usage to compare windows.";
  }
  const periods = result.afterRecommendations.periods;
  if (
    periods.length &&
    periods.every((p) => p.status === "censored" || p.status === "ineligible")
  )
    return "Sessions were never idle long enough for it to trigger, so it made no difference.";
  if (!periods.some((p) => p.status === "eligible"))
    return "No idle gaps could be compared with it on and off.";
  if (result.afterRecommendations.deltaUsd === 0)
    return "On and off cost the same on recorded idle gaps, so neither is cheaper.";
  return "Idle summaries or resumed sessions could not be priced.";
}
function capitalize(text: string): string {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

/** The default report: one verdict per setting, in plain words. */
function summary(
  report: Report,
  out: {
    line: (text?: string, indent?: number, style?: number) => void;
    section: (title: string) => void;
    profileName: (id: string) => string;
    arrow: string;
  },
) {
  const { line, section, arrow } = out;
  const coverage = report.scan.coverage;
  const requests = report.scan.requests;
  const since = report.scan.since ?? requests[0]?.time ?? null;
  const until = Date.parse(report.generatedAt);
  const many = report.scan.profiles.length > 1;
  line("token-max", 0, 1);
  line(
    "Estimated with public API prices, not your subscription bill. Nothing was changed.",
  );
  line();
  line(
    coverage.requests && since !== null
      ? `Read ${count(coverage.requests)} requests from ${new Date(since).toISOString().slice(0, 10)} to ${report.generatedAt.slice(0, 10)} (${days(since, until)}):`
      : `No requests found${report.scan.since === null ? "" : ` since ${new Date(report.scan.since).toISOString().slice(0, 10)}`} in:`,
  );
  for (const p of report.scan.profiles) line(out.profileName(p.id), 2);
  if (coverage.missingUsage)
    line(
      `${count(coverage.missingUsage)} requests have no recorded usage and are left out of comparisons.`,
    );
  if (coverage.skippedLines)
    line(
      `${count(coverage.skippedLines)} unreadable transcript lines in ${count(coverage.skippedFiles)} files were skipped.`,
    );
  if (!report.scan.snapshots.length)
    line(
      "No transcript histories found. Select a profile with --claude-dir or --codex-dir after using that agent.",
    );
  else if (!coverage.requests)
    line("Try an earlier --since or select another profile.");

  // A check for a working directory that no longer exists has no setting left
  // to change; show it only when it is the sole result for its setting.
  const key = (r: CheckResult) =>
    `${r.check}:${r.bucket}:${r.profile}:${r.check === "compaction-window" ? r.models.join(",") : ""}`;
  // A comparison with no requests is not a verdict, whatever the tie-break said.
  // Window coverage counts only changed parts, so zero there can still be a keep.
  // Fast mode warns from current settings alone, so it never needs requests.
  const empty = (r: CheckResult) =>
    (r.check === "claude-cache-ttl" || r.check === "compaction-window") &&
    r.requests === 0;
  const action = (r: CheckResult) => (empty(r) ? "advisory" : r.action);
  const decided = new Set(
    report.checks.filter((r) => action(r) !== "advisory").map(key),
  );
  // Only a vanished directory is proven irrelevant; any other unresolved owner,
  // such as a project override, stays visible beside the resolved verdict.
  const checks = report.checks.filter(
    (r) =>
      !(
        action(r) === "advisory" &&
        r.check === "fast-mode" &&
        r.reason.includes("working directory no longer exists") &&
        decided.has(key(r))
      ),
  );
  const name = (r: CheckResult) =>
    settingName(r) + (many ? ` [${out.profileName(r.profile)}]` : "");
  const changes = checks.filter((r) => action(r) === "change");
  line();
  line(
    changes.length
      ? `${changes.length} setting change${changes.length === 1 ? "" : "s"} recommended.`
      : "No setting changes recommended.",
    0,
    1,
  );
  const window = (r: CheckResult) => days(r.since, r.until);
  const monthly = (r: CheckResult) =>
    r.monthlySavingsUsd === null
      ? ""
      : ` (about ${about(r.monthlySavingsUsd)} per 30 days)`;
  if (changes.length) {
    section("Change");
    for (const r of changes) {
      line(
        `${name(r)}: ${settingValue(r, r.current)} ${arrow} ${settingValue(r, r.recommended)}`,
        2,
        36,
      );
      if (r.savingsUsd !== null)
        line(
          `Saves about ${about(r.savingsUsd)} over ${window(r)}${monthly(r)}${coverageNote(r)}.`,
          6,
        );
      else if (r.check === "fast-mode") line(r.reason + ".", 6);
      line(`Set in ${home(r.owner)}`, 6);
    }
  }
  const keeps = checks.filter((r) => action(r) === "keep");
  if (keeps.length) {
    section("Keep");
    for (const r of keeps) {
      line(`${name(r)}: ${settingValue(r, r.current)}`, 2);
      if (r.check === "claude-cache-ttl" && r.oneHourSavingsUsd !== 0) {
        const other = r.current === "1h" ? "5m" : "1h";
        const margin = Math.abs(r.oneHourSavingsUsd);
        line(
          `${r.current} costs about ${about(margin)} less than ${other} over ${r.windowBasisKind === "ttl-transition" ? `the ${window(r)} since it last changed` : window(r)}${coverageNote(r)}.`,
          6,
        );
      } else if (r.check === "compaction-window")
        line("No other window was cheaper on recorded sessions.", 6);
      else if (r.check === "idle-compaction")
        line(
          r.current
            ? "Summarizing idle sessions cost less than re-reading them."
            : "Summarizing idle sessions would not have saved money.",
          6,
        );
    }
  }
  const unknown = checks.filter((r) => action(r) === "advisory");
  if (unknown.length) {
    section("Not enough evidence");
    for (const r of unknown) {
      line(`${name(r)}: ${settingValue(r, r.current)}`, 2);
      line(unresolved(r), 6);
    }
  }
  line();
  line(
    `Prices: ${report.prices.offline ? "cached" : "fetched"} from ${home(report.prices.source)}${report.prices.fetchedAt ? ` (${report.prices.fetchedAt.slice(0, 10)})` : ""}.`,
  );
  line(
    "Each line compares one setting with everything else unchanged; savings do not add up.",
  );
  line(
    "--details shows the evidence and assumptions behind each line; --json is for scripts.",
  );
}
function coverageNote(r: CheckResult): string {
  const c = r.coverage;
  if (!c || c.priced === c.total) return "";
  // Percent, because one request can bill several parts.
  const percent = Math.floor((c.priced / c.total) * 1000) / 10;
  return `, using the ${percent}% of requests with measured usage`;
}
