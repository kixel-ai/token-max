import { wrapText } from "./terminal.js";
import type { CheckResult, Cost, Report } from "./model.js";
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
  const profiles = new Map(report.scan.profiles.map((p) => [p.id, p]));
  function profileName(id: string): string {
    const p = profiles.get(id);
    return p ? `${p.harness} ${display(p.root)}` : display(id);
  }

  line("token-max", 0, 1);
  line("Public API-price estimates, not subscription bills.");
  const requests = report.scan.requests;
  const earliest = requests.reduce<number | null>(
    (time, r) => Math.min(time ?? r.time, r.time),
    null,
  );
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
    `${coverage.requests} selected requests in ${report.scan.snapshots.length} files; ${requests.filter((r) => r.evidence === "measured").length} with measured usage; ${coverage.missingUsage} missing usage`,
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
      field("Current", result.current ?? "unknown");
      field("Suggested", result.recommended ?? "unknown");
      field("Reason", reason(result));
      field("Baseline", result.baseline);
      field(
        "Window",
        `${instant(result.since)} ${arrow} ${instant(result.until)}; ${result.requests} requests; ${result.windowBasis}`,
      );
      field(
        "Estimated net saving vs baseline",
        `${result.savingsUsd === null ? "unknown" : usd(result.savingsUsd)} over window; ${result.monthlySavingsUsd === null ? "unknown" : usd(result.monthlySavingsUsd)} / 30 days`,
      );
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
      if (result.usageEstimates.length)
        field(
          "Unlinked attempts",
          `${result.usageEstimates.filter((e) => e.evidence === "estimated").length} estimated inputs; ${result.usageEstimates.filter((e) => e.evidence === "unknown").length} unsupported inputs. Original usage and historical billing stay unknown.`,
        );
      if (result.breakEven) field("Break-even", result.breakEven);
      field(
        "Limits",
        "Fixed-behavior replay with current settings applied to history; monthly values extrapolate the comparison window. Full assumptions and uncertainty: --details.",
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
        for (const estimate of result.usageEstimates) {
          field(
            "Attempt",
            `${estimate.request}: ${estimate.evidence}; basis ${estimate.basis.join(", ") || "unavailable"}; ${estimate.reason}; output budget ${estimate.maxOutputTokens ?? "unknown"} (not observed output)`,
          );
          notes("Attempt assumption", estimate.assumptions);
        }
        for (const evidence of result.cacheEvidence) {
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
  if (result.evidence === "guidance")
    return `Provider guidance; dollars ${result.savingsUsd === null ? "unknown" : "estimated separately"}. ${result.assumptions.join("; ")}`;
  if (result.recommended === null)
    return "No priced winner established; missing evidence is unknown, never zero.";
  if (result.action === "advisory")
    return "Conditional replay winner; current effective setting or applicability is unresolved.";
  return "Lowest estimated aggregate cache cost under the stated replay assumptions.";
}
