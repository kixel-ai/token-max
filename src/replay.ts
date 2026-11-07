import { speedChecks } from "./fast.js";
import { timeoutChecks } from "./timeouts.js";
import { analyzeExposure } from "./exposure.js";
import type {
  BillingPart,
  CacheReplayEvidence,
  CheckResult,
  TtlCheckResult,
  CompactionCharge,
  Cost,
  Profile,
  Report,
  Request,
  Scan,
  StructuralEvent,
  Ttl,
  UsageEstimate,
} from "./model.js";
import {
  addCosts,
  pricePart,
  priceRequest,
  PRICE_SOURCES,
  serverCost,
  unknownCost,
  zeroCost,
  type Catalog,
} from "./prices.js";
import { estimateUsage } from "./estimates.js";
import { observedTtl } from "./cache.js";
import { cacheBucket as bucket, replayParts } from "./scenarios.js";
import { compactionChecks } from "./compaction.js";
import { idleChecks } from "./idle.js";
import { identity, ScanError } from "./safe.js";

const TTL_SOURCE =
  "https://code.claude.com/docs/en/prompt-caching (2026-11-07)";
const MINUTE = 60_000;

function inBucket(r: Request, p: BillingPart, selected: "main" | "subagent") {
  return bucket(r, p) === selected || bucket(r, p) === "unknown";
}

function versionSupported(version: string | null): boolean {
  if (version === null) return false;
  const parts = version.split(".").map(Number);
  if (parts.length !== 3 || parts.some((v) => !Number.isInteger(v)))
    return false;
  return (
    parts[0]! > 2 ||
    (parts[0] === 2 && (parts[1]! > 1 || (parts[1] === 1 && parts[2]! >= 242)))
  );
}

function proxyRequest(
  event: StructuralEvent,
  previous: Request | undefined,
): Request | null {
  if (event.preTokens === null || event.postTokens === null) return null;
  const pricing = event.pricing ?? previous?.parts.at(-1);
  if (!pricing) return null;
  return {
    evidence: "estimated",
    id: identity(event.id),
    harness: event.harness,
    session: event.session,
    time: event.time,
    endTime: event.time,
    agent: "subagent",
    model: pricing.model,
    provider: event.pricing?.provider ?? previous?.provider ?? null,
    api: "messages",
    source: "compaction",
    trigger: event.trigger,
    reference: null,
    turn: null,
    version: event.version,
    client: event.client,
    cwd: event.cwd,
    profiles: event.profiles,
    locations: [event.location],
    accounting: "individual",
    attempt: null,
    serverTools: {},
    parts: [
      {
        model: pricing.model,
        tier: pricing.tier,
        speed: pricing.speed,
        geo: pricing.geo,
        kind: "compaction",
        usage: {
          input: event.preTokens,
          output: event.postTokens,
          read: 0,
          write: 0,
          write5m: 0,
          write1h: 0,
          write30m: null,
          write12h: null,
          reasoning: null,
        },
      },
    ],
    issues: [
      "summary model/tier/provider assumed from preceding metadata in its own file; public API equivalent if endpoint is unrecorded",
    ],
  };
}

/** Associate summaries with recorded usage or estimate each unmatched boundary once. */
export function compactionCharges(
  scan: Scan,
  catalog: Catalog,
): CompactionCharge[] {
  const result: CompactionCharge[] = [];
  const claimed = new Set<string>();
  const lastBoundary = new Map<string, number>();
  const sessions = new Map<string, Request[]>();
  for (const r of [...scan.cacheContext.requests, ...scan.requests]) {
    const group = sessions.get(r.session) ?? [];
    group.push(r);
    sessions.set(r.session, group);
  }
  const byId = new Map(
    [...scan.cacheContext.requests, ...scan.requests].map((r) => [r.id, r]),
  );
  const selected = new Set(scan.events.map((e) => e.id));
  const boundaries = [...scan.cacheContext.boundaries, ...scan.events]
    .filter((e) => e.kind === "compaction")
    .sort((a, b) => a.time - b.time);
  for (const e of boundaries) {
    const aliases = [...new Set(e.origins.map((o) => o.session))];
    const session = [
      ...new Map(
        aliases
          .flatMap((alias) => sessions.get(alias) ?? [])
          .map((r) => [r.id, r]),
      ).values(),
    ].sort((a, b) => a.time - b.time);
    let linked = e.request ? byId.get(e.request) : undefined;
    let pairing = "explicit request identity";
    if (!linked) {
      const candidates = session.filter(
        (r) =>
          r.time <= e.time &&
          r.time > (lastBoundary.get(r.session) ?? -Infinity) &&
          r.parts.some((p) => p.kind === "compaction") &&
          !claimed.has(r.id),
      );
      if (candidates.length === 1) {
        linked = candidates[0];
        pairing =
          "estimated sequence attribution: one recorded summary since preceding boundary";
      }
    }
    for (const alias of aliases) lastBoundary.set(alias, e.time);
    const assumptions = [
      "boundary time and duration do not establish summary generation time",
      "precomputed does not prove idle compaction; no missing final boundary proves no summary",
    ];
    if (linked) {
      if (!linked.parts.some((p) => p.kind === "compaction"))
        for (const p of linked.parts) p.kind = "compaction";
      const cost = claimed.has(linked.id)
        ? zeroCost()
        : addCosts(
            linked.parts
              .filter((p) => p.kind === "compaction")
              .map((p) => pricePart(catalog, linked!, p)),
          );
      claimed.add(linked.id);
      result.push({
        event: e.id,
        request: linked.id,
        evidence: "measured",
        cost,
        ttlCost: { "5m": zeroCost(), "1h": zeroCost() },
        assumptions: [
          ...assumptions,
          pairing,
          "recorded usage already included in request costs; linked costs allocated to the first boundary only, do not add again",
        ],
      });
      continue;
    }
    const preceding = session.filter((r) => r.time <= e.time).at(-1);
    const proxy = e.harness === "claude" ? proxyRequest(e, preceding) : null;
    const cost = proxy
      ? priceRequest(catalog, proxy)
      : unknownCost(
          "summary usage/model or preTokens/postTokens proxy unavailable",
        );
    result.push({
      event: e.id,
      request: null,
      evidence: proxy ? "estimated" : "unknown",
      cost,
      ttlCost: { "5m": cost, "1h": cost },
      assumptions: [
        ...assumptions,
        "preTokens charged at ordinary input; postTokens is a summary-output proxy, not the full resumed context",
        "warm cache reads can lower input cost; cold writes can raise it up to 2x input before unrecorded system/tool/instruction overhead",
        "no cache-write split or generation timestamp: the same summary proxy is included in both outside-main TTL scenarios",
      ],
    });
  }
  return result.filter((charge) => selected.has(charge.event));
}

function defaultSince(
  requests: Request[],
  selectedBucket: "main" | "subagent",
): { change: number | null; mixed: boolean } {
  const cohorts = new Map<string, { ttl: Ttl; change: number | null }>();
  let ambiguous = false;
  for (const r of requests)
    for (const p of r.parts) {
      if (bucket(r, p) !== selectedBucket) continue;
      const ttl = observedTtl(p);
      if (!ttl) continue;
      // Sources/agent kinds can legitimately have different policies concurrently.
      // Missing source cannot establish a transition within the outside-main bucket.
      if (selectedBucket === "subagent" && !r.source) ambiguous = true;
      const key = [r.source, r.agent, p.kind, p.model].join(":");
      const previous = cohorts.get(key);
      cohorts.set(key, {
        ttl,
        change:
          previous && previous.ttl !== ttl
            ? r.time
            : (previous?.change ?? null),
      });
    }
  const mixed = new Set([...cohorts.values()].map((c) => c.ttl)).size > 1;
  if (mixed || ambiguous) return { change: null, mixed: true };
  const changes = [...cohorts.values()].flatMap((c) =>
    c.change === null ? [] : [c.change],
  );
  return { change: changes.length ? Math.max(...changes) : null, mixed: false };
}

function replayCost(options: {
  requests: Request[];
  scan: Scan;
  catalog: Catalog;
  selectedBucket: "main" | "subagent";
  ttl: Ttl;
  since: number;
  coldPrefixes?: boolean;
  estimates: Map<string, UsageEstimate>;
  attemptScenario?: "point" | "none" | "cold";
}): {
  comparison: Cost;
  fixed: Cost;
  predecessors: Set<string>;
  evidence: CacheReplayEvidence[];
} {
  const { requests, scan, catalog, selectedBucket, ttl, since } = options;
  const predecessors = new Set<string>();
  const evidence: CacheReplayEvidence[] = [];
  const parts: Cost[] = [];
  const fixed: Cost[] = [];
  for (const item of replayParts({
    requests,
    events: [...scan.cacheContext.boundaries, ...scan.events],
    catalog,
    estimates: options.estimates,
    ttls: { main: ttl, subagent: ttl, unknown: ttl },
    since,
    ...(options.coldPrefixes === undefined
      ? {}
      : { coldPrefixes: options.coldPrefixes }),
    ...(options.attemptScenario === undefined
      ? {}
      : { attemptScenario: options.attemptScenario }),
  })) {
    if (!item.selected || !inBucket(item.request, item.p, selectedBucket))
      continue;
    if (item.evidence) {
      evidence.push(item.evidence);
      for (const predecessor of item.evidence.predecessors)
        predecessors.add(predecessor.request);
    }
    parts.push(item.components.cache);
    fixed.push(item.components.ordinary, item.components.output);
  }
  return {
    comparison: addCosts(parts),
    fixed: addCosts(fixed),
    predecessors,
    evidence,
  };
}

function check(options: {
  scan: Scan;
  catalog: Catalog;
  profile: Profile;
  selectedBucket: "main" | "subagent";
  summaries: CompactionCharge[];
  estimates: Map<string, UsageEstimate>;
  since: number | undefined;
  now: number;
}): TtlCheckResult | null {
  const { scan, catalog, profile, selectedBucket, summaries, now } = options;
  const requests = [...scan.cacheContext.requests, ...scan.requests]
    .filter((r) => r.harness === "claude" && r.profiles.includes(profile.id))
    .sort((a, b) => a.time - b.time);
  const relevant = requests.filter((r) =>
    r.parts.some((p) => inBucket(r, p, selectedBucket)),
  );
  const boundaries =
    selectedBucket === "subagent"
      ? scan.events.filter(
          (e) =>
            e.harness === "claude" &&
            e.kind === "compaction" &&
            e.profiles.includes(profile.id),
        )
      : [];
  if (!relevant.length && !boundaries.length) return null;
  const window =
    options.since === undefined
      ? defaultSince(requests, selectedBucket)
      : { change: null, mixed: false };
  const change = window.change;
  const since =
    options.since ??
    change ??
    relevant.reduce(
      (n, r) => Math.min(n, r.time),
      boundaries.reduce((n, e) => Math.min(n, e.time), Infinity),
    );
  if (
    !relevant.some((r) => r.time >= since) &&
    !boundaries.some((e) => e.time >= since)
  )
    return null;
  const summaryIds = new Set(
    boundaries.filter((e) => e.time >= since).map((e) => e.id),
  );
  const summaryCost = {
    "5m": addCosts(
      summaries
        .filter((s) => summaryIds.has(s.event) && s.request === null)
        .map((s) => s.ttlCost["5m"]),
    ),
    "1h": addCosts(
      summaries
        .filter((s) => summaryIds.has(s.event) && s.request === null)
        .map((s) => s.ttlCost["1h"]),
    ),
  };
  const replay = (
    ttl: Ttl,
    extra: { coldPrefixes?: boolean; attemptScenario?: "none" | "cold" } = {},
  ) =>
    replayCost({
      requests,
      scan,
      catalog,
      selectedBucket,
      ttl,
      since,
      estimates: options.estimates,
      ...extra,
    });
  const short = replay("5m");
  const long = replay("1h");
  const comparisonCost = { "5m": short.comparison, "1h": long.comparison };
  const fixedCost = addCosts([
    short.fixed,
    ...relevant.filter((r) => r.time >= since).map(serverCost),
    summaryCost["5m"],
  ]);
  const cost = {
    "5m": addCosts([comparisonCost["5m"], fixedCost]),
    "1h": addCosts([comparisonCost["1h"], fixedCost]),
  };
  const coldComparison = replay("1h", { coldPrefixes: true }).comparison;
  const cold = addCosts([coldComparison, fixedCost]);
  const usageEstimates = requests
    .filter(
      (r) =>
        (r.time >= since &&
          r.parts.some((p) => inBucket(r, p, selectedBucket))) ||
        short.predecessors.has(r.id) ||
        long.predecessors.has(r.id),
    )
    .flatMap((r) =>
      options.estimates.has(r.id) ? [options.estimates.get(r.id)!] : [],
    );
  const estimatedAttempts = usageEstimates.filter((e) => e.part !== null);
  const unlinked = estimatedAttempts.length
    ? {
        noBilledAttempt: {
          "5m": replay("5m", { attemptScenario: "none" }).comparison,
          "1h": replay("1h", { attemptScenario: "none" }).comparison,
        },
        coldCache: {
          "5m": replay("5m", { attemptScenario: "cold" }).comparison,
          "1h": replay("1h", { attemptScenario: "cold" }).comparison,
        },
      }
    : null;
  const benefit =
    coldComparison.totalUsd !== null && comparisonCost["1h"].totalUsd !== null
      ? coldComparison.totalUsd - comparisonCost["1h"].totalUsd
      : null;
  const breakEven =
    benefit !== null && benefit > 0 && comparisonCost["5m"].totalUsd !== null
      ? (coldComparison.totalUsd! - comparisonCost["5m"].totalUsd) / benefit
      : null;
  const current = profile.ttl[selectedBucket];
  let recommended: Ttl | null = null;
  if (
    comparisonCost["5m"].totalUsd !== null &&
    comparisonCost["1h"].totalUsd !== null
  )
    recommended =
      comparisonCost["5m"].totalUsd! < comparisonCost["1h"].totalUsd!
        ? "5m"
        : comparisonCost["1h"].totalUsd! < comparisonCost["5m"].totalUsd!
          ? "1h"
          : current.value;
  const recentVersion =
    relevant.at(-1)?.version ?? requests.at(-1)?.version ?? null;
  const applies =
    versionSupported(recentVersion) &&
    current.value !== null &&
    current.reason !== "prompt caching is disabled";
  const action =
    recommended !== null && applies
      ? recommended !== current.value
        ? "change"
        : "keep"
      : "advisory";
  const savings =
    current.value && recommended
      ? comparisonCost[current.value].totalUsd! -
        comparisonCost[recommended].totalUsd!
      : null;
  const profiles = [
    ...new Set([
      ...relevant.flatMap((r) => r.profiles),
      ...boundaries.flatMap((e) => e.profiles),
    ]),
  ];
  const uncertainty = [
    "gap replay assumes the same requests, stable prefixes, and no extra work; shared caches and invalidations can change the outcome",
    "unknown source identifiers are never inferred from idle time",
    "unknown part/agent attribution is conditional in both possible buckets; per-check request counts and fixed costs are not additive",
    "current profile settings are applied to history; project/CLI settings and agent frontmatter may override user defaults",
    "unknown client lifetime and unlogged discarded summaries are not measured zero",
  ];
  if (!versionSupported(recentVersion))
    uncertainty.push(
      "current supported client version is not established; recorded latest version is absent or predates TTL controls",
    );
  if (!current.value) uncertainty.push(current.reason);
  if (profiles.length > 1)
    uncertainty.push(
      "shared history has ambiguous profile provenance; this is a conditional replay for the named current profile, not assigned historical spend",
    );
  for (const estimate of usageEstimates)
    uncertainty.push(estimate.reason, ...estimate.assumptions);
  uncertainty.push(
    ...relevant.filter((r) => r.time >= since).flatMap((r) => r.issues),
    ...requests
      .filter(
        (r) => short.predecessors.has(r.id) || long.predecessors.has(r.id),
      )
      .flatMap((r) => r.issues),
    ...short.evidence.map((e) => e.reason),
    ...long.evidence.map((e) => e.reason),
    ...cost["5m"].unknown,
    ...cost["1h"].unknown,
  );
  return {
    check: "claude-cache-ttl",
    windowBasisKind:
      options.since !== undefined
        ? "explicit"
        : change !== null
          ? "ttl-transition"
          : "available",
    profile: profile.id,
    historyProfiles: profiles,
    bucket: selectedBucket,
    evidence: recommended ? "estimated" : "unknown",
    action,
    current: current.value,
    recommended,
    owner: current.owner,
    since,
    until: now,
    windowBasis:
      options.since !== undefined
        ? "explicit --since; current settings applied to history"
        : change !== null
          ? "since latest observed write-TTL transition; not proof of a setting change"
          : window.mixed
            ? "available history; mixed agent/source TTLs do not establish a shared setting transition"
            : "available history; no recorded setting change, current settings applied to history",
    baseline: `current ${selectedBucket} TTL for ${profile.root}`,
    requests: relevant.filter((r) => r.time >= since).length,
    cost,
    summaryCost,
    comparisonCost,
    fixedCost,
    usageEstimates,
    cacheEvidence: [...short.evidence, ...long.evidence],
    savingsUsd: savings,
    monthlySavingsUsd:
      savings !== null && now > since
        ? (savings * 30 * 24 * 60 * MINUTE) / (now - since)
        : null,
    breakEven:
      breakEven === null
        ? null
        : `1h breaks even at ${(breakEven * 100).toFixed(1)}% of modeled warm-prefix reuse in 5-60 minute gaps`,
    sensitivity: {
      coldPrefix1hUsd: cold.totalUsd,
      coldPrefix1hComparisonUsd: coldComparison.totalUsd,
      warmReuseBreakEvenFraction: breakEven,
      unlinked,
    },
    assumptions: [
      current.reason,
      "latest recorded client version is assumed to remain applicable to current profile controls",
      "1h writes use documented 2x input; first visible request keeps observed reads from prefixes warmed before the window",
      "reuse conversions are limited to the previously cached prefix; new input stays a write",
      "ordinary input, output, server fees and unmatched summary proxies are held identical under fixed behavior and cancel; comparisonCost contains cache charges only, even when fixedCost is unknown",
      "unlinked-attempt estimates are comparison-only; original usage/coverage and historical spend remain unknown, never excluded or assigned zero billing",
      "cache evidence retains known, estimated and unknown predecessor witnesses, including pre-window attempts and possible cohorts with missing metadata; request counts and charges cover only the selected window",
      "per-check savings share baselines and must not be summed",
    ],
    uncertainty: [...new Set(uncertainty)],
    sources: [TTL_SOURCE, ...PRICE_SOURCES],
  };
}

/** Build the shared report without summing savings from independent checks. */
export function analyze(options: {
  scan: Scan;
  catalog: Catalog;
  since?: number;
  now?: number;
}): Report {
  const { scan, catalog } = options;
  const now = options.now ?? Date.now();
  if (
    scan.since !== null &&
    options.since !== undefined &&
    options.since < scan.since
  )
    throw new ScanError(
      "comparison window predates the scanned billing window",
    );
  const since = options.since ?? scan.since ?? undefined;
  const compactions = compactionCharges(scan, catalog);
  const estimates = estimateUsage(scan);
  const groups = new Map<string, Request[]>();
  for (const r of scan.requests) {
    const key = [...r.profiles].sort().join(":");
    const group = groups.get(key) ?? [];
    group.push(r);
    groups.set(key, group);
  }
  const historical: Report["historical"] = [...groups.entries()].map(
    ([key, requests]) => ({
      bucket: requests[0]!.profiles.length > 1 ? `shared:${key}` : key,
      profiles: requests[0]!.profiles,
      requests: requests.length,
      evidence: "estimated",
      cost: addCosts(requests.map((r) => priceRequest(catalog, r))),
    }),
  );
  const checks: CheckResult[] = [];
  for (const profile of scan.profiles.filter((p) => p.harness === "claude"))
    for (const selectedBucket of ["main", "subagent"] as const) {
      const result = check({
        scan,
        catalog,
        profile,
        selectedBucket,
        summaries: compactions,
        estimates,
        since,
        now,
      });
      if (result) checks.push(result);
    }
  checks.push(
    ...compactionChecks({
      scan,
      catalog,
      summaries: compactions,
      estimates,
      ttls: checks.filter((c) => c.check === "claude-cache-ttl"),
      since,
      now,
    }),
  );
  checks.push(
    ...idleChecks({
      scan,
      catalog,
      summaries: compactions,
      estimates,
      ttls: checks.filter((c) => c.check === "claude-cache-ttl"),
      windows: checks.filter((c) => c.check === "compaction-window"),
      since,
      now,
    }),
  );
  checks.push(...speedChecks({ scan, catalog, since, now }));
  checks.push(
    ...timeoutChecks({
      scan,
      catalog,
      since,
      now,
      ttls: checks.filter((c) => c.check === "claude-cache-ttl"),
    }),
  );
  return {
    schemaVersion: 1,
    generatedAt: new Date(now).toISOString(),
    scan,
    prices: {
      source: catalog.source,
      fetchedAt: catalog.fetchedAt,
      offline: catalog.offline,
    },
    historical,
    compactions,
    exposure: analyzeExposure(scan.exposure, scan.requests, catalog),
    checks,
    assumptions: [
      "host-local read-only analysis; network access is only for public prices",
      "measured usage is replayed at present public API rates, not historical invoices or subscription savings",
      "compaction summary proxies are separate from recorded request charges",
      "unlogged or unresumed summary generation is unknown; a missing final boundary does not establish zero cost",
      "Codex has no user cache-TTL control; its usage is priced but no TTL change is recommended",
      "no summed per-check or joint savings total",
    ],
  };
}
