import type {
  BillingPart,
  CheckResult,
  CompactionCharge,
  Cost,
  Profile,
  Report,
  Request,
  Scan,
  StructuralEvent,
  Ttl,
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
import { identity } from "./safe.js";

const TTL_SOURCE =
  "https://code.claude.com/docs/en/prompt-caching (2026-11-07)";
const MINUTE = 60_000;

function observedTtl(p: BillingPart): Ttl | null {
  if ((p.usage.write5m ?? 0) > 0 && p.usage.write1h === 0) return "5m";
  if ((p.usage.write1h ?? 0) > 0 && p.usage.write5m === 0) return "1h";
  return null;
}

function bucket(r: Request, p: BillingPart): "main" | "subagent" | "unknown" {
  if (p.kind === "compaction" || p.kind === "advisor") return "subagent";
  return r.agent;
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

function replayPart(options: {
  p: BillingPart;
  ttl: Ttl;
  previous: { time: number; prefix: number } | undefined;
  time: number;
  coldPrefixes: boolean;
}): BillingPart {
  const { p, ttl, previous, time, coldPrefixes } = options;
  const u = { ...p.usage };
  if (u.read === null || u.write === null) return { ...p, usage: u };
  const gap = previous ? time - previous.time : null;
  const observed = observedTtl(p);
  if (gap !== null && gap >= 5 * MINUTE && gap < 60 * MINUTE) {
    if (ttl === "5m" && observed !== "5m") {
      u.write += u.read;
      u.read = 0;
    }
    if (ttl === "1h" && observed !== "1h") {
      const reusable = Math.min(
        u.write,
        Math.max(0, (previous?.prefix ?? 0) - u.read),
      );
      u.write -= reusable;
      u.read += reusable;
    }
    if (ttl === "1h" && coldPrefixes) {
      u.write += u.read;
      u.read = 0;
    }
  }
  // New prefix tokens remain writes, even when an older prefix survives longer.
  u.write5m = ttl === "5m" ? u.write : 0;
  u.write1h = ttl === "1h" ? u.write : 0;
  return { ...p, usage: u };
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
  for (const r of scan.requests) {
    const group = sessions.get(r.session) ?? [];
    group.push(r);
    sessions.set(r.session, group);
  }
  const byId = new Map(scan.requests.map((r) => [r.id, r]));
  for (const e of scan.events.filter((e) => e.kind === "compaction")) {
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
  return result;
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
}): Cost {
  const { requests, scan, catalog, selectedBucket, ttl, since } = options;
  const previous = new Map<string, { time: number; prefix: number }>();
  const parts: Cost[] = [];
  const boundaries = new Map<string, number[]>();
  for (const e of scan.events)
    if (e.kind === "compaction") {
      for (const origin of e.origins) {
        const times = boundaries.get(origin.session) ?? [];
        times.push(e.time);
        boundaries.set(origin.session, times);
      }
    }
  for (const r of requests) {
    for (const p of r.parts) {
      if (bucket(r, p) !== selectedBucket) continue;
      const key = [r.session, p.model, r.provider, p.speed, p.kind].join(":");
      let prior = previous.get(key);
      if (
        prior &&
        boundaries
          .get(r.session)
          ?.some((time) => time > prior!.time && time <= r.time)
      )
        prior = undefined;
      if (r.time >= since)
        parts.push(
          pricePart(
            catalog,
            r,
            replayPart({
              p,
              ttl,
              previous: prior,
              time: r.time,
              coldPrefixes: options.coldPrefixes ?? false,
            }),
          ),
        );
      if (p.usage.read !== null && p.usage.write !== null)
        previous.set(key, {
          time: r.time,
          prefix: p.usage.read + p.usage.write,
        });
      else previous.delete(key);
    }
  }
  return addCosts(parts);
}

function check(options: {
  scan: Scan;
  catalog: Catalog;
  profile: Profile;
  selectedBucket: "main" | "subagent";
  summaries: CompactionCharge[];
  since: number | undefined;
  now: number;
}): CheckResult | null {
  const { scan, catalog, profile, selectedBucket, summaries, now } = options;
  const requests = scan.requests.filter(
    (r) => r.harness === "claude" && r.profiles.includes(profile.id),
  );
  const relevant = requests.filter((r) =>
    r.parts.some((p) => bucket(r, p) === selectedBucket),
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
  const comparisonCost = {
    "5m": replayCost({
      requests,
      scan,
      catalog,
      selectedBucket,
      ttl: "5m",
      since,
    }),
    "1h": replayCost({
      requests,
      scan,
      catalog,
      selectedBucket,
      ttl: "1h",
      since,
    }),
  };
  const fixedCost = addCosts([
    ...relevant.filter((r) => r.time >= since).map(serverCost),
    summaryCost["5m"],
  ]);
  const cost = {
    "5m": addCosts([comparisonCost["5m"], fixedCost]),
    "1h": addCosts([comparisonCost["1h"], fixedCost]),
  };
  const coldComparison = replayCost({
    requests,
    scan,
    catalog,
    selectedBucket,
    ttl: "1h",
    since,
    coldPrefixes: true,
  });
  const cold = addCosts([coldComparison, fixedCost]);
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
  uncertainty.push(
    ...relevant.filter((r) => r.time >= since).flatMap((r) => r.issues),
    ...cost["5m"].unknown,
    ...cost["1h"].unknown,
  );
  return {
    check: "claude-cache-ttl",
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
    },
    assumptions: [
      current.reason,
      "latest recorded client version is assumed to remain applicable to current profile controls",
      "1h writes use documented 2x input; first visible request keeps observed reads from prefixes warmed before the window",
      "reuse conversions are limited to the previously cached prefix; new input stays a write",
      "server fees and unmatched summary proxies are held identical in both TTL scenarios and cancel in comparisonCost, even if fixedCost is unknown",
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
  const compactions = compactionCharges(scan, catalog);
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
        since: options.since,
        now,
      });
      if (result) checks.push(result);
    }
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
