#!/usr/bin/env node
import pkg from "../package.json" with { type: "json" };
import { homedir } from "node:os";
import { join } from "node:path";
import { parseArgs } from "node:util";
import { scan } from "./reader.js";
import { loadPrices } from "./prices.js";
import { analyze } from "./replay.js";
import { display, ScanError } from "./safe.js";
import { formatReport } from "./report.js";
import { setImmediate } from "node:timers/promises";
import { wrapText } from "./terminal.js";
import { audit } from "./hosts.js";

const HELP = `token-max: cheaper agent settings from local usage

Usage: token-max [options]

  --since YYYY-MM-DD|ISO  Select from midnight UTC or an instant with timezone
  --ssh HOST              Audit an explicit OpenSSH destination (repeatable)
  --include-local         Add this host when --ssh is selected
  --claude-dir DIR        Select a Claude profile (repeatable)
  --codex-dir DIR         Select a Codex home, including archived sessions (repeatable)
  --details               Show all check evidence, assumptions, uncertainty and sources
  --plain                 Plain text without color or decorative Unicode
  --json                  Complete structured report, schema version 1
  --offline               Use cached public prices without network access
  --prices-file FILE      Use a local models.dev catalog without network access
  --price-cache FILE      Cache path (default: ~/.cache/token-max/models.json)
  -v, --version           Show the package version
  -h, --help              Show this help

Without profile flags, discover local Claude/Codex profiles. Explicit profiles
limit the entire run. Current settings are applied to the selected history;
checks can use different windows and baselines. Independent savings do not add.
--ssh selects remote discovery only; --include-local adds local discovery.
Directory flags apply only locally and require --include-local with --ssh.
SSH requires existing authentication/host keys and remote Node >=22 or Bun
>=1.4.4 (1.x) on POSIX Linux/macOS. No remote install or price fetch occurs.

Examples:
  token-max --since 2026-11-01
  token-max --claude-dir ~/.claude-work --codex-dir ~/.codex --details
  token-max --offline --plain
  token-max --json > report.json

Public API-price estimates, not subscription bills. Reads transcripts/settings
only; never edits settings or retains/prints prompt, reply or component text.
Color is automatic on terminals; NO_COLOR disables it. Progress goes to stderr
only on interactive terminals, never with --json or redirected output.
SIGINT/SIGTERM cancel with exit 2 and no success report.

Exit 0: no applicable change (advisory findings may remain).
Exit 1: change recommended. Exit 2: scan, pricing, argument or cancellation failure.
`;

/** Run one atomic scan: failures emit only a safe diagnostic, never partial findings. */
async function main(args = process.argv.slice(2)): Promise<number> {
  const json = args.includes("--json");
  const cancellation = new AbortController();
  let auditingSsh = false;
  const cancel = () => {
    // Preserve the local I/O escape hatch after all SSH children are reaped.
    // A repeated signal must never exit while an audit still owns processes.
    if (cancellation.signal.aborted && !auditingSsh) process.exit(2);
    cancellation.abort();
  };
  process.on("SIGINT", cancel);
  process.on("SIGTERM", cancel);
  try {
    let values;
    try {
      ({ values } = parseArgs({
        args,
        options: {
          since: { type: "string" },
          ssh: { type: "string", multiple: true },
          "include-local": { type: "boolean" },
          json: { type: "boolean" },
          offline: { type: "boolean" },
          plain: { type: "boolean" },
          details: { type: "boolean" },
          version: { type: "boolean", short: "v" },
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
      process.stdout.write(
        HELP.split("\n")
          .map((row) => {
            const description = /^ {2}-.*? {2,}(?=\S)/.exec(row);
            return wrapText(row, {
              ...(process.stdout.isTTY
                ? { width: process.stdout.columns || 80 }
                : {}),
              ...(description ? { continuation: description[0].length } : {}),
            });
          })
          .join("\n"),
      );
      return 0;
    }
    if (values.version) {
      process.stdout.write(pkg.version + "\n");
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
    const interactive =
      !!process.stdout.isTTY &&
      !!process.stderr.isTTY &&
      process.env.TERM !== "dumb";
    const progress = interactive && !values.json;
    function phase(message: string) {
      cancellation.signal.throwIfAborted();
      if (progress) process.stderr.write(`token-max: ${message}...\n`);
    }
    const options = {
      signal: cancellation.signal,
      ...(since === undefined ? {} : { since }),
      ...(values["claude-dir"] ? { claudeDirs: values["claude-dir"] } : {}),
      ...(values["codex-dir"] ? { codexDirs: values["codex-dir"] } : {}),
    };
    if (values["include-local"] && !values.ssh?.length)
      throw new ScanError("--include-local requires --ssh");
    if (
      values.ssh?.length &&
      !values["include-local"] &&
      (values["claude-dir"] || values["codex-dir"])
    )
      throw new ScanError(
        "directory flags are local-only; add --include-local or omit them",
      );
    phase(
      values.ssh?.length
        ? "Scanning selected hosts"
        : "Scanning local histories",
    );
    let data;
    auditingSsh = !!values.ssh?.length;
    try {
      data = values.ssh?.length
        ? await audit({
            ...options,
            ssh: values.ssh,
            includeLocal: !!values["include-local"],
          })
        : await scan(options);
    } finally {
      auditingSsh = false;
    }
    phase("Loading public prices");
    const catalog = await loadPrices({
      signal: cancellation.signal,
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
    phase("Checking settings");
    const report = analyze({
      scan: data,
      catalog,
      ...(since === undefined ? {} : { since }),
    });
    const output = values.json
      ? JSON.stringify(report) + "\n"
      : formatReport(report, {
          ...(process.stdout.isTTY
            ? { width: process.stdout.columns || 80 }
            : {}),
          color:
            !!process.stdout.isTTY &&
            !values.plain &&
            !process.env.NO_COLOR &&
            process.env.TERM !== "dumb",
          unicode:
            !!process.stdout.isTTY &&
            !values.plain &&
            process.env.TERM !== "dumb" &&
            /utf-?8/i.test(
              process.env.LC_ALL ||
                process.env.LC_CTYPE ||
                process.env.LANG ||
                "",
            ),
          details: !!values.details,
        });
    // Let pending signals run after synchronous analysis/rendering, before the
    // single success write. A cancelled run must never produce recommendations.
    await setImmediate();
    cancellation.signal.throwIfAborted();
    process.stdout.write(output);
    return report.checks.some((c) => c.action === "change") ? 1 : 0;
  } catch (error) {
    const safe = cancellation.signal.aborted
      ? new ScanError("cancelled; no report produced")
      : error instanceof ScanError
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
  } finally {
    process.off("SIGINT", cancel);
    process.off("SIGTERM", cancel);
  }
}

process.exitCode = await main();
