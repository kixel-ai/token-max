import type {
  BillingPart,
  CompactionCharge,
  CompactionCheckResult,
  Cost,
  IdleCheckResult,
  IdleControl,
  IdlePeriod,
  IdleScenario,
  Profile,
  Request,
  Scan,
  StructuralEvent,
  Ttl,
  TtlCheckResult,
  UsageEstimate,
} from "./model.js";
import { idleControls, windowControl } from "./controls.js";
import {
  compactionSamples,
  replayWindows,
  type CompactionSample,
} from "./compaction.js";
import { replayParts } from "./scenarios.js";
import { TTL_MILLISECONDS } from "./cache.js";
import {
  addCosts,
  priceComponents,
  pricePart,
  serverCost,
  unknownCost,
  zeroCost,
  PRICE_SOURCES,
  type Catalog,
} from "./prices.js";

const HOUR = TTL_MILLISECONDS["1h"];
// The inspected native 2.1.316 implementation arms at 90% of the lifetime.
// This is a timing estimate, not a documented timer setting or version guarantee.
const EARLY = HOUR * 0.9;
const SOURCES = [
  "https://code.claude.com/docs/en/settings-reference#idlecompaction (2026-11-07)",
  "https://code.claude.com/docs/en/prompt-caching#compacting-the-conversation (2026-11-07)",
];

function supported(version: string | null, patch: number): boolean {
  const v = /^(\d+)\.(\d+)\.(\d+)$/
    .exec(version ?? "")
    ?.slice(1)
    .map(Number);
  return (
    !!v &&
    (v[0]! > 2 || (v[0] === 2 && (v[1]! > 1 || (v[1] === 1 && v[2]! >= patch))))
  );
}
function median(values: number[]): number {
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2
    ? sorted[mid]!
    : Math.ceil((sorted[mid - 1]! + sorted[mid]!) / 2);
}
function scale(cost: Cost, fraction: number): Cost {
  if (fraction === 0) return zeroCost();
  return {
    ...cost,
    knownUsd: cost.knownUsd * fraction,
    totalUsd: cost.totalUsd === null ? null : cost.totalUsd * fraction,
  };
}
function difference(after: Cost, before: Cost): Cost {
  if (JSON.stringify(after) === JSON.stringify(before)) return zeroCost();
  const knownUsd = after.knownUsd - before.knownUsd;
  return {
    knownUsd,
    totalUsd:
      after.totalUsd === null || before.totalUsd === null
        ? null
        : after.totalUsd - before.totalUsd,
    unknown: [...after.unknown, ...before.unknown],
    assumptions: [...new Set([...after.assumptions, ...before.assumptions])],
  };
}
function prefix(p: BillingPart): number | null {
  return p.usage.read === null || p.usage.write === null
    ? null
    : p.usage.read + p.usage.write;
}
function control(profile: Profile, r?: Request): IdleControl {
  return (
    (r?.cwd ? profile.idleDirectories?.[r.cwd] : undefined) ??
    profile.idle ??
    idleControls({
      scopes: [{ config: profile.settings, file: profile.settingsFile }],
      env: {},
    })
  );
}
function scopedWindow(profile: Profile, r: Request, model: string | null) {
  const c = control(profile, r);
  return windowControl(
    c.compaction ? { ...profile, compaction: c.compaction } : profile,
    model,
  );
}
function scopedTtls(
  profile: Profile,
  r: Request,
  ttls: IdleScenario["ttls"],
): IdleScenario["ttls"] {
  const current = control(profile, r).ttl ?? profile.ttl;
  return {
    main:
      current.main.owner === profile.ttl.main.owner
        ? ttls.main
        : current.main.value,
    subagent:
      current.subagent.owner === profile.ttl.subagent.owner
        ? ttls.subagent
        : current.subagent.value,
  };
}
function cohort(r: Request, p: BillingPart, s: CompactionSample): boolean {
  return (
    r.provider !== null &&
    p.model !== null &&
    r.provider === s.request.provider &&
    r.version !== null &&
    r.version === s.request.version &&
    r.client !== null &&
    r.client === s.request.client &&
    p.model === s.p.model &&
    r.agent === s.request.agent &&
    p.tier === s.p.tier &&
    p.speed === s.p.speed &&
    p.geo === s.p.geo
  );
}
function historical(
  events: StructuralEvent[],
  r: Request,
): Record<string, string | number | boolean> {
  const result: Record<string, string | number | boolean> = {};
  for (const e of events) {
    if (e.time > r.time) break;
    if (e.kind === "settings" && e.origins.some((o) => o.session === r.session))
      Object.assign(result, e.settings);
  }
  return result;
}
function client(r: Request): "supported" | "unsupported" | "unknown" {
  if (
    r.agent === "subagent" ||
    r.source?.startsWith("agent:") ||
    r.client?.startsWith("sdk-") ||
    r.client === "sdk-cli"
  )
    return "unsupported";
  // A CLI label also occurs on headless runs; the REPL source establishes interactivity.
  if (r.client === "cli" && r.source === "repl_main_thread") return "supported";
  if (r.client === "claude-vscode" || r.client === "ssh-remote")
    return "supported";
  if (
    r.client?.startsWith("remote") ||
    r.client === "mcp" ||
    r.client === "claude-code-github-action"
  )
    return "unsupported";
  // Desktop entrypoints alone do not distinguish local/SSH Code from hosted execution.
  return "unknown";
}
interface PreparedPeriod {
  period: IdlePeriod;
  previous: Request;
  next: Request | undefined;
  boundary: StructuralEvent | undefined;
  knownOn: boolean;
  window: number | null;
  threshold: number | null;
  ttls: IdleScenario["ttls"];
}
function bySession<T>(
  values: T[],
  sessions: (value: T) => string[],
): Map<string, T[]> {
  const result = new Map<string, T[]>();
  for (const value of values)
    for (const session of new Set(sessions(value))) {
      const group = result.get(session) ?? [];
      group.push(value);
      result.set(session, group);
    }
  return result;
}
function periods(options: {
  requests: Request[];
  events: StructuralEvent[];
  profile: Profile;
  since: number;
  now: number;
  ttls: IdleScenario["ttls"];
  windows: IdleScenario["windows"];
  owner: string;
}): {
  prepared: PreparedPeriod[];
  activationCohorts: IdleScenario["activationCohorts"];
} {
  const result: PreparedPeriod[] = [];
  const sessions = new Map<string, Request[]>();
  for (const r of options.requests.filter(
    (r) =>
      r.agent !== "subagent" &&
      r.parts.some((p) => p.kind === "message" || p.kind === "unknown"),
  )) {
    const group = sessions.get(r.session) ?? [];
    group.push(r);
    sessions.set(r.session, group);
  }
  const eventGroups = bySession(options.events, (e) =>
    e.origins.map((o) => o.session),
  );
  for (const [session, group] of sessions) {
    const events = eventGroups.get(session) ?? [];
    const boundaries = events.filter(
      (e) => e.kind === "compaction" && e.idleAttribution === "notice",
    );
    let boundaryIndex = 0;
    let settingsIndex = 0;
    let activityIndex = 0;
    let turnStart = -Infinity;
    let turnEnd = -Infinity;
    const pending = new Set<string>();
    const old: Record<string, string | number | boolean> = {};
    for (const [index, r] of group.entries()) {
      const c = control(options.profile, r);
      if (c.owner !== options.owner) continue;
      const next = group[index + 1];
      const end = next?.time ?? options.now;
      if (
        end < options.since ||
        r.time + EARLY < options.since ||
        end <= r.endTime
      )
        continue;
      const gapMs = end - r.time;
      const p = r.parts.find(
        (p) => p.kind === "message" || p.kind === "unknown",
      )!;
      while (
        boundaryIndex < boundaries.length &&
        boundaries[boundaryIndex]!.time <= r.time
      )
        boundaryIndex++;
      const boundary =
        boundaries[boundaryIndex] && boundaries[boundaryIndex]!.time <= end
          ? boundaries[boundaryIndex]
          : undefined;
      while (
        settingsIndex < events.length &&
        events[settingsIndex]!.time <= r.time
      ) {
        const e = events[settingsIndex++]!;
        if (e.kind === "settings") Object.assign(old, e.settings);
      }
      const flagState = supported(r.version, 314)
        ? "not-required"
        : typeof old.idleCompactionAvailable === "boolean"
          ? String(old.idleCompactionAvailable)
          : null;
      const contextTokens =
        p.usage.input === null || p.usage.output === null
          ? null
          : p.usage.input + p.usage.output;
      const nativeWindow = scopedWindow(options.profile, r, p.model);
      const windowOwner = nativeWindow.owner;
      const window = !supported(
        r.version,
        nativeWindow.minimumClaudePatch ?? 288,
      )
        ? null
        : (options.windows.find(
            (w) => w.owner === windowOwner && w.models.includes(p.model),
          )?.value ?? null);
      const ttls = scopedTtls(options.profile, r, options.ttls);
      while (
        activityIndex < events.length &&
        events[activityIndex]!.time <= r.time + EARLY
      ) {
        const e = events[activityIndex++]!;
        if (e.kind === "turn-start" || e.kind === "turn-end") pending.clear();
        if (e.kind === "turn-start") turnStart = e.time;
        if (e.kind === "turn-end") turnEnd = e.time;
        if (e.kind === "tool-start") pending.add(e.toolId ?? e.id);
        if (e.kind === "tool-end" && e.toolId) pending.delete(e.toolId);
      }
      const interrupted =
        turnStart > r.endTime && turnStart < end && turnStart < r.time + EARLY;
      const stopped =
        !boundary &&
        events.some(
          (e) =>
            e.time > r.time &&
            e.time <= r.time + EARLY &&
            e.time < end &&
            (e.liveness === "closed" || e.liveness === "asleep"),
        );
      let status: IdlePeriod["status"] = "eligible";
      let reason =
        "interactive request gap under current native prerequisites; open-and-awake activation is estimated";
      if (gapMs < EARLY) {
        status = "censored";
        reason = next
          ? "return precedes the estimated near-expiry trigger"
          : "final observation has not reached the estimated idle trigger; no session end inferred";
      } else if (
        ttls.main === "5m" ||
        c.autoCompact === false ||
        client(r) === "unsupported" ||
        stopped ||
        pending.size > 0 ||
        interrupted ||
        r.endTime >= r.time + EARLY ||
        (contextTokens !== null &&
          c.threshold !== null &&
          contextTokens < c.threshold) ||
        (window !== null && c.threshold !== null && window <= c.threshold)
      ) {
        status = "ineligible";
        reason =
          ttls.main === "5m"
            ? "main TTL is 5m"
            : stopped
              ? "recorded client shutdown/sleep interrupts the open-and-awake interval"
              : c.autoCompact === false
                ? "automatic compaction is disabled"
                : client(r) === "unsupported"
                  ? "SDK/headless/remote/subagent client is outside the confirmed interactive policy"
                  : pending.size > 0 ||
                      interrupted ||
                      r.endTime >= r.time + EARLY
                    ? "tool wait or active turn interrupts the idle interval"
                    : contextTokens !== null &&
                        c.threshold !== null &&
                        contextTokens < c.threshold
                      ? "context is below the effective idle threshold"
                      : "auto-compact window removes the eligible size band";
      } else if (
        !c.resolved ||
        ttls.main === null ||
        contextTokens === null ||
        p.kind === "unknown" ||
        c.threshold === null ||
        window === null ||
        (!boundary &&
          (client(r) !== "supported" ||
            !supported(r.version, 290) ||
            flagState === null ||
            flagState === "false"))
      ) {
        status = "unknown";
        reason =
          "effective threshold/window/TTL, input size, interactive execution, version or pre-2.1.314 flag availability is unresolved";
      }
      const knownOn =
        old.idleCompaction === true &&
        old.promptCacheTtl === "1h" &&
        old.autoCompactEnabled === true &&
        flagState !== null &&
        flagState !== "false";
      result.push({
        previous: r,
        next,
        boundary,
        knownOn,
        window,
        threshold: c.threshold,
        ttls,
        period: {
          request: r.id,
          resume: next?.id ?? null,
          session: r.session,
          contextTokens,
          gapMs,
          status,
          reason,
          confirmed:
            status === "eligible" &&
            turnEnd >= r.endTime &&
            turnEnd < r.time + EARLY,
          returnTiming: !next
            ? "final"
            : gapMs >= EARLY && gapMs <= HOUR
              ? "near-expiry"
              : gapMs < HOUR
                ? "before-expiry"
                : "after-expiry",
          outcome: boundary
            ? "applied"
            : knownOn && next
              ? "not-applied"
              : "unobserved",
          event: boundary?.id ?? null,
          version: r.version,
          client: r.client,
          flagState,
          activation: null,
          activationBasis: "not applicable",
          summaryCost: zeroCost(),
        },
      });
    }
  }
  const cohorts = new Map<string, IdleScenario["activationCohorts"][number]>();
  for (const { period: p, knownOn } of result) {
    if (
      p.status !== "eligible" ||
      !p.resume ||
      !knownOn ||
      !p.version ||
      !p.client ||
      p.flagState === null
    )
      continue;
    const key = JSON.stringify([p.version, p.client, p.flagState]);
    const c = cohorts.get(key) ?? {
      version: p.version,
      client: p.client,
      flagState: p.flagState,
      eligible: 0,
      applied: 0,
      fraction: 0,
    };
    c.eligible++;
    c.applied += p.outcome === "applied" ? 1 : 0;
    c.fraction = c.applied / c.eligible;
    cohorts.set(key, c);
  }
  for (const { period: p } of result) {
    if (p.status !== "eligible") {
      p.activation = p.status === "unknown" ? null : 0;
      continue;
    }
    if (p.outcome !== "unobserved") {
      p.activation = p.outcome === "applied" ? 1 : 0;
      p.activationBasis =
        "recorded applied/not-applied returned outcome; missing discarded generation is separately unobserved";
      continue;
    }
    const c = cohorts.get(JSON.stringify([p.version, p.client, p.flagState]));
    p.activation = c ? c.fraction : 1;
    p.activationBasis = c
      ? `same version/client/flag known-on returned applied fraction ${c.applied}/${c.eligible}; final periods use that same fraction with no second liveness factor`
      : "documented-trigger fallback: left open and awake, one summary; final periods have the same assumed activation and no return benefit";
  }
  return { prepared: result, activationCohorts: [...cohorts.values()] };
}

interface State {
  weight: number;
  offset: number;
  prefix: number | null;
  time: number;
  lifetime: number;
  retained: number;
  active: string | null;
  input: number | null;
  cohort: string | null;
}
interface Run {
  adjustment: Cost;
  byPeriod: Map<string, Cost>;
  resumed: number;
}
function mergeStates(states: State[]): State[] {
  const grouped = new Map<string, State>();
  for (const state of states) {
    if (state.weight === 0) continue;
    const key = JSON.stringify([
      state.offset,
      state.prefix,
      state.time,
      state.lifetime,
      state.retained,
      state.active,
      state.input,
      state.cohort,
    ]);
    const prior = grouped.get(key);
    if (prior) prior.weight += state.weight;
    else grouped.set(key, { ...state });
  }
  return [...grouped.values()];
}
function adjusted(
  p: BillingPart,
  state: State,
  r: Request,
  ttl: Ttl | null,
): BillingPart | null {
  const size = prefix(p);
  if (state.offset === 0) return p;
  if (
    size === null ||
    p.usage.input === null ||
    ttl === null ||
    size + state.offset < 0 ||
    p.usage.input + state.offset < 0
  )
    return null;
  const remaining = size + state.offset;
  const warm =
    r.time - state.time < state.lifetime && state.prefix !== null
      ? Math.min(remaining, state.prefix)
      : 0;
  // Actual resumed reads can establish external warming of the unchanged prefix.
  const read = Math.max(
    warm,
    Math.min(p.usage.read ?? 0, state.retained, remaining),
  );
  const write = remaining - read;
  return {
    ...p,
    usage: {
      ...p.usage,
      input: p.usage.input + state.offset,
      read,
      write,
      write5m: ttl === "5m" ? write : 0,
      write1h: ttl === "1h" ? write : 0,
    },
  };
}
function changedCost(
  catalog: Catalog,
  r: Request,
  before: BillingPart,
  after: BillingPart | null,
): Cost {
  if (!after)
    return unknownCost(
      "counterfactual retained/replacement prefix cannot be established",
    );
  if (JSON.stringify(before) === JSON.stringify(after)) return zeroCost();
  const a = priceComponents(catalog, r, after);
  const b = priceComponents(catalog, r, before);
  const outputUnit = (p: BillingPart) =>
    priceComponents(catalog, r, { ...p, usage: { ...p.usage, output: 1 } })
      .output;
  return addCosts([
    difference(a.cache, b.cache),
    difference(a.ordinary, b.ordinary),
    difference(outputUnit(after), outputUnit(before)).totalUsd === 0
      ? zeroCost()
      : difference(a.output, b.output),
  ]);
}

function run(options: {
  on: boolean;
  prepared: PreparedPeriod[];
  requests: Request[];
  events: StructuralEvent[];
  samples: CompactionSample[];
  summaries: CompactionCharge[];
  catalog: Catalog;
  since: number;
  ttls: IdleScenario["ttls"];
  mode: "point" | "closed" | "open" | "larger" | "late" | "fraction";
  activationFraction?: number;
  profile: Profile;
  captureOff?: Map<string, Request>;
  captureOn?: Map<string, Request>;
}): Run {
  const costs: Cost[] = [];
  const byPeriod = new Map<string, Cost>();
  let resumed = 0;
  const charge = (id: string | null, cost: Cost, weight: number) => {
    const value = scale(cost, weight);
    costs.push(value);
    if (id) byPeriod.set(id, addCosts([byPeriod.get(id) ?? zeroCost(), value]));
  };
  const requestsById = new Map(options.requests.map((r) => [r.id, r]));
  const summaryRequests = new Set(
    options.summaries.flatMap((s) => (s.request ? [s.request] : [])),
  );
  const chargedBoundaries = new Set<string>();
  const preparedGroups = bySession(options.prepared, (p) => [
    p.previous.session,
  ]);
  const requestGroups = bySession(options.requests, (r) => [r.session]);
  const boundaryGroups = bySession(
    options.events.filter((e) => e.kind === "compaction"),
    (e) => e.origins.map((o) => o.session),
  );
  const summariesByEvent = new Map(options.summaries.map((s) => [s.event, s]));
  const eventsById = new Map(options.events.map((e) => [e.id, e]));
  for (const [session, ps] of preparedGroups) {
    const idleByEvent = new Map(
      ps.filter((p) => p.boundary).map((p) => [p.boundary!.id, p]),
    );
    type Tick =
      | { time: number; kind: "request"; r: Request }
      | { time: number; kind: "boundary"; e: StructuralEvent }
      | { time: number; kind: "idle"; item: PreparedPeriod };
    const ticks: Tick[] = [
      ...(requestGroups.get(session) ?? []).map((r) => ({
        kind: "request" as const,
        time: r.time,
        r,
      })),
      ...(boundaryGroups.get(session) ?? []).map((e) => ({
        kind: "boundary" as const,
        time: e.time,
        e,
      })),
      ...ps
        .filter((p) => !p.boundary && p.period.status === "eligible")
        .map((item) => ({
          kind: "idle" as const,
          time: item.previous.time + (options.mode === "late" ? HOUR : EARLY),
          item,
        })),
    ];
    ticks.sort(
      (a, b) =>
        a.time - b.time ||
        (a.kind === "boundary" ? -1 : b.kind === "boundary" ? 1 : 0),
    );
    let states: State[] = [
      {
        weight: 1,
        offset: 0,
        prefix: null,
        time: -Infinity,
        lifetime: HOUR,
        retained: 0,
        active: null,
        input: null,
        cohort: null,
      },
    ];
    for (const tick of ticks) {
      if (tick.kind === "idle") {
        if (!options.on) continue;
        const {
          period: p,
          previous,
          next,
          window,
          threshold,
          ttls,
        } = tick.item;
        if (tick.time > previous.time + p.gapMs || tick.time < options.since)
          continue;
        const activation =
          options.mode === "fraction" && p.outcome === "unobserved"
            ? options.activationFraction!
            : options.mode === "closed" && p.outcome === "unobserved"
              ? 0
              : options.mode === "open" && p.outcome === "unobserved"
                ? 1
                : p.activation!;
        if (activation === 0) continue;
        const part = previous.parts.find((p) => p.kind === "message")!;
        const matches = options.samples.filter((s) =>
          cohort(previous, part, s),
        );
        const summaryCohorts = new Set(
          matches.map((s) =>
            JSON.stringify([
              s.summaryRequest.provider,
              s.summary.model,
              s.summary.tier,
              s.summary.speed,
              s.summary.geo,
            ]),
          ),
        );
        const branched: State[] = [];
        for (const state of states) {
          const input = (p.contextTokens ?? 0) + state.offset;
          if (threshold !== null && input < threshold) {
            branched.push(state);
            continue;
          }
          if (
            !matches.length ||
            summaryCohorts.size !== 1 ||
            prefix(part) === null ||
            window === null ||
            part.kind !== "message"
          ) {
            if (options.mode === "point")
              p.summaryCost = unknownCost("summary calibration is unresolved");
            charge(
              p.request,
              unknownCost(
                "no compatible linked summary and full post-compaction prefix calibration, or summary pricing cohort is ambiguous",
              ),
              state.weight * activation,
            );
            branched.push(state);
            continue;
          }
          const sample = matches[0]!;
          const size = (values: number[]) =>
            options.mode === "larger"
              ? values.reduce((max, value) => Math.max(max, value), -Infinity)
              : median(values);
          const output = size(matches.map((s) => s.output));
          const replacement = size(matches.map((s) => s.prefix));
          if (options.mode === "point")
            p.calibration = {
              events: matches.map((s) => s.event),
              summaryTokens: output,
              prefixTokens: replacement,
            };
          const retained = Math.min(
            replacement - output,
            size(matches.map((s) => s.p.usage.read ?? 0)),
          );
          if (retained < 0 || replacement >= input || replacement >= window) {
            charge(
              p.request,
              unknownCost(
                "calibrated full post-compaction prefix does not shrink this eligible context",
              ),
              state.weight * activation,
            );
            branched.push(state);
            continue;
          }
          const sameCache =
            sample.summary.model === part.model &&
            sample.summaryRequest.provider === previous.provider &&
            sample.summary.speed === part.speed &&
            sample.summary.geo === part.geo;
          const read =
            sameCache && tick.time - state.time < HOUR
              ? Math.min(input, Math.max(0, prefix(part)! + state.offset))
              : 0;
          const write = input - read;
          const overhead = size(
            matches.map((s) =>
              Math.max(
                0,
                (s.summary.usage.input ?? 0) -
                  (eventsById.get(s.event)?.preTokens ??
                    s.summary.usage.input ??
                    0),
              ),
            ),
          );
          const summary: BillingPart = {
            ...sample.summary,
            usage: {
              ...sample.summary.usage,
              input: input + overhead,
              output,
              read,
              write,
              write5m: ttls.subagent === "5m" ? write : 0,
              write1h: ttls.subagent === "1h" ? write : 0,
            },
          };
          const cost =
            ttls.subagent === null
              ? unknownCost("outside-main summary TTL is unresolved")
              : pricePart(options.catalog, sample.summaryRequest, summary);
          charge(p.request, cost, state.weight * activation);
          if (options.mode === "point")
            p.summaryCost = addCosts([
              p.summaryCost,
              scale(cost, state.weight * activation),
            ]);
          if (next) resumed += state.weight * activation;
          branched.push({ ...state, weight: state.weight * (1 - activation) });
          branched.push({
            weight: state.weight * activation,
            offset: replacement - p.contextTokens!,
            prefix: retained,
            time: sameCache ? tick.time : state.time,
            lifetime: sameCache
              ? ttls.subagent === null
                ? 0
                : TTL_MILLISECONDS[ttls.subagent]
              : state.lifetime,
            retained,
            active: p.request,
            input: state.input,
            cohort: state.cohort,
          });
        }
        states = mergeStates(branched);
      } else if (tick.kind === "boundary") {
        const e = tick.e;
        const source =
          idleByEvent.get(e.id)?.previous ??
          requestsById.get(e.request ?? "") ??
          requestGroups.get(session)![0]!;
        const ttls = scopedTtls(options.profile, source, options.ttls);
        const idle = idleByEvent.get(e.id);
        const summary = summariesByEvent.get(e.id);
        const linked = requestsById.get(summary?.request ?? "");
        const billed =
          linked?.parts.filter((p) => p.kind === "compaction") ?? [];
        const allocated =
          !chargedBoundaries.has(e.id) &&
          (!summary?.request || summary.cost.totalUsd !== 0);
        chargedBoundaries.add(e.id);
        if (idle) {
          const resumedPart = idle.next?.parts.find(
            (p) => p.kind === "message",
          );
          const full = resumedPart ? prefix(resumedPart) : null;
          for (const state of states) {
            state.input = null;
            state.cohort = null;
            if (options.on) {
              const base =
                summary?.request && linked
                  ? addCosts(
                      billed.map((p) => pricePart(options.catalog, linked, p)),
                    )
                  : (summary?.cost ??
                    unknownCost("applied idle summary charge is unavailable"));
              const extra =
                billed.length && linked
                  ? addCosts(
                      billed.map((p) =>
                        changedCost(
                          options.catalog,
                          linked,
                          p,
                          adjusted(p, state, linked, ttls.subagent),
                        ),
                      ),
                    )
                  : state.offset === 0
                    ? zeroCost()
                    : unknownCost(
                        "changed input to an unmatched observed summary cannot be priced",
                      );
              const cost = allocated ? addCosts([base, extra]) : zeroCost();
              if ((linked?.time ?? e.time) >= options.since)
                charge(idle.period.request, cost, state.weight);
              if (options.mode === "point")
                idle.period.summaryCost = addCosts([
                  idle.period.summaryCost,
                  scale(cost, state.weight),
                ]);
              if (idle.next) resumed += state.weight;
              state.offset = 0;
              state.active = null;
            } else {
              if (!idle.next) {
                state.active = null;
                continue;
              }
              if (e.preTokens === null || full === null)
                charge(
                  idle.period.request,
                  unknownCost(
                    "applied idle has no pre-context and full resumed-prefix geometry for the off path",
                  ),
                  state.weight,
                );
              else state.offset += e.preTokens - full;
              state.retained = Math.min(
                resumedPart?.usage.read ?? 0,
                Math.max(
                  0,
                  (full ?? 0) -
                    (billed[0]?.usage.output ?? e.postTokens ?? full ?? 0),
                ),
              );
              state.active = idle.period.request;
            }
          }
        } else {
          // Later observed compactions converge the paths. Charge their changed
          // summary input before dropping the carried context difference.
          for (const state of states) {
            if (allocated && state.offset !== 0 && e.time >= options.since) {
              const extra =
                linked && billed.length
                  ? addCosts(
                      billed.map((p) =>
                        changedCost(
                          options.catalog,
                          linked,
                          p,
                          adjusted(p, state, linked, ttls.subagent),
                        ),
                      ),
                    )
                  : unknownCost(
                      "counterfactual input to a later unmatched compaction is unknown",
                    );
              charge(state.active, extra, state.weight);
            }
            state.offset = 0;
            state.prefix = null;
            state.active = null;
            state.retained = 0;
            state.input = null;
            state.cohort = null;
          }
        }
        states = mergeStates(states);
      } else {
        const r = tick.r;
        const captured = options.on ? options.captureOn : options.captureOff;
        if (captured && !captured.has(r.id))
          captured.set(r.id, {
            ...r,
            parts: r.parts.map((p) => ({ ...p, usage: { ...p.usage } })),
          });
        const ttls = scopedTtls(options.profile, r, options.ttls);
        const messages = r.parts.filter(
          (p) => p.kind === "message" || p.kind === "unknown",
        );
        for (const state of states) {
          for (const p of r.parts) {
            if (p.kind === "compaction" && summaryRequests.has(r.id)) continue;
            if (r.agent === "subagent" || p.kind === "advisor") continue;
            const currentCohort = JSON.stringify([
              r.provider,
              p.model,
              r.agent,
              p.speed,
              p.geo,
            ]);
            const invalid =
              state.offset !== 0 &&
              (p.kind === "message" || p.kind === "unknown") &&
              (messages.length !== 1 ||
                (state.cohort !== null && state.cohort !== currentCohort) ||
                (state.input !== null &&
                  (p.usage.input === null || p.usage.input < state.input)));
            const next = invalid
              ? null
              : messages.length === 1
                ? adjusted(
                    p,
                    state,
                    r,
                    p.kind === "compaction" ? ttls.subagent : ttls.main,
                  )
                : state.offset === 0
                  ? p
                  : null;
            if (captured && state.offset !== 0) {
              const target = captured.get(r.id)!;
              target.parts[r.parts.indexOf(p)] = next ?? {
                ...p,
                usage: { ...p.usage, input: null, read: null, write: null },
              };
            }
            if (r.time >= options.since)
              charge(
                state.active,
                changedCost(options.catalog, r, p, next),
                state.weight,
              );
            if (p.kind === "message" || p.kind === "unknown") {
              state.prefix = next ? prefix(next) : null;
              state.input = p.usage.input;
              state.cohort = currentCohort;
              state.time = r.time;
              state.lifetime =
                ttls.main === null ? 0 : TTL_MILLISECONDS[ttls.main];
            }
          }
        }
        states = mergeStates(states);
      }
    }
    if (options.on)
      for (const { period: p } of ps)
        if (p.status === "unknown") {
          if (options.mode === "point") p.summaryCost = unknownCost(p.reason);
          charge(p.request, unknownCost(p.reason), 1);
        }
  }
  return { adjustment: addCosts(costs), byPeriod, resumed };
}

/** Compare aggregate idle policies after the shared TTL and window recommendations. */
export function idleChecks(options: {
  scan: Scan;
  catalog: Catalog;
  summaries: CompactionCharge[];
  estimates: Map<string, UsageEstimate>;
  ttls: TtlCheckResult[];
  windows: CompactionCheckResult[];
  since: number | undefined;
  now: number;
  /** Joint policy consumers use the same open/closed lifetime assumption for every policy. */
  lifetime?: "open" | "closed";
  /** Capture the deterministic idle-off request geometry for the joint keepalive replay. */
  captureOff?: Map<string, Request>;
  /** Capture deterministic open/awake idle-on geometry for per-request joint costs. */
  captureOn?: Map<string, Request>;
  /** Keep measured summary calibration when a joint replay removes observed idle boundaries. */
  calibrationScan?: Scan;
}): IdleCheckResult[] {
  const { scan, catalog, now } = options;
  const checks: IdleCheckResult[] = [];
  for (const profile of scan.profiles.filter((p) => p.harness === "claude")) {
    const requests = [...scan.cacheContext.requests, ...scan.requests]
      .filter((r) => r.profiles.includes(profile.id))
      .sort((a, b) => a.time - b.time || a.id.localeCompare(b.id));
    const main = scan.requests.filter(
      (r) =>
        r.profiles.includes(profile.id) &&
        r.agent === "main" &&
        r.parts.some((p) => p.kind === "message" || p.kind === "unknown"),
    );
    if (!main.length) continue;
    const events = [...scan.cacheContext.boundaries, ...scan.events]
      .filter((e) => e.profiles.includes(profile.id))
      .sort((a, b) => a.time - b.time);
    const since =
      options.since ??
      main.reduce((first, r) => Math.min(first, r.time), Infinity);
    const calibration = compactionSamples({
      requests: options.calibrationScan
        ? [
            ...options.calibrationScan.cacheContext.requests,
            ...options.calibrationScan.requests,
          ].filter((r) => r.profiles.includes(profile.id))
        : requests,
      events: options.calibrationScan
        ? [
            ...options.calibrationScan.cacheContext.boundaries,
            ...options.calibrationScan.events,
          ].filter((e) => e.profiles.includes(profile.id))
        : events,
      summaries: options.summaries,
    });
    const models = [
      ...new Set(
        main.flatMap((r) =>
          r.parts
            .filter((p) => p.kind === "message" || p.kind === "unknown")
            .map((p) => p.model),
        ),
      ),
    ];
    const currentWindows: IdleScenario["windows"] = [];
    for (const r of main)
      for (const model of models) {
        const c = scopedWindow(profile, r, model);
        if (
          !currentWindows.some(
            (w) => w.owner === c.owner && w.models.includes(model),
          )
        )
          currentWindows.push({
            owner: c.owner,
            models: [model],
            value: c.value,
          });
      }
    const recommendedWindows = currentWindows.map((w) => {
      const check = options.windows.find(
        (c) => c.profile === profile.id && c.owner === w.owner,
      );
      return {
        ...w,
        value: check?.action === "change" ? check.recommended : w.value,
      };
    });
    const currentTtls = {
      main: profile.ttl.main.value,
      subagent: profile.ttl.subagent.value,
    };
    const afterTtls = { ...currentTtls };
    for (const bucket of ["main", "subagent"] as const) {
      const check = options.ttls.find(
        (c) => c.profile === profile.id && c.bucket === bucket,
      );
      if (check?.action === "change") afterTtls[bucket] = check.recommended;
    }
    const controls = new Map(
      main.map((r) => {
        const c = control(profile, r);
        return [c.owner, c] as const;
      }),
    );
    const requestGroups = bySession(requests, (r) => [r.session]);
    const replay = (
      ttls: IdleScenario["ttls"],
      windows: IdleScenario["windows"],
    ) => {
      const windowed: ReturnType<typeof replayWindows> = {
        requests: [],
        events: [],
        added: [],
        errors: [],
        affected: new Set(),
        estimatedParts: new Set(),
      };
      const replayed: ReturnType<typeof replayParts> = [];
      const untouchedEstimates = new Set<string>();
      for (const group of requestGroups.values()) {
        const source =
          group.find(
            (r) =>
              r.agent === "main" && r.parts.some((p) => p.kind === "message"),
          ) ?? group[0]!;
        const policy = scopedTtls(profile, source, ttls);
        const keys = windows.filter((w) =>
          w.models.some(
            (model) => scopedWindow(profile, source, model).owner === w.owner,
          ),
        );
        if (
          group.some(
            (r) =>
              JSON.stringify(scopedTtls(profile, r, ttls)) !==
                JSON.stringify(policy) ||
              keys.some((w) =>
                w.models.some(
                  (model) => scopedWindow(profile, r, model).owner !== w.owner,
                ),
              ),
          )
        )
          windowed.errors.push(
            "effective project TTL/window policy changes within a conversation; its counterfactual is unresolved",
          );
        const part = replayWindows({
          requests: group,
          events,
          estimates: options.estimates,
          windows: keys,
          catalog,
          samples: calibration,
          since,
          ttls: policy,
        });
        for (const r of part.requests) windowed.requests.push(r);
        for (const e of part.events) windowed.events.push(e);
        for (const added of part.added) windowed.added.push(added);
        for (const error of part.errors) windowed.errors.push(error);
        if (part.requests === group)
          for (const r of group) untouchedEstimates.add(r.id);
        for (const item of replayParts({
          requests: part.requests,
          events: [...events, ...part.events],
          estimates: part.requests === group ? options.estimates : new Map(),
          catalog,
          ttls: { ...policy, unknown: null },
          since,
          estimatedParts: part.estimatedParts,
        }))
          replayed.push(item);
      }
      windowed.requests.sort(
        (a, b) => a.time - b.time || a.id.localeCompare(b.id),
      );
      const allEvents = [...events, ...windowed.events].sort(
        (a, b) => a.time - b.time,
      );
      const replayedParts = new Map(
        replayed.map((i) => [`${i.request.id}:${i.part}`, i.p]),
      );
      const rs = windowed.requests.map((r) => {
        const estimate = untouchedEstimates.has(r.id)
          ? options.estimates.get(r.id)?.part
          : null;
        return {
          ...r,
          parts: (estimate ? [estimate] : r.parts).map(
            (p, part) => replayedParts.get(`${r.id}:${part}`) ?? p,
          ),
        };
      });
      return { windowed, replayed, allEvents, rs };
    };
    const currentReplay = replay(currentTtls, currentWindows);
    const recommendedReplay = replay(afterTtls, recommendedWindows);
    for (const [owner, c] of controls) {
      const scenario = (
        base: ReturnType<typeof replay>,
        ttls: IdleScenario["ttls"],
        windows: IdleScenario["windows"],
        baseline: string,
      ): IdleScenario => {
        const { windowed, replayed, allEvents, rs } = base;
        const { prepared, activationCohorts } = periods({
          requests: rs,
          events: allEvents,
          profile,
          since,
          now,
          ttls,
          windows,
          owner,
        });
        const idleSummaryEvents = new Set(
          prepared.flatMap((p) => (p.boundary ? [p.boundary.id] : [])),
        );
        const idleSummaryRequests = new Set(
          options.summaries
            .filter(
              (s) => idleSummaryEvents.has(s.event) && s.cost.totalUsd !== 0,
            )
            .flatMap((s) => (s.request ? [s.request] : [])),
        );
        const recordedBaselineCost = addCosts([
          ...windowed.errors.map(unknownCost),
          ...replayed
            .filter((i) => i.selected)
            .map((i) => addCosts(Object.values(i.components))),
          ...options.summaries
            .filter(
              (s) =>
                !s.request &&
                allEvents.some((e) => e.id === s.event && e.time >= since),
            )
            .map((s) => s.cost),
          ...rs.filter((r) => r.time >= since).map(serverCost),
          ...windowed.added.map((a) => a.summaryCost),
        ]);
        const common = addCosts([
          ...windowed.errors.map(unknownCost),
          ...replayed
            .filter(
              (i) =>
                i.selected &&
                !(
                  i.p.kind === "compaction" &&
                  idleSummaryRequests.has(i.request.id)
                ),
            )
            .map((i) => addCosts(Object.values(i.components))),
          ...options.summaries
            .filter(
              (s) =>
                !s.request &&
                !idleSummaryEvents.has(s.event) &&
                allEvents.some((e) => e.id === s.event && e.time >= since),
            )
            .map((s) => s.cost),
          ...rs.filter((r) => r.time >= since).map(serverCost),
          ...windowed.added.map((a) => a.summaryCost),
        ]);
        const args = {
          prepared,
          requests: rs,
          events: allEvents,
          samples: calibration,
          summaries: options.summaries,
          catalog,
          since,
          ttls,
          profile,
        };
        const off = run({
          ...args,
          on: false,
          mode: "point",
          ...(base === recommendedReplay && options.captureOff
            ? { captureOff: options.captureOff }
            : {}),
        });
        const on = run({
          ...args,
          on: true,
          mode: options.lifetime ?? "point",
          ...(base === recommendedReplay &&
          options.captureOn &&
          options.lifetime === "open"
            ? { captureOn: options.captureOn }
            : {}),
        });
        const delta = (r: Run) =>
          difference(r.adjustment, off.adjustment).totalUsd;
        const closed = delta(run({ ...args, on: true, mode: "closed" }));
        const open = delta(run({ ...args, on: true, mode: "open" }));
        let lifetimeBreakEvenFraction: number | null = null;
        if (closed !== null && open !== null) {
          if (closed === 0) lifetimeBreakEvenFraction = 0;
          else if (open === 0) lifetimeBreakEvenFraction = 1;
          else if (Math.sign(closed) !== Math.sign(open)) {
            let low = 0;
            let high = 1;
            while (true) {
              const middle = (low + high) / 2;
              if (high - low <= Number.EPSILON) {
                lifetimeBreakEvenFraction = middle;
                break;
              }
              const value = delta(
                run({
                  ...args,
                  on: true,
                  mode: "fraction",
                  activationFraction: middle,
                }),
              );
              if (value === null) break;
              if (value === 0) {
                lifetimeBreakEvenFraction = middle;
                break;
              }
              if (Math.sign(value) === Math.sign(closed)) low = middle;
              else high = middle;
            }
          }
        }
        const unknown = [
          ...new Set([
            ...windowed.errors,
            ...on.adjustment.unknown,
            ...off.adjustment.unknown,
          ]),
        ];
        const applicable = prepared.some(
          (p) =>
            p.ttls.main !== "5m" &&
            control(profile, p.previous).autoCompact !== false &&
            (p.window === null ||
              p.threshold === null ||
              p.window > p.threshold),
        );
        const deltaUsd = unknown.length ? null : delta(on);
        const sizes = [...new Set(prepared.map((p) => p.period.contextTokens))];
        return {
          baseline,
          applicable,
          ttls,
          windows,
          periods: prepared.map((p) => p.period),
          cost: {
            off: addCosts([common, off.adjustment]),
            on: addCosts([common, on.adjustment]),
          },
          recordedBaselineCost,
          adjustment: { off: off.adjustment, on: on.adjustment },
          deltaUsd,
          monthlyDeltaUsd:
            deltaUsd !== null && now > since
              ? (deltaUsd * 30 * 86400000) / (now - since)
              : null,
          byContext: sizes.map((tokens) => {
            const selected = prepared.filter(
              (p) => p.period.contextTokens === tokens,
            );
            return {
              tokens,
              periods: selected.length,
              deltaUsd: difference(
                addCosts(
                  selected.map(
                    (p) => on.byPeriod.get(p.period.request) ?? zeroCost(),
                  ),
                ),
                addCosts(
                  selected.map(
                    (p) => off.byPeriod.get(p.period.request) ?? zeroCost(),
                  ),
                ),
              ).totalUsd,
            };
          }),
          activationCohorts,
          sensitivity: {
            closedDeltaUsd: closed,
            openDeltaUsd: open,
            largerSummaryDeltaUsd: delta(
              run({ ...args, on: true, mode: "larger" }),
            ),
            lateTriggerDeltaUsd: delta(
              run({ ...args, on: true, mode: "late" }),
            ),
            lifetimeBreakEvenFraction,
            rereadBreakEvenUsdPerResumedSummary:
              deltaUsd !== null && deltaUsd < 0 && on.resumed > 0
                ? -deltaUsd / on.resumed
                : null,
          },
          unknown,
        };
      };
      const currentSettings = scenario(
        currentReplay,
        currentTtls,
        currentWindows,
        "idle on/off at current TTL and current window controls",
      );
      const afterRecommendations = scenario(
        recommendedReplay,
        afterTtls,
        recommendedWindows,
        "idle on/off after applicable TTL and compaction-window changes; independent savings are not additive",
      );
      const delta = afterRecommendations.deltaUsd;
      const winner =
        delta === null ? null : delta < 0 ? true : delta > 0 ? false : c.value;
      const recent = main
        .filter((r) => control(profile, r).owner === owner)
        .at(-1)!;
      const applicable =
        c.resolved &&
        c.value !== null &&
        client(recent) === "supported" &&
        supported(recent.version, 290) &&
        afterRecommendations.applicable &&
        (supported(recent.version, 314) ||
          historical(events, recent).idleCompactionAvailable === true);
      const savings =
        delta === null || c.value === null
          ? null
          : winner === c.value
            ? 0
            : Math.abs(delta);
      checks.push({
        check: "idle-compaction",
        bucket: "main",
        profile: profile.id,
        historyProfiles: [...new Set(main.flatMap((r) => r.profiles))],
        evidence: delta === null ? "unknown" : "estimated",
        action:
          applicable && winner !== null
            ? winner === c.value
              ? "keep"
              : "change"
            : "advisory",
        current: c.value,
        recommended: afterRecommendations.applicable ? winner : null,
        owner,
        threshold: c.threshold,
        since,
        until: now,
        windowBasis:
          options.since === undefined
            ? "available transcript window replayed under today's settings; historical on/off state is used only where recorded"
            : "explicit --since window replayed under today's settings; historical on/off state is used only where recorded",
        baseline: afterRecommendations.baseline,
        requests: main.filter((r) => r.time >= since).length,
        currentSettings,
        afterRecommendations,
        usageEstimates: [...options.estimates.values()].filter((e) =>
          requests.some((r) => r.id === e.request),
        ),
        savingsUsd: savings,
        monthlySavingsUsd:
          savings !== null && now > since
            ? (savings * 30 * 86400000) / (now - since)
            : null,
        breakEven:
          afterRecommendations.sensitivity.lifetimeBreakEvenFraction === null
            ? null
            : `uncertain-period open/awake activation fraction ${afterRecommendations.sensitivity.lifetimeBreakEvenFraction} is a modeled break-even between the stated closure/open endpoints`,
        assumptions: [
          c.reason,
          `effective idle threshold ${c.threshold ?? "unknown"}; owner ${c.thresholdOwner}; threshold tuning is outside this check`,
          "delta is on cost minus off cost over the entire selected window, including final unresumed summaries; the aggregate point estimate decides, with a zero tie retaining the current value",
          "documented near-expiry trigger is modeled at 90% of one hour from inspected native v2.1.316; one-hour trigger sensitivity covers near-expiry returns; this timing is estimated for other versions and is not a documented timer control",
          "summary-to-resume delay uses that estimated trigger, never boundary time or durationMs; summary input reads the prior warm prefix and writes new context at the outside-main TTL",
          "hypothetical summaries use cohort median linked billed output and full first resumed cache prefix, including system/project/retained/new context, plus observed incremental growth; maxima supply summary-size sensitivity",
          "unchanged system/project prefix is estimated from post-compaction reads; actual resumed usage takes precedence for an applied idle; altered prefixes reuse only within their chosen lifetime and cannot inherit external warming beyond the unchanged prefix",
          "context changes carry forward through later requests and reprice context tiers and later summaries until an observed compaction converges the paths; observed summary usage and unmatched proxies are charged once",
          "activation transfers only within the same known version/client/flag cohort; without comparable recorded known-on outcomes the deciding fallback leaves the client open and awake; final periods incur one expected summary with no assumed return benefit",
          "closed/open sensitivity changes unobserved activation once, without a second lifetime probability; a bracketed break-even replays a common uncertain activation fraction, including sequential context changes; no bracket is not proof that no internal crossing exists",
          "fixed transcript behavior holds outputs, tools and incremental growth constant; lost detail can cause extra re-reads, whose per-resumed-summary break-even is a caveat, not an unbounded veto",
        ],
        uncertainty: [
          ...new Set([
            "missing applied boundaries do not prove that unlogged or discarded summaries never ran; their unobserved charges are not measured or included as known zero, and can change this conditional API-price estimate",
            "native transcripts do not establish historical idle settings, feature-flag fetch state or closed/asleep client lifetime; precomputed alone is not idle attribution; unknown eligibility or missing full-prefix calibration remains unpriced",
            "Desktop labels alone do not establish local execution; CLI labels require REPL provenance; T3 proactive policies are outside this native check",
            ...(!applicable
              ? [
                  !afterRecommendations.applicable
                    ? "idle compaction is inapplicable after TTL/window recommendations or disabled automatic compaction"
                    : "effective ownership or current native version/flag support is unresolved; this is advisory",
                ]
              : []),
            ...afterRecommendations.unknown,
          ]),
        ],
        sources: [...SOURCES, ...PRICE_SOURCES],
      });
    }
  }
  return checks;
}
