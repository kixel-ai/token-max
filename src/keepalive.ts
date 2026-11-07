import type {
  BillingPart,
  CompactionCharge,
  CompactionCheckResult,
  Cost,
  KeepaliveCheckResult,
  KeepaliveScenario,
  Profile,
  Request,
  Scan,
  StructuralEvent,
  Ttl,
  TtlCheckResult,
  UsageEstimate,
} from "./model.js";
import { TTL_MILLISECONDS, CODEX_CACHE_MILLISECONDS } from "./cache.js";
import { idleChecks } from "./idle.js";
import { compactionSamples, replayWindows } from "./compaction.js";
import {
  addCosts,
  priceRequest,
  unknownCost,
  zeroCost,
  writeBilling,
  PRICE_SOURCES,
  type Catalog,
} from "./prices.js";

const MINUTE = 60_000;
const SOURCES = [
  "https://code.claude.com/docs/en/scheduled-tasks (2026-11-07)",
  "https://code.claude.com/docs/en/hooks#asyncRewake (2026-11-07)",
  "https://platform.claude.com/docs/en/build-with-claude/prompt-caching (2026-11-07)",
  ...PRICE_SOURCES,
];
function minus(a: Cost, b: Cost): Cost {
  return {
    knownUsd: a.knownUsd - b.knownUsd,
    totalUsd:
      a.totalUsd === null || b.totalUsd === null
        ? null
        : a.totalUsd - b.totalUsd,
    unknown: [...new Set([...a.unknown, ...b.unknown])],
    assumptions: [...new Set([...a.assumptions, ...b.assumptions])],
  };
}
function median(values: number[]): number | null {
  values.sort((a, b) => a - b);
  const middle = Math.floor(values.length / 2);
  return values.length
    ? values.length % 2
      ? values[middle]!
      : Math.ceil((values[middle - 1]! + values[middle]!) / 2)
    : null;
}
function cohort(r: Request, p: BillingPart): string {
  return JSON.stringify([
    r.session,
    r.provider,
    p.model,
    p.tier,
    p.speed,
    p.geo,
  ]);
}
function cachePrefix(p: BillingPart): number | null {
  return p.usage.read === null || p.usage.write === null
    ? null
    : p.usage.read + p.usage.write;
}

function policy(cap: number | null, idle: boolean, interval?: number): string {
  return cap === null
    ? idle
      ? "idle-compaction"
      : "no-keepalive"
    : `ping:${cap}ms${idle ? "+idle-compaction" : ""}${interval === undefined ? "" : `@${interval}ms`}`;
}
function ttl(
  profile: Profile,
  r: Request,
  checks: TtlCheckResult[],
): Ttl | null {
  const check = checks.find(
    (c) => c.profile === profile.id && c.bucket === "main",
  );
  const project = r.cwd
    ? profile.idleDirectories?.[r.cwd]?.ttl?.main
    : undefined;
  if (project && project.owner !== profile.ttl.main.owner) return project.value;
  return check?.action === "change"
    ? check.recommended
    : ((r.cwd
        ? profile.idleDirectories?.[r.cwd]?.ttl?.main.value
        : undefined) ?? profile.ttl.main.value);
}
function lifetime(
  profile: Profile,
  r: Request,
  p: BillingPart,
  checks: TtlCheckResult[],
): number | null {
  if (r.harness === "claude") {
    const value = ttl(profile, r, checks);
    return value ? TTL_MILLISECONDS[value] : null;
  }
  // A read of a long-lived entry only refreshes the default 30-minute clock.
  return r.provider === "openai" && p.model && writeBilling(p.model) === true
    ? CODEX_CACHE_MILLISECONDS
    : null;
}
function cadence(ms: number): number {
  // The five-minute loop has a documented 15-second lead. The other cadences
  // are explicit modeling choices from the maintainer's question, not cron guarantees.
  return ms === 5 * MINUTE
    ? ms - 15_000
    : ms === 60 * MINUTE
      ? 55 * MINUTE
      : ms - 30_000;
}
interface Period {
  previous: Request;
  next: Request | undefined;
  lifetime: number | null;
  interval: number | null;
  gap: number;
  live: number;
  basis: string;
}
interface Replay {
  cost: Cost;
  pingCost: Cost;
  pings: number;
  finalPings: number;
  idleAvailable: boolean;
  finalCharges: { elapsed: number; cost: Cost }[];
}

/** Compare data-derived capped pings and native idle policies using shared billing and idle replay. */
export function keepaliveChecks(options: {
  scan: Scan;
  catalog: Catalog;
  summaries: CompactionCharge[];
  estimates: Map<string, UsageEstimate>;
  ttls: TtlCheckResult[];
  windows: CompactionCheckResult[];
  since: number | undefined;
  now: number;
}): KeepaliveCheckResult[] {
  const result: KeepaliveCheckResult[] = [];
  for (const profile of options.scan.profiles) {
    const selected = options.scan.requests.filter((r) =>
      r.profiles.includes(profile.id),
    );
    const recorded = selected.filter((r) => r.keepalive?.attributed === true);
    let all = [...options.scan.cacheContext.requests, ...selected]
      .filter((r) => r.profiles.includes(profile.id))
      .sort((a, b) => a.time - b.time || a.id.localeCompare(b.id));
    const reconstructionErrors: string[] = [];
    let events = [
      ...options.scan.cacheContext.boundaries,
      ...options.scan.events,
    ].filter((e) => e.profiles.includes(profile.id));
    if (profile.harness === "claude") {
      const captured = new Map<string, Request>();
      const reconstructed = idleChecks({
        ...options,
        scan: { ...options.scan, profiles: [profile] },
        ttls: [],
        windows: [],
        captureOff: captured,
      });
      reconstructionErrors.push(
        ...reconstructed.flatMap(
          (c) => c.afterRecommendations.adjustment.off.unknown,
        ),
      );
      const removed = new Set(
        events
          .filter(
            (e) => e.kind === "compaction" && e.idleAttribution === "notice",
          )
          .map((e) => e.id),
      );
      const summaries = new Set(
        options.summaries
          .filter((s) => removed.has(s.event))
          .flatMap((s) => (s.request ? [s.request] : [])),
      );
      all = all
        .map((r) => captured.get(r.id) ?? r)
        .map((r) => ({
          ...r,
          parts: r.parts.filter(
            (p) => p.kind !== "compaction" || !summaries.has(r.id),
          ),
        }))
        .filter((r) => r.parts.length > 0);
      events = events.filter((e) => !removed.has(e.id));
    }
    const real = all.filter(
      (r) =>
        !r.keepalive?.attributed &&
        r.agent === "main" &&
        r.parts.some((p) => p.kind === "message" || p.kind === "unknown"),
    );
    if (!real.some((r) => selected.some((s) => s.id === r.id))) continue;
    const since =
      options.since ??
      options.scan.since ??
      Math.min(...selected.map((r) => r.time));
    const periods: Period[] = [];
    const sessions = new Map<string, Request[]>();
    for (const r of real) {
      const group = sessions.get(r.session) ?? [];
      group.push(r);
      sessions.set(r.session, group);
    }
    for (const group of sessions.values())
      for (const [index, previous] of group.entries()) {
        const next = group[index + 1];
        const end = next?.time ?? options.now;
        if (end <= since || previous.time > options.now) continue;
        const p = previous.parts.find(
          (p) => p.kind === "message" || p.kind === "unknown",
        )!;
        const ms = lifetime(profile, previous, p, options.ttls);
        const stopped = events
          .filter(
            (e) =>
              e.session === previous.session &&
              e.time > previous.time &&
              e.time < end &&
              (e.liveness === "closed" || e.liveness === "asleep"),
          )
          .sort((a, b) => a.time - b.time)[0];
        periods.push({
          previous,
          next,
          lifetime: ms,
          interval: ms === null ? null : cadence(ms),
          gap: Math.max(0, end - previous.time),
          live: Math.max(0, (stopped?.time ?? end) - previous.time),
          basis: stopped
            ? "recorded shutdown/sleep; no catch-up until a real return"
            : "fallback: client left open and awake until return/cap; final periods incur the full cap",
        });
      }
    const current = profile.keepalive;
    const caps = new Set<number>();
    for (const p of periods) {
      if (!p.next || !p.interval || !p.lifetime || p.gap < p.lifetime) continue;
      const last = Math.max(
        p.interval,
        Math.ceil((p.gap - p.lifetime + 1) / p.interval) * p.interval,
      );
      for (let cap = p.interval; cap <= last; cap += p.interval) caps.add(cap);
    }
    if (current?.capMs && current.capMs > 0) caps.add(current.capMs);
    // No fixed maximum: beyond the last observed return another ping can only
    // add cost under fixed behavior. Current longer caps remain priced above.
    const unknown: string[] = [...reconstructionErrors];
    if (
      options.scan.coverage.issues.some(
        (i) =>
          i ===
          "scheduled wake has no verified parent-linked billed request; keepalive attribution is incomplete",
      )
    )
      unknown.push(
        "unlinked scheduled wake leaves ping cost and warmth attribution unresolved",
      );
    if (selected.some((r) => r.keepalive && !r.keepalive.attributed))
      unknown.push(
        "scheduled/task provenance is recorded but cache-only purpose is not established; affected no-ping reconstruction is conditional",
      );
    if (periods.some((p) => p.lifetime === null))
      unknown.push(
        "model cache refresh policy or effective Claude TTL is unresolved",
      );
    const originalPings = new Set(
      all.filter((r) => r.keepalive?.attributed).map((r) => r.id),
    );
    const byId = new Map(all.map((r) => [r.id, r]));
    const runs = new Map<string, Replay>();
    const run = (
      cap: number | null,
      idle: boolean,
      mode: "point" | "closed" | "open",
      intervalOverride?: number,
    ): Replay => {
      const key = JSON.stringify([cap, idle, mode, intervalOverride]);
      const prior = runs.get(key);
      if (prior) return prior;
      const rs: Request[] = [];
      const cache = new Map<
        string,
        {
          time: number;
          prefix: number;
          longUntil: number;
          longPrefix: number;
          hypothetical: boolean;
        }
      >();
      let pingCost = zeroCost(),
        pings = 0,
        finalPings = 0;
      const injected = new Map<
        string,
        {
          ordinary: number | null;
          output: number | null;
          elapsed: number;
          final: boolean;
        }
      >();
      const lastReplay = new Map<string, Request>();
      const finalCharges: Replay["finalCharges"] = [];
      const changedEvents: StructuralEvent[] = events.map((e) => {
        const settings = { ...e.settings };
        // Changed ping timing cannot inherit the recorded idle outcome of a different gap.
        delete settings.idleCompaction;
        return { ...e, settings };
      });
      const growth = new Map<string, number>();
      const lastOriginal = new Map<string, Request>();
      const additions: Request[] = [];
      for (const period of periods) {
        const interval = intervalOverride ?? period.interval;
        if (cap === null || !interval || period.lifetime === null) continue;
        const r = period.previous,
          p = r.parts.find((p) => p.kind === "message") ?? r.parts[0]!;
        const examples = recorded.filter(
          (sample) =>
            sample.provider === r.provider &&
            sample.parts.length === 1 &&
            sample.parts[0]!.model === p.model &&
            sample.parts[0]!.tier === p.tier &&
            sample.parts[0]!.speed === p.speed &&
            sample.parts[0]!.geo === p.geo,
        );
        const proxies = examples.length
          ? examples
          : real.filter(
              (sample) =>
                sample.provider === r.provider &&
                sample.parts.length === 1 &&
                sample.parts[0]!.model === p.model &&
                sample.parts[0]!.tier === p.tier &&
                sample.parts[0]!.speed === p.speed &&
                sample.parts[0]!.geo === p.geo,
            );
        const ordinary = median(
          proxies.flatMap((sample) => {
            const u = sample.parts[0]!.usage;
            return u.input === null || u.read === null || u.write === null
              ? []
              : [u.input - u.read - u.write];
          }),
        );
        const output = median(
          proxies.flatMap((sample) =>
            sample.parts[0]!.usage.output === null
              ? []
              : [sample.parts[0]!.usage.output!],
          ),
        );
        const limit =
          mode === "closed" && period.basis.startsWith("fallback")
            ? 0
            : Math.min(
                cap,
                period.basis.startsWith("fallback") ? Infinity : period.live,
              );
        // A final period has no inferred exit at the observation boundary.
        const end = period.next ? Math.min(period.gap, limit) : limit;
        for (
          let elapsed = interval;
          elapsed <= end && (!period.next || elapsed < period.gap);
          elapsed += interval
        ) {
          const fire = Math.max(r.time + elapsed, r.endTime);
          if (
            fire > r.time + limit ||
            (period.next && fire >= period.next.time)
          )
            continue;
          if (fire < since) continue;
          elapsed = fire - r.time;
          const ping: Request = {
            ...r,
            id: `keepalive:${r.id}:${elapsed}`,
            time: fire,
            endTime: fire,
            evidence: "estimated",
            trigger: null,
            reference: null,
            serverTools: {},
            parts: [{ ...p, usage: { ...p.usage } }],
            issues: [],
            attempt: null,
          };
          delete ping.keepalive;
          additions.push(ping);
          injected.set(ping.id, {
            ordinary,
            output,
            elapsed,
            final: !period.next,
          });
          pings++;
          if (!period.next) finalPings++;
        }
      }
      const timeline = [...all, ...additions].sort(
        (a, b) => a.time - b.time || a.id.localeCompare(b.id),
      );
      for (const r of timeline) {
        if (originalPings.has(r.id)) {
          const part = r.parts.find((p) => p.kind === "message");
          const previous = lastOriginal
            .get(r.session)
            ?.parts.find((p) => p.kind === "message");
          if (
            part?.usage.input === null ||
            part?.usage.output === null ||
            !part ||
            !previous ||
            previous.usage.input === null ||
            previous.usage.output === null
          ) {
            unknown.push(
              "attributed ping context growth cannot be reconstructed from a preceding measured turn",
            );
          } else
            growth.set(
              r.session,
              (growth.get(r.session) ?? 0) -
                Math.max(
                  0,
                  part.usage.input -
                    previous.usage.input -
                    previous.usage.output,
                ) -
                part.usage.output,
            );
          lastOriginal.set(r.session, r);
          continue;
        }
        const estimate = options.estimates.get(r.id);
        const parts = estimate?.part ? [estimate.part] : r.parts;
        const clone = {
          ...r,
          parts: parts.map((part) => ({ ...part, usage: { ...part.usage } })),
        };
        const pingMetadata = injected.get(r.id);
        if (pingMetadata) {
          const prior = lastReplay
            .get(r.session)
            ?.parts.find((p) => p.kind === "message");
          const u = clone.parts[0]!.usage;
          const size = prior ? cachePrefix(prior) : null;
          const input = prior?.usage.input ?? null,
            output = prior?.usage.output ?? null;
          u.input =
            input === null || output === null || pingMetadata.ordinary === null
              ? null
              : input + output + pingMetadata.ordinary;
          u.read = size;
          u.write =
            input === null || output === null || size === null
              ? null
              : input + output - size;
          u.output = pingMetadata.output;
          u.reasoning = null;
          u.write5m = 0;
          u.write1h = 0;
          u.write30m = 0;
          u.write12h = 0;
        }
        for (const p of clone.parts) {
          if (p.kind === "message" && !injected.has(r.id)) {
            const previous = lastOriginal.get(r.session);
            if (
              events.some(
                (e) =>
                  e.kind === "compaction" &&
                  e.origins.some((o) => o.session === r.session) &&
                  e.time > (previous?.time ?? -Infinity) &&
                  e.time <= r.time,
              )
            )
              growth.delete(r.session);
            const delta = growth.get(r.session) ?? 0;
            const u = p.usage;
            if (u.input !== null && u.read !== null && u.write !== null) {
              if (delta >= 0) {
                u.input += delta;
                u.write += delta;
              } else {
                let remove = -delta;
                if (remove > u.input) {
                  u.input = null;
                  u.read = null;
                  u.write = null;
                } else {
                  u.input -= remove;
                  const read = Math.min(u.read, remove);
                  u.read -= read;
                  remove -= read;
                  const write = Math.min(u.write, remove);
                  u.write -= write;
                }
              }
            }
          }
          if (p.kind !== "message") continue;
          const ms = lifetime(profile, r, p, options.ttls),
            u = p.usage;
          if (
            ms === null ||
            u.read === null ||
            u.write === null ||
            u.input === null
          )
            continue;
          const key = cohort(r, p);
          let previous = cache.get(key);
          if (
            events.some(
              (e) =>
                e.kind === "compaction" &&
                e.origins.some((o) => o.session === r.session) &&
                e.time > (previous?.time ?? -Infinity) &&
                e.time <= r.time,
            )
          )
            previous = undefined;
          const warmPrefix = previous
            ? r.time - previous.time < ms
              ? previous.prefix
              : r.time < previous.longUntil
                ? previous.longPrefix
                : 0
            : 0;
          const warm = warmPrefix > 0;
          const removed = all.some(
            (other) =>
              originalPings.has(other.id) &&
              other.session === r.session &&
              other.time > (previous?.time ?? -Infinity) &&
              other.time < r.time,
          );
          if (injected.has(r.id)) {
            const cold = Math.max(0, u.read - warmPrefix);
            u.write += cold;
            u.read -= cold;
          }
          if (!injected.has(r.id) && previous) {
            if (warm && previous.hypothetical) {
              const long = r.harness === "codex" ? (u.write12h ?? 0) : 0;
              const shortReused = Math.min(
                u.write - long,
                Math.max(0, warmPrefix - u.read),
              );
              const longReused = Math.min(
                long,
                Math.max(
                  0,
                  (r.time < previous.longUntil ? previous.longPrefix : 0) -
                    u.read -
                    shortReused,
                ),
              );
              u.write -= shortReused + longReused;
              u.read += shortReused + longReused;
              if (r.harness === "codex" && u.write12h !== null)
                u.write12h -= longReused;
            } else if (removed) {
              const cold = Math.min(
                Math.max(0, u.read - warmPrefix),
                previous.prefix,
              );
              u.read -= cold;
              u.write += cold;
            }
          }
          if (r.harness === "claude") {
            const chosen = ttl(profile, r, options.ttls);
            u.write5m = chosen === "5m" ? u.write : 0;
            u.write1h = chosen === "1h" ? u.write : 0;
          } else if (writeBilling(p.model ?? "") === true) {
            const long = u.write12h ?? 0;
            u.write30m = Math.max(0, u.write - long);
          }
          const size = cachePrefix(p);
          if (size !== null)
            cache.set(key, {
              time: r.time,
              prefix: size,
              hypothetical: injected.has(r.id),
              longPrefix:
                (u.write12h ?? 0) > 0
                  ? (r.time < (previous?.longUntil ?? 0)
                      ? previous!.longPrefix
                      : 0) + (u.write12h ?? 0)
                  : (previous?.longPrefix ?? 0),
              longUntil:
                (u.write12h ?? 0) > 0
                  ? r.time + 12 * 60 * MINUTE
                  : (previous?.longUntil ?? 0),
            });
        }
        if (injected.has(r.id)) {
          const charge = priceRequest(options.catalog, clone);
          pingCost = addCosts([pingCost, charge]);
          if (pingMetadata?.final)
            finalCharges.push({ elapsed: pingMetadata.elapsed, cost: charge });
          const p = clone.parts[0]!,
            u = p.usage;
          const ordinary =
            u.input === null || u.read === null || u.write === null
              ? null
              : u.input - u.read - u.write;
          if (ordinary !== null && u.output !== null)
            growth.set(
              r.session,
              (growth.get(r.session) ?? 0) + ordinary + u.output,
            );
        } else lastOriginal.set(r.session, r);
        lastReplay.set(r.session, clone);
        rs.push(clone);
      }
      const synthetic: Scan = {
        ...options.scan,
        profiles: [profile],
        requests: rs.filter((r) => r.time >= since),
        events: changedEvents.filter((e) => e.time >= since),
        cacheContext: {
          requests: rs.filter((r) => r.time < since),
          boundaries: changedEvents.filter(
            (e) => e.time < since && e.kind === "compaction",
          ),
        },
      };
      let cost: Cost,
        idleAvailable = false;
      if (profile.harness === "claude") {
        const captured = new Map<string, Request>();
        const checks = idleChecks({
          ...options,
          scan: synthetic,
          calibrationScan: options.scan,
          estimates: new Map(),
          since,
          lifetime: mode === "closed" ? "closed" : "open",
          ...(idle ? { captureOn: captured } : { captureOff: captured }),
        });
        if (!(idle && mode === "closed")) {
          pingCost = addCosts(
            [...injected.keys()].map((id) => {
              const r = captured.get(id);
              return r
                ? priceRequest(options.catalog, r)
                : unknownCost(
                    "joint replay did not establish this ping's context",
                  );
            }),
          );
          finalCharges.length = 0;
          for (const [id, p] of injected)
            if (p.final)
              finalCharges.push({
                elapsed: p.elapsed,
                cost: captured.has(id)
                  ? priceRequest(options.catalog, captured.get(id)!)
                  : unknownCost("joint final ping geometry is unresolved"),
              });
        }
        const scenarios = checks.map((c) => c.afterRecommendations);
        idleAvailable = scenarios.some((s) =>
          s.periods.some(
            (p) => p.status === "eligible" || p.status === "unknown",
          ),
        );
        if (scenarios.length) {
          const baseline = scenarios[0]!.recordedBaselineCost;
          cost = addCosts([
            baseline,
            ...scenarios.map((s) =>
              minus(s.cost[idle ? "on" : "off"], baseline),
            ),
          ]);
        } else
          cost = unknownCost(
            "native idle replay has no comparable main request",
          );
      } else {
        const windows = options.windows.filter((c) => c.profile === profile.id);
        const windowed = replayWindows({
          requests: rs,
          events: changedEvents,
          estimates: new Map(),
          windows: windows.map((c) => ({
            value: c.action === "change" ? c.recommended : c.current,
            models: c.models,
          })),
          catalog: options.catalog,
          samples: compactionSamples({
            requests: all,
            events,
            summaries: options.summaries,
          }),
          since,
          ttls: { main: null, subagent: null },
        });
        cost = addCosts(
          windowed.requests
            .filter((r) => r.time >= since)
            .map((r) => priceRequest(options.catalog, r)),
        );
        cost = addCosts([
          cost,
          ...windowed.added.map((a) => a.summaryCost),
          ...windowed.errors.map(unknownCost),
          ...options.summaries
            .filter(
              (s) =>
                !s.request &&
                events.some((e) => e.id === s.event && e.time >= since),
            )
            .map((s) => s.cost),
        ]);
        const byId = new Map(windowed.requests.map((r) => [r.id, r]));
        pingCost = addCosts(
          [...injected.keys()].map((id) =>
            byId.has(id)
              ? priceRequest(options.catalog, byId.get(id)!)
              : unknownCost(
                  "window replay did not establish this ping's context",
                ),
          ),
        );
        finalCharges.length = 0;
        for (const [id, p] of injected)
          if (p.final)
            finalCharges.push({
              elapsed: p.elapsed,
              cost: byId.has(id)
                ? priceRequest(options.catalog, byId.get(id)!)
                : unknownCost("final ping context is unresolved"),
            });
      }
      if (unknown.length) cost = addCosts([cost, ...unknown.map(unknownCost)]);
      const value = {
        cost,
        pingCost,
        pings,
        finalPings,
        idleAvailable,
        finalCharges,
      };
      runs.set(key, value);
      return value;
    };
    const baseline = run(null, false, "point");
    const alternatives: {
      cap: number | null;
      idle: boolean;
      interval?: number;
    }[] = [{ cap: null, idle: false }];
    if (baseline.idleAvailable) alternatives.push({ cap: null, idle: true });
    for (const cap of [...caps].sort((a, b) => a - b)) {
      alternatives.push({ cap, idle: false });
      if (baseline.idleAvailable) alternatives.push({ cap, idle: true });
    }
    const currentCap = current?.capMs ?? null;
    const currentInterval = current?.intervalMs;
    const currentIdle = current?.policy?.includes("idle-compaction") === true;
    const currentValid =
      current?.resolved === true &&
      current.policy !== null &&
      (currentCap === null
        ? ["no-keepalive", "idle-compaction"].includes(current.policy)
        : Number.isSafeInteger(currentCap) &&
          (current.policy === policy(currentCap, currentIdle) ||
            current.policy ===
              policy(currentCap, currentIdle, currentInterval ?? undefined)) &&
          currentCap > 0 &&
          currentInterval !== null &&
          currentInterval !== undefined &&
          Number.isSafeInteger(currentInterval) &&
          currentInterval > 0);
    const alternateInterval =
      currentCap !== null &&
      currentInterval &&
      periods.some((p) => p.interval !== currentInterval)
        ? currentInterval
        : undefined;
    const currentPolicy = currentValid
      ? policy(currentCap, currentIdle, alternateInterval)
      : null;
    if (
      currentValid &&
      currentCap !== null &&
      !alternatives.some(
        (p) =>
          p.cap === currentCap &&
          p.idle === currentIdle &&
          p.interval === alternateInterval,
      )
    )
      alternatives.push({
        cap: currentCap,
        idle: currentIdle,
        ...(alternateInterval ? { interval: alternateInterval } : {}),
      });
    const scenarios: KeepaliveScenario[] = alternatives.map(
      ({ cap, idle, interval }) => {
        const point = run(cap, idle, "point", interval),
          closed = run(cap, idle, "closed", interval),
          open = run(cap, idle, "open", interval);
        const saving = minus(baseline.cost, point.cost).totalUsd;
        const closedSaving = minus(
          run(null, false, "closed").cost,
          closed.cost,
        ).totalUsd;
        let breakEvenOpenMs: number | null = null;
        // Holding returned periods fixed, final ping charges are monotone in
        // the unreturned open duration. Joint idle crossings remain unresolved.
        if (
          cap !== null &&
          !idle &&
          !options.windows.some(
            (w) => w.profile === profile.id && w.action === "change",
          ) &&
          saving !== null &&
          point.finalCharges.every((p) => p.cost.totalUsd !== null)
        ) {
          const budget =
            saving +
            point.finalCharges.reduce((n, p) => n + p.cost.totalUsd!, 0);
          let spent = 0;
          for (const charge of [...point.finalCharges].sort(
            (a, b) => a.elapsed - b.elapsed,
          )) {
            spent += charge.cost.totalUsd!;
            if (spent > budget && budget >= 0) {
              breakEvenOpenMs = charge.elapsed;
              break;
            }
          }
        }
        return {
          policy: policy(cap, idle, interval),
          intervalMs: interval ?? null,
          estimatedOpenMs:
            cap === null
              ? null
              : periods.reduce(
                  (n, p) =>
                    n +
                    Math.min(
                      cap,
                      p.next ? p.gap : Infinity,
                      p.basis.startsWith("fallback") ? Infinity : p.live,
                    ),
                  0,
                ),
          capMs: cap,
          idle,
          comparisonCost: point.cost,
          pingCost: point.pingCost,
          pings: point.pings,
          finalPings: point.finalPings,
          savingsUsd: saving,
          monthlySavingsUsd:
            saving === null || options.now <= since
              ? null
              : (saving * 30 * 86400000) / (options.now - since),
          sensitivity: {
            closedSavingsUsd: closedSaving,
            openSavingsUsd: minus(run(null, false, "open").cost, open.cost)
              .totalUsd,
            breakEvenOpenMs,
            extraWorkBreakEvenUsdPerPing:
              saving !== null && saving > 0 && point.pings > 0
                ? saving / point.pings
                : null,
          },
        };
      },
    );
    const priced = scenarios.filter((s) => s.comparisonCost.totalUsd !== null);
    const winner =
      priced.length === scenarios.length
        ? priced.reduce((best, s) =>
            s.comparisonCost.totalUsd! < best.comparisonCost.totalUsd! ||
            (s.comparisonCost.totalUsd === best.comparisonCost.totalUsd &&
              s.policy === currentPolicy)
              ? s
              : best,
          )
        : null;
    const savingsCurrentKnown = () =>
      scenarios.some(
        (s) => s.policy === currentPolicy && s.comparisonCost.totalUsd !== null,
      );
    const supported =
      profile.harness === "claude" &&
      current?.resolved === true &&
      current.supported === true &&
      currentPolicy !== null &&
      savingsCurrentKnown() &&
      current.mechanism !== "unknown";
    const savings = currentPolicy
      ? (() => {
          const prior = scenarios.find((s) => s.policy === currentPolicy);
          return prior && winner
            ? minus(prior.comparisonCost, winner.comparisonCost).totalUsd
            : null;
        })()
      : (winner?.savingsUsd ?? null);
    result.push({
      check: "keepalive",
      bucket: "main",
      profile: profile.id,
      historyProfiles: [...new Set(selected.flatMap((r) => r.profiles))],
      evidence: winner ? "estimated" : "unknown",
      action:
        !winner || !supported || currentPolicy === null
          ? "advisory"
          : winner.policy === currentPolicy
            ? "keep"
            : "change",
      current: currentPolicy,
      recommended: winner?.policy ?? null,
      owner:
        (current
          ? [
              current.owner,
              ...(winner?.idle && !currentIdle
                ? [
                    profile.idle?.owner,
                    ...Object.values(profile.idleDirectories ?? {}).map(
                      (c) => c.owner,
                    ),
                  ].filter((o): o is string => o !== undefined)
                : []),
            ]
              .filter((o, i, a) => a.indexOf(o) === i)
              .join("; ")
          : undefined) ??
        (profile.harness === "claude"
          ? "Claude /loop or asyncRewake owner unresolved"
          : "Codex: no verified native keepalive mechanism"),
      mechanism: current?.mechanism ?? "unknown",
      since,
      until: options.now,
      windowBasis:
        options.since !== undefined || options.scan.since !== null
          ? "explicit selected window"
          : "current settings applied to available history",
      baseline:
        "reconstructed no keepalive, idle compaction off, after applicable TTL/window changes; current-policy savings when known; never add idle/keepalive savings",
      requests: selected.length,
      savingsUsd: savings,
      monthlySavingsUsd:
        savings === null || options.now <= since
          ? null
          : (savings * 30 * 86400000) / (options.now - since),
      breakEven:
        "Per-cap break-even is the first unreturned ping that exhausts the returned-period saving, holding returned periods fixed; joint-idle crossings stay unknown. Extra-work dollars per ping are a budget, not a guaranteed token allowance.",
      recordedPings: recorded.length,
      recordedPingCost: addCosts(
        recorded.map((r) => priceRequest(options.catalog, r)),
      ),
      scenarios,
      periods: periods.map((p) => ({
        request: p.previous.id,
        resume: p.next?.id ?? null,
        gapMs: p.gap,
        lifetimeMs: p.basis.startsWith("fallback") ? null : p.live,
        lifetimeBasis: p.basis,
        intervalMs: p.interval,
      })),
      usageEstimates: [...options.estimates.values()].filter((e) =>
        byId.has(e.request),
      ),
      assumptions: [
        "fixed behavior and matching cache prefixes; keepalive is a full billed turn, not a free timer",
        "caps cover every cadence multiple through the last ping needed to bridge an observed return; known current caps are also included; intermediate caps can change native idle timing",
        "ping overhead uses median explicitly attributed same-model/tier ping input/output, otherwise median comparable real-turn input/output as an estimated full-turn proxy; reasoning remains in output",
        "fallback lifetime is open and awake to return/cap, including full final caps beyond the snapshot boundary; shutdown/sleep stops pings until a real return; no one-hour activation fraction is extrapolated",
        "remove explicitly attributed pings and reconstruct their cache warmth; ambiguous task purpose prevents a priced winner",
        "native idle replay receives synthetic ping requests, so they reset its last-request clock; compaction and keepalive costs are compared jointly, not added",
        "Codex 30-minute reads preserve valid longer coverage without restarting its 12-hour clock; longer observed hits remain measured evidence, not a guarantee",
        "55-minute Claude and 29.5-minute Codex cadences are modeled ideals; /loop rounding, jitter, active turns, request duration and sleep may miss expiry; native /loop 5m runs every 4m45s",
      ],
      uncertainty: [
        ...unknown,
        current?.reason ??
          "native transcripts do not establish current ping purpose, cap, task existence or hook state; no records does not mean off",
        ...(!supported
          ? [
              "priced winner is advisory until a verified mechanism and current effective control metadata establish ownership and support",
            ]
          : []),
        "retained ping prompt/reply growth, model thinking, cache routing, external prefix warming and useful scheduled work can alter the fixed-behavior result",
      ],
      sources: SOURCES,
    });
  }
  return result;
}
