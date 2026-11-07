#!/usr/bin/env node
import { homedir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { scan } from "./reader.js";
import { loadPrices } from "./prices.js";
import { analyze } from "./replay.js";
import { display, ScanError } from "./safe.js";
import type { Cost, Report } from "./model.js";

const HELP = `token-max [--since YYYY-MM-DD|ISO] [--json] [--offline]
  --claude-dir DIR   Repeatable Claude config root; explicit roots limit the run
  --codex-dir DIR    Repeatable Codex home; includes sessions and archived_sessions
  --prices-file FILE  Read a local models.dev catalog, without network access
  --price-cache FILE  Price cache (default: ~/.cache/token-max/models.json)

Reads transcripts/settings only. No prompt/reply text is retained or printed.
Exit 0: no applicable change; 1: change recommended; 2: scan or price failure.
`;
function money(cost: Cost): string {
  return cost.totalUsd === null
    ? `unknown ($${cost.knownUsd.toFixed(2)} known subtotal)`
    : `$${cost.totalUsd.toFixed(2)}`;
}

/** Minimal plain output, backed by the same report used for JSON and later UI work. */
export function formatReport(report: Report): string {
  const lines = [
    "token-max: public API-price estimates, not subscription bills",
    `Prices: ${display(report.prices.source)}${report.prices.offline ? " (offline/local)" : ""}`,
    "Roots:",
  ];
  for (const root of report.scan.roots)
    lines.push(
      `  ${root.status} ${root.harness} ${display(root.path)} (${root.origin})`,
    );
  lines.push(
    `Snapshot: ${report.scan.snapshots.length} files, ${report.scan.coverage.requests} requests; ${report.scan.coverage.deferredFiles} unterminated final lines deferred`,
  );
  lines.push(
    `Coverage: source/trigger ${report.scan.coverage.sourceKnown}/${report.scan.coverage.requests}, service tier ${report.scan.coverage.tierKnown}/${report.scan.coverage.requests}, missing usage ${report.scan.coverage.missingUsage}`,
  );
  for (const group of report.historical) {
    const names = group.profiles.map(
      (id) => report.scan.profiles.find((p) => p.id === id)?.root ?? id,
    );
    lines.push(
      `Estimated recorded-request cost${names.length > 1 ? " (shared, profile attribution unknown)" : ""}: ${names.map(display).join(", ")}: ${money(group.cost)}`,
    );
    for (const reason of group.cost.unknown) lines.push(`  Unknown: ${reason}`);
  }
  for (const result of report.checks) {
    const profile = report.scan.profiles.find((p) => p.id === result.profile)!;
    lines.push(
      `${result.action.toUpperCase()} [${result.evidence}] ${display(profile.root)} ${result.bucket}: ${result.current ?? "unknown"} -> ${result.recommended ?? "unknown"}`,
    );
    lines.push(`  Owner: ${display(result.owner)}; ${result.windowBasis}`);
    lines.push(
      `  5m ${money(result.cost["5m"])}; 1h ${money(result.cost["1h"])} (summary proxies: ${money(result.summaryCost["5m"])}/${money(result.summaryCost["1h"])})`,
    );
    if (result.fixedCost.totalUsd === null)
      lines.push(
        `  TTL comparison: 5m ${money(result.comparisonCost["5m"])}; 1h ${money(result.comparisonCost["1h"])}; identical fixed costs remain unknown.`,
      );
    if (result.usageEstimates.length)
      lines.push(
        `  Unlinked attempts: ${result.usageEstimates.filter((e) => e.evidence === "estimated").length} estimated inputs; ${result.usageEstimates.filter((e) => e.evidence === "unknown").length} unsupported. Original usage and historical billing stay unknown.`,
      );
    if (result.sensitivity.unlinked) {
      const scenarios = result.sensitivity.unlinked;
      lines.push(
        `  Attempt sensitivity (cache comparison only): no billed/cache-affecting attempt 5m ${money(scenarios.noBilledAttempt["5m"])}, 1h ${money(scenarios.noBilledAttempt["1h"])}; cold estimated input 5m ${money(scenarios.coldCache["5m"])}, 1h ${money(scenarios.coldCache["1h"])}. Conditional scenarios, not guaranteed bounds; the aggregate estimate decides.`,
      );
    }
    if (result.savingsUsd !== null)
      lines.push(
        `  Estimated net saving vs current: $${result.savingsUsd.toFixed(2)}${result.monthlySavingsUsd === null ? "" : `; $${result.monthlySavingsUsd.toFixed(2)}/30 days`}`,
      );
    if (result.breakEven)
      lines.push(
        `  Sensitivity: ${result.breakEven}; cold-prefix 1h comparison $${result.sensitivity.coldPrefix1hComparisonUsd!.toFixed(2)} (fixed costs excluded). The aggregate estimate decides the winner.`,
      );
    for (const estimate of result.usageEstimates)
      lines.push(
        `  Attempt ${estimate.request}: ${estimate.evidence}; basis ${estimate.basis.join(", ") || "unavailable"}; ${estimate.reason}${estimate.maxOutputTokens === null ? "" : `; recorded output limit ${estimate.maxOutputTokens} (not observed output)`}`,
      );
    for (const assumption of result.assumptions)
      lines.push(`  Assumption: ${assumption}`);
    for (const uncertainty of result.uncertainty)
      lines.push(`  Uncertainty: ${uncertainty}`);
  }
  if (report.compactions.length)
    lines.push(
      `Compactions: ${report.compactions.filter((c) => c.request !== null).length} linked usage; ${report.compactions.filter((c) => c.evidence === "estimated").length} summary proxies; ${report.compactions.filter((c) => c.evidence === "unknown").length} unknown costs`,
    );
  for (const issue of report.scan.coverage.issues)
    lines.push(`Coverage: ${issue}`);
  lines.push(...report.assumptions);
  return lines.join("\n") + "\n";
}

/** Run one atomic scan: failures emit only a safe diagnostic, never partial findings. */
export async function main(args = process.argv.slice(2)): Promise<number> {
  const json = args.includes("--json");
  try {
    let values;
    try {
      ({ values } = parseArgs({
        args,
        options: {
          since: { type: "string" },
          json: { type: "boolean" },
          offline: { type: "boolean" },
          help: { type: "boolean", short: "h" },
          "claude-dir": { type: "string", multiple: true },
          "codex-dir": { type: "string", multiple: true },
          "prices-file": { type: "string" },
          "price-cache": { type: "string" },
        },
      }));
    } catch {
      throw new ScanError("invalid arguments; use --help");
    }
    if (values.help) {
      process.stdout.write(HELP);
      return 0;
    }
    let since: number | undefined;
    if (values.since) {
      const value = values.since;
      if (!/^\d{4}-\d\d-\d\d(?:$|T.*(?:Z|[+-]\d\d:\d\d)$)/.test(value))
        throw new ScanError(
          "--since requires YYYY-MM-DD (UTC) or an ISO instant with timezone",
        );
      since = Date.parse(value);
      const date = value.slice(0, 10);
      if (
        !Number.isFinite(since) ||
        new Date(Date.parse(date + "T00:00:00Z")).toISOString().slice(0, 10) !==
          date
      )
        throw new ScanError("invalid --since date");
    }
    const options = {
      ...(since === undefined ? {} : { since }),
      ...(values["claude-dir"] ? { claudeDirs: values["claude-dir"] } : {}),
      ...(values["codex-dir"] ? { codexDirs: values["codex-dir"] } : {}),
    };
    const data = await scan(options);
    const catalog = await loadPrices({
      cache:
        values["price-cache"] ??
        join(
          process.env.XDG_CACHE_HOME ?? join(homedir(), ".cache"),
          "token-max",
          "models.json",
        ),
      ...(values["prices-file"] ? { file: values["prices-file"] } : {}),
      ...(values.offline === undefined ? {} : { offline: values.offline }),
    });
    const report = analyze({
      scan: data,
      catalog,
      ...(since === undefined ? {} : { since }),
    });
    process.stdout.write(
      values.json ? JSON.stringify(report) + "\n" : formatReport(report),
    );
    return report.checks.some((c) => c.action === "change") ? 1 : 0;
  } catch (error) {
    const safe =
      error instanceof ScanError
        ? error
        : new ScanError("analysis failed; no report produced");
    const diagnostic = {
      error: safe.message,
      ...(safe.location ? { location: safe.location } : {}),
    };
    process.stderr.write(
      json
        ? JSON.stringify(diagnostic) + "\n"
        : `token-max: ${safe.message}${safe.location ? ` at ${display(safe.location.file)}:${safe.location.line}` : ""}\n`,
    );
    return 2;
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href)
  process.exitCode = await main();
