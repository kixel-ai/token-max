import { TTL_MILLISECONDS } from "./cache.js";
import type {
  Cost,
  Harness,
  Request,
  Scan,
  StructuralEvent,
  TimeoutCheckResult,
  TimeoutControl,
  TtlCheckResult,
} from "./model.js";
import {
  addCosts,
  priceComponents,
  unknownCost,
  type Catalog,
} from "./prices.js";
import { count, identity, object, ScanError } from "./safe.js";

const SOURCES = [
  "https://code.claude.com/docs/en/env-vars (2026-11-07)",
  "https://code.claude.com/docs/en/tools-reference (2026-11-07)",
  "https://code.claude.com/docs/en/settings-reference#env (2026-11-07)",
  "https://code.claude.com/docs/en/prompt-caching#choose-the-ttl-yourself (2026-11-07)",
  "https://learn.chatgpt.com/docs/config-file/config-reference (2026-11-07)",
  "https://learn.chatgpt.com/docs/config-file/config-advanced#profiles (2026-11-07)",
];

function version(value: string | null): number[] | null {
  return /^\d+\.\d+\.\d+$/.test(value ?? "")
    ? value!.split(".").map(Number)
    : null;
}

function native(harness: Harness, client: string | null): boolean {
  return harness === "claude"
    ? ["cli", "sdk-cli", "sdk-ts"].includes(client ?? "")
    : ["cli", "codex_cli_rs", "codex_exec"].includes(client ?? "");
}

/** Read only numeric limits, background flags and opaque polling targets from native calls. */
export function terminalArguments(options: {
  harness: Harness;
  tool: string | null;
  input: unknown;
  client: string | null;
  version: string | null;
}): StructuralEvent["terminal"] {
  const { harness, tool, client } = options;
  if (!native(harness, client)) return undefined;
  if (harness === "codex" && tool !== "write_stdin") return undefined;
  const v = version(options.version);
  if (!v) return undefined;
  let input = options.input;
  if (harness === "codex" && typeof input === "string") {
    // This is the native structured argument envelope, never tool output.
    try {
      input = JSON.parse(input);
    } catch {
      throw new ScanError("invalid terminal tool arguments");
    }
  }
  const a = object(input);
  if (harness === "claude" && tool === "Bash") {
    return {
      timeoutMs: count(a.timeout),
      background:
        a.run_in_background === undefined
          ? false
          : typeof a.run_in_background === "boolean"
            ? a.run_in_background
            : null,
      poll: null,
    };
  }
  if (
    harness === "claude" &&
    tool === "TaskOutput" &&
    v[0] === 2 &&
    v[1] === 1 &&
    v[2]! < 277 &&
    typeof a.task_id === "string" &&
    a.task_id.length
  ) {
    return {
      timeoutMs: count(a.timeout),
      background: true,
      poll: identity(a.task_id),
    };
  }
  if (
    harness === "codex" &&
    tool === "write_stdin" &&
    (a.chars === undefined || a.chars === "") &&
    typeof a.session_id === "number" &&
    Number.isSafeInteger(a.session_id) &&
    a.session_id >= 0
  ) {
    return {
      timeoutMs: count(a.yield_time_ms),
      background: true,
      poll: identity(String(a.session_id)),
    };
  }
  return undefined;
}

function applicable(r: Request | undefined): boolean {
  if (!r || !native(r.harness, r.client)) return false;
  const v = version(r.version);
  if (!v) return false;
  // These are conservative verified applicability floors, not claimed feature
  // introduction versions. Older/custom/hosted clients retain advisory guidance.
  return r.harness === "claude"
    ? v[0]! > 2 || (v[0] === 2 && (v[1]! > 1 || (v[1] === 1 && v[2]! >= 319)))
    : v[0]! > 0 || v[1]! >= 167;
}

function misses(catalog: Catalog, r: Request): Cost {
  if (!r.parts.length || r.evidence === "unknown")
    return unknownCost("linked continuation has no verified usage");
  return addCosts(
    r.parts.map((p) => {
      const all = priceComponents(catalog, r, p);
      // Keep total input for context-tier pricing. Only the cache component of
      // this copy is consumed; ordinary input still uses the original counters.
      const writes = priceComponents(catalog, r, {
        ...p,
        usage: {
          ...p.usage,
          read: 0,
        },
      }).cache;
      return addCosts([all.ordinary, writes]);
    }),
  );
}

/** Compare effective foreground ceilings with the selected TTL without inventing removable dollars. */
export function timeoutChecks(options: {
  scan: Scan;
  catalog: Catalog;
  ttls: TtlCheckResult[];
  since: number | undefined;
  now: number;
}): TimeoutCheckResult[] {
  const { scan, catalog, now } = options;
  return scan.profiles.flatMap((profile): TimeoutCheckResult[] => {
    if (!profile.timeout) return [];
    const requests = scan.requests.filter(
      (r) =>
        r.profiles.includes(profile.id) &&
        (options.since === undefined || r.time >= options.since),
    );
    const groups = new Map<
      string,
      {
        control: TimeoutControl;
        bucket: TimeoutCheckResult["bucket"];
        requests: Request[];
        events: StructuralEvent[];
      }
    >();
    const requestById = new Map(requests.map((r) => [r.id, r]));
    for (const r of requests) {
      const control =
        (r.cwd ? profile.timeoutDirectories?.[r.cwd] : undefined) ??
        profile.timeout;
      const bucket =
        profile.harness === "codex" || r.agent === "unknown" ? "all" : r.agent;
      const key = JSON.stringify([control, bucket]);
      const group = groups.get(key) ?? {
        control,
        bucket,
        requests: [],
        events: [],
      };
      group.requests.push(r);
      groups.set(key, group);
    }
    for (const e of scan.events) {
      if (
        !e.profiles.includes(profile.id) ||
        e.kind !== "tool-start" ||
        !e.terminal ||
        (options.since !== undefined && e.time < options.since)
      )
        continue;
      const control =
        (e.cwd ? profile.timeoutDirectories?.[e.cwd] : undefined) ??
        profile.timeout;
      const agent = e.request ? requestById.get(e.request)?.agent : undefined;
      const bucket =
        profile.harness === "codex" || !agent || agent === "unknown"
          ? "all"
          : agent;
      const key = JSON.stringify([control, bucket]);
      const group = groups.get(key) ?? {
        control,
        bucket,
        requests: [],
        events: [],
      };
      group.events.push(e);
      groups.set(key, group);
    }
    if (!groups.size)
      groups.set("default", {
        control: profile.timeout,
        bucket: "all",
        requests: [],
        events: [],
      });
    const endsByTool = new Map<string, StructuralEvent[]>();
    for (const e of scan.events)
      if (e.kind === "tool-end" && e.toolId) {
        const ends = endsByTool.get(e.toolId) ?? [];
        ends.push(e);
        endsByTool.set(e.toolId, ends);
      }
    return [...groups.values()].map(
      ({ control, bucket, requests, events }): TimeoutCheckResult => {
        const latest = [...requests].sort((a, b) => b.time - a.time)[0];
        const ttlControl =
          bucket === "all" ? null : (control.ttl ?? profile.ttl)[bucket];
        const currentTtl = ttlControl?.value ?? null;
        const replay = options.ttls.find(
          (c) =>
            c.profile === profile.id &&
            c.bucket === bucket &&
            c.current === currentTtl &&
            c.owner === ttlControl?.owner &&
            c.action !== "advisory",
        );
        const evaluationTtl = replay?.recommended ?? currentTtl;
        const ttlResolved =
          ttlControl?.resolved !== false || replay?.action === "change";
        const supported =
          control.resolved &&
          applicable(latest) &&
          (profile.harness === "codex" || ttlResolved);
        const at = (ttl: typeof currentTtl) =>
          !control.resolved || ttl === null || control.ceilingMs === null
            ? null
            : control.ceilingMs >= TTL_MILLISECONDS[ttl];
        const violation = ttlResolved ? at(evaluationTtl) : null;
        const action =
          profile.harness === "codex" || !supported || violation === null
            ? "advisory"
            : violation
              ? "change"
              : "keep";
        const continuations = new Map<string, Request[]>();
        for (const r of requests)
          for (const id of r.toolResults ?? []) {
            const linked = continuations.get(id) ?? [];
            linked.push(r);
            continuations.set(id, linked);
          }
        const seen = new Set<string>();
        let repeated = 0;
        const calls = events.map((e) => {
          const poll = e.terminal!.poll;
          const key = `${e.session}:${poll}`;
          if (poll && seen.has(key)) repeated++;
          if (poll) seen.add(key);
          const ends = (
            e.toolId ? (endsByTool.get(e.toolId) ?? []) : []
          ).filter(
            (end) =>
              end.origins.some((o) =>
                e.origins.some((start) => start.session === o.session),
              ) && end.time >= e.time,
          );
          const linked = (
            e.toolId ? (continuations.get(e.toolId) ?? []) : []
          ).filter((r) =>
            ends.some(
              (end) =>
                end.time <= r.time &&
                end.origins.some((o) => o.session === r.session),
            ),
          );
          return {
            event: e.id,
            tool: e.tool!,
            timeoutMs: e.terminal!.timeoutMs,
            background: e.terminal!.background,
            poll,
            elapsedMs: ends.length === 1 ? ends[0]!.time - e.time : null,
            linkedRequests: linked.map((r) => r.id),
          };
        });
        const linkedIds = new Set(calls.flatMap((c) => c.linkedRequests));
        const linked = requests.filter((r) => linkedIds.has(r.id));
        const unknownCalls = calls.filter(
          (c) => !c.linkedRequests.length,
        ).length;
        const observedCost = addCosts([
          ...linked.map((r) => misses(catalog, r)),
          ...(unknownCalls || !calls.length
            ? [
                unknownCost(
                  "tool-to-billed-continuation attribution is incomplete; no causal expiry cost is established",
                ),
              ]
            : []),
        ]);
        const current = !control.resolved
          ? null
          : profile.harness === "claude"
            ? control.ceilingMs === null
              ? null
              : `${control.ceilingMs} ms`
            : control.pollMs === null
              ? null
              : `${control.pollMs} ms per empty poll`;
        const reason =
          profile.harness === "codex"
            ? "background_terminal_max_timeout bounds one empty write_stdin poll (default 300000 ms), not command lifetime; do not raise it to avoid cache misses"
            : !supported
              ? `${control.reason}; native client/version or effective TTL applicability is unresolved or outside the verified policy`
              : violation === null
                ? "effective cache TTL or foreground ceiling is unknown"
                : violation
                  ? "effective foreground ceiling is at or above the evaluation TTL; lower both owning limits below the TTL where necessary"
                  : "effective foreground ceiling is below the evaluation TTL";
        return {
          check: "terminal-timeout",
          bucket,
          profile: profile.id,
          historyProfiles: [...new Set(requests.flatMap((r) => r.profiles))],
          evidence: "guidance",
          action,
          current,
          recommended:
            profile.harness === "claude" && supported && evaluationTtl !== null
              ? violation
                ? `< ${TTL_MILLISECONDS[evaluationTtl]} ms effective foreground ceiling`
                : current
              : null,
          owner: control.owner,
          control,
          reason,
          applicable: supported,
          currentTtl,
          evaluationTtl,
          atOrAboveCurrentTtl:
            ttlControl?.resolved === false ? null : at(currentTtl),
          atOrAboveEvaluationTtl: violation,
          since: options.since ?? requests[0]?.time ?? null,
          until: now,
          windowBasis:
            options.since === undefined
              ? "available selected usage; current controls applied to history; no terminal setting-change history established"
              : "explicit --since; current controls applied to history",
          baseline:
            "recorded requests and current effective terminal controls; foreground guidance evaluated after applicable TTL recommendations; observed charges remain at recorded TTL, billed response tier and speed",
          requests: requests.length,
          savingsUsd: null,
          monthlySavingsUsd: null,
          breakEven: null,
          observed: {
            calls: calls.length,
            foregroundRequests: calls.filter(
              (c) => c.tool === "Bash" && c.background === false,
            ).length,
            backgroundRequests: calls.filter(
              (c) => c.tool === "Bash" && c.background === true,
            ).length,
            pollCalls: calls.filter((c) => c.poll !== null).length,
            repeatedPollCalls: repeated,
            capHits: null,
            capHitReason:
              "no verified typed cap-hit marker; requested timeouts, interrupted flags, elapsed time and result text do not establish an actual cap hit",
            linkedRequests: [...linkedIds],
            missCost: observedCost,
            callsWithUnknownAttribution: unknownCalls,
          },
          calls,
          usageEstimates: [],
          assumptions: [
            "Claude effective foreground ceiling is max(BASH_MAX_TIMEOUT_MS default 600000, BASH_DEFAULT_TIMEOUT_MS default 120000); foreground waits do not issue model requests",
            "native background commands have separate lifetime rules; a foreground timeout may move work to the background; this check does not stop or time the user's processes",
            "verified applicability floor: Claude Code 2.1.319 CLI/SDK, Codex 0.167.0 CLI; these are not feature introduction versions; hosted/custom clients stay advisory",
            "latest recorded native client/version is assumed to remain applicable to the current profile; an unresolved subagent frontmatter TTL is advisory unless an actionable bucket TTL change supersedes it",
            "polling requires allowlisted native tool arguments and repeated opaque target identity within one session; Bash command/Read/output text is never inspected for loops",
            "observed cache-miss/write charges are noncached input plus cache writes on direct-parent linked continuations; include new-prefix writes, not proven expiry or removable waste; shared continuation charged once",
            "native Codex calls lack verified direct billed-continuation linkage here; requested service tier is never promoted to billed tier",
            "shortening a wait may add requests, cache reads, output or follow-on work; net removable savings remain unknown; API-price equivalents are not subscription bills; independent savings are not summed",
          ],
          uncertainty: [
            control.reason,
            "current files do not establish historical cap, client launch overrides or process lifetime; the historical 55-minute example is not setting evidence",
            "native TaskOutput was removed in 2.1.277; arbitrary shell loops and output-file reads have unknown polling attribution",
            ...observedCost.unknown,
          ],
          sources: SOURCES,
        };
      },
    );
  });
}
