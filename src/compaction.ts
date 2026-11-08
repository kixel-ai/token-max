import type {
  AddedCompaction,
  BillingPart,
  CompactionCharge,
  CompactionCheckResult,
  Cost,
  Request,
  Scan,
  StructuralEvent,
  Ttl,
  TtlCheckResult,
  UsageEstimate,
  WindowScenario,
} from "./model.js";
import { TTL_MILLISECONDS, CODEX_CACHE_MILLISECONDS } from "./cache.js";
import { CLAUDE_WINDOW_BOUNDS, windowControl } from "./controls.js";
import { replayParts, type ReplayedPart } from "./scenarios.js";
import {
  addCosts,
  pairedSavings,
  priceComponents,
  pricePart,
  serverCost,
  unknownCost,
  zeroCost,
  writeBilling,
  PRICE_SOURCES,
  type Catalog,
} from "./prices.js";

const SOURCES = [
  "https://code.claude.com/docs/en/model-config#context-window-and-auto-compaction (2026-11-07)",
  "https://code.claude.com/docs/en/settings-reference#modelsettings (2026-11-07)",
  "https://developers.openai.com/codex/config-reference (2026-11-07)",
  "https://developers.openai.com/api/docs/guides/compaction (2026-11-07)",
];

/** Linked summary output and the full first post-compaction cache prefix. */
export interface CompactionSample {
  event: string;
  request: Request;
  p: BillingPart;
  output: number;
  prefix: number;
  summaryRequest: Request;
  summary: BillingPart;
}

function sameCohort(
  r: Request,
  p: BillingPart,
  sample: CompactionSample,
): boolean {
  return (
    p.model !== null &&
    r.provider !== null &&
    r.agent !== "unknown" &&
    p.model === sample.p.model &&
    r.provider === sample.request.provider &&
    r.agent === sample.request.agent &&
    p.tier === sample.p.tier &&
    p.speed === sample.p.speed &&
    p.geo === sample.p.geo
  );
}

function median(values: number[]): number {
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2
    ? sorted[mid]!
    : Math.ceil((sorted[mid - 1]! + sorted[mid]!) / 2);
}

function prefix(r: Request, p: BillingPart): number | null {
  const u = p.usage;
  return r.harness === "codex"
    ? u.input
    : u.read === null || u.write === null
      ? null
      : u.read + u.write;
}

/** Calibrate summaries from billed output and full resumed requests, never postTokens alone. */
export function compactionSamples(options: {
  requests: Request[];
  events: StructuralEvent[];
  summaries: CompactionCharge[];
}): CompactionSample[] {
  const result: CompactionSample[] = [];
  const byId = new Map(options.requests.map((r) => [r.id, r]));
  const boundaries = options.events.filter((e) => e.kind === "compaction");
  for (const event of boundaries) {
    const aliases = new Set(event.origins.map((o) => o.session));
    const nextBoundary = boundaries
      .filter(
        (e) =>
          e.time > event.time && e.origins.some((o) => aliases.has(o.session)),
      )
      .reduce((t, e) => Math.min(t, e.time), Infinity);
    const next = options.requests.find(
      (r) =>
        aliases.has(r.session) &&
        r.time > event.time &&
        r.time < nextBoundary &&
        r.parts.some((p) => p.kind === "message"),
    );
    if (!next || next.parts.filter((p) => p.kind === "message").length !== 1)
      continue;
    const p = next.parts.find((p) => p.kind === "message")!;
    if ((event.model ?? event.pricing?.model) !== p.model) continue;
    const size = prefix(next, p);
    if (size === null || size <= 0 || next.evidence !== "measured") continue;
    const charge = options.summaries.find((c) => c.event === event.id);
    const linked = byId.get(charge?.request ?? event.request ?? "");
    const parts = linked?.parts.filter((p) => p.kind === "compaction") ?? [];
    const summary = parts.length === 1 ? parts[0]! : null;
    // The resumed context can contain retained items and instructions. It is not
    // evidence of billed summary output; unmatched proxies stay in the baseline.
    if (
      !linked ||
      !summary ||
      summary.usage.output === null ||
      summary.usage.output <= 0
    )
      continue;
    const output = summary.usage.output;
    const summaryRequest = linked;
    const summaryPart = summary;
    result.push({
      event: event.id,
      request: next,
      p,
      output,
      prefix: size,
      summaryRequest,
      summary: summaryPart,
    });
  }
  return result;
}

function summaryPart(
  sample: CompactionSample,
  input: number,
  output: number,
  cold: boolean,
): BillingPart {
  const u = {
    ...sample.summary.usage,
    input,
    output,
    reasoning: null,
    read: 0,
    write: 0,
    write5m: 0,
    write1h: 0,
    write30m: 0,
    write12h: 0,
  };
  if (cold) {
    u.write = input;
    if (sample.summaryRequest.harness === "claude") u.write1h = input;
    else if (writeBilling(sample.summary.model ?? "")) u.write12h = input;
    else u.write = 0;
  }
  return { ...sample.summary, kind: "compaction", usage: u };
}

function total(item: ReplayedPart): Cost {
  return addCosts(Object.values(item.components));
}

function outputUnit(catalog: Catalog, item: ReplayedPart): Cost {
  return priceComponents(catalog, item.request, {
    ...item.p,
    usage: { ...item.p.usage, output: 1 },
  }).output;
}

interface Simulation {
  requests: Request[];
  added: AddedCompaction[];
  errors: string[];
  /** Sessions whose replay is unknown from this time on; other sessions stay priced. */
  uncertain: Map<string, number>;
  affected: Set<string>;
  events: StructuralEvent[];
  estimatedParts: Set<string>;
}

function simulate(options: {
  requests: Request[];
  events: StructuralEvent[];
  estimates: Map<string, UsageEstimate>;
  window: number;
  models: (string | null)[];
  catalog: Catalog;
  samples: CompactionSample[];
  since: number;
  larger: boolean;
  cold: boolean;
  ttls: Record<"main" | "subagent", Ttl | null>;
}): Simulation {
  const result: Simulation = {
    requests: [],
    added: [],
    errors: [],
    uncertain: new Map(),
    affected: new Set(),
    events: [],
    estimatedParts: new Set(),
  };
  const states = new Map<
    string,
    {
      removed: number;
      input: number | null;
      cohort: string;
      time: number;
      prefix: number | null;
    }
  >();
  const boundaries = new Map<string, number[]>();
  for (const event of options.events.filter((e) => e.kind === "compaction"))
    for (const origin of event.origins) {
      const times = boundaries.get(origin.session) ?? [];
      times.push(event.time);
      boundaries.set(origin.session, times);
    }
  for (const r of options.requests) {
    const fail = (reason: string) => {
      result.errors.push(reason);
      if (!result.uncertain.has(r.session))
        result.uncertain.set(r.session, r.time);
    };
    const billing = options.estimates.get(r.id)?.part;
    const parts = billing ? [billing] : r.parts;
    let state = states.get(r.session);
    const previousTime = state?.time;
    if (
      previousTime !== undefined &&
      (boundaries.get(r.session) ?? []).some(
        (t) => t > previousTime && t <= r.time,
      )
    )
      state = undefined;
    const messages = parts.filter(
      (p) => p.kind === "message" || p.kind === "unknown",
    );
    const clone: Request = {
      ...r,
      evidence: billing ? "estimated" : r.evidence,
      parts: parts.map((p) => ({ ...p, usage: { ...p.usage } })),
    };
    result.requests.push(clone);
    for (const [index, original] of parts.entries()) {
      if (original.kind !== "message" && original.kind !== "unknown") continue;
      const eligible =
        options.models.includes(original.model) || original.model === null;
      const p = clone.parts[index]!;
      const u = p.usage;
      const cohort = JSON.stringify([r.provider, p.model, r.agent, p.speed]);
      state ??= { removed: 0, input: null, cohort, time: r.time, prefix: null };
      const selected = r.time >= options.since;
      // Unknown usage could cross any window; it is not an unaffected zero-cost request.
      if (
        selected &&
        eligible &&
        (u.input === null ||
          ((prefix(r, p) === null || p.kind === "unknown") &&
            (state.removed > 0 || u.input > options.window)))
      ) {
        fail(
          "missing input/cache geometry or iteration semantics can affect window eligibility",
        );
        state.input = null;
        continue;
      }
      if (
        state.removed > 0 &&
        (messages.length !== 1 ||
          state.cohort !== cohort ||
          u.input === null ||
          (state.input !== null && u.input < state.input))
      ) {
        fail(
          "compacted context crosses multipart, model/agent/provider/speed changes or an unexplained input decrease",
        );
        state.removed = 0;
      }
      const oldPrefix = prefix(r, p);
      if (
        state.removed > 0 &&
        oldPrefix !== null &&
        oldPrefix < state.removed
      ) {
        fail("recorded prefix cannot contain the modeled removed context");
        state.removed = 0;
      }
      let added: AddedCompaction | null = null;
      if (
        selected &&
        eligible &&
        u.input !== null &&
        u.input - state.removed > options.window
      ) {
        const matches = options.samples.filter((s) => sameCohort(r, p, s));
        if (messages.length !== 1 || !matches.length || oldPrefix === null) {
          fail(
            "no compatible measured compaction summary and resumed-prefix sample, or ambiguous multipart request",
          );
        } else {
          const template = matches[0]!;
          // Mixed summary models cannot establish which model an added pass would use.
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
          if (summaryCohorts.size !== 1)
            fail(
              "compatible samples disagree about summary model/provider/tier/speed/geography",
            );
          const output = options.larger
            ? matches.reduce((n, s) => Math.max(n, s.output), 0)
            : median(matches.map((s) => s.output));
          const resumed = options.larger
            ? matches.reduce((n, s) => Math.max(n, s.prefix), 0)
            : median(matches.map((s) => s.prefix));
          const ordinary = u.input - oldPrefix;
          if (
            resumed + ordinary > options.window ||
            resumed >= oldPrefix - state.removed
          ) {
            fail(
              "observed resumed-prefix proxy does not shrink this context below the candidate window",
            );
          } else {
            added = {
              request: r.id,
              inputTokens: u.input - state.removed,
              summaryTokens: output,
              prefixTokens: resumed,
              basis: matches.map((s) => ({
                event: s.event,
                request: s.request.id,
              })),
              summaryCost: pricePart(
                options.catalog,
                template.summaryRequest,
                summaryPart(
                  template,
                  u.input - state.removed,
                  output,
                  options.cold,
                ),
              ),
              prefixCost: zeroCost(),
            };
            result.added.push(added);
            const event = options.events.find((e) => e.id === template.event)!;
            result.events.push({
              ...event,
              id: `modeled:${r.id}:${index}`,
              time: r.time,
              session: r.session,
              origins: r.locations.map((location) => ({
                session: r.session,
                location,
              })),
              request: null,
            });
            state.removed = oldPrefix - resumed;
          }
        }
      }
      if (
        selected &&
        state.removed > 0 &&
        u.input !== null &&
        oldPrefix !== null
      ) {
        const remaining = oldPrefix - state.removed;
        u.input -= state.removed;
        const ttl = options.ttls[r.agent === "subagent" ? "subagent" : "main"];
        const billedWrites =
          r.harness === "codex" ? writeBilling(p.model ?? "") : true;
        const lifetime =
          r.harness === "claude"
            ? ttl === null
              ? null
              : TTL_MILLISECONDS[ttl]
            : billedWrites
              ? CODEX_CACHE_MILLISECONDS
              : null;
        // A newly generated prefix cannot inherit the old transcript's external warming.
        const warm =
          !added &&
          lifetime !== null &&
          r.time - state.time < lifetime &&
          state.prefix !== null;
        if (
          r.harness === "codex" &&
          billedWrites !== true &&
          !added &&
          (u.read ?? 0) > 0
        )
          fail(
            "counterfactual prefix cache retention is not established for this Codex model",
          );
        u.read = warm ? Math.min(state.prefix!, remaining) : 0;
        if (r.harness === "claude") {
          u.write = remaining - u.read;
          u.write5m = ttl === "5m" ? u.write : 0;
          u.write1h = ttl === "1h" ? u.write : 0;
        } else if (billedWrites) {
          if (original.usage.write === null)
            fail(
              "Codex cache-write geometry is missing for an affected request",
            );
          u.write = remaining - u.read;
          u.write30m = u.write;
          u.write12h = 0;
        }
        state.prefix = remaining;
        result.estimatedParts.add(`${r.id}:${index}`);
        result.affected.add(r.id);
        if (added) {
          const justPrefix = {
            ...p,
            usage: {
              ...u,
              output: 0,
              input: u.input,
              read: 0,
              write: remaining,
              write5m: u.write5m,
              write1h: u.write1h,
              write30m: remaining,
              write12h: 0,
            },
          };
          // Attribute only the new prefix charge; it already lives in this request's total.
          const components = priceComponents(
            options.catalog,
            clone,
            justPrefix,
          );
          added.prefixCost =
            r.harness === "claude" || writeBilling(p.model ?? "")
              ? components.cache
              : pricePart(options.catalog, clone, {
                  ...justPrefix,
                  usage: { ...justPrefix.usage, input: remaining, write: 0 },
                });
        }
      }
      state.input = original.usage.input;
      state.cohort = cohort;
      state.time = r.time;
      states.set(r.session, state);
    }
  }
  return result;
}

/** Apply selected window controls using the same simulation as the window check. */
export function replayWindows(options: {
  requests: Request[];
  events: StructuralEvent[];
  estimates: Map<string, UsageEstimate>;
  windows: { value: number | null; models: (string | null)[] }[];
  catalog: Catalog;
  samples: CompactionSample[];
  since: number;
  ttls: Record<"main" | "subagent", Ttl | null>;
}): Simulation {
  const result: Simulation = {
    requests: options.requests,
    events: [],
    added: [],
    errors: [],
    uncertain: new Map(),
    affected: new Set(),
    estimatedParts: new Set(),
  };
  for (const window of options.windows) {
    if (window.value === null) continue;
    const next = simulate({
      ...options,
      requests: result.requests,
      events: [...options.events, ...result.events],
      estimates:
        result.requests === options.requests ? options.estimates : new Map(),
      window: window.value,
      models: window.models,
      larger: false,
      cold: false,
    });
    result.requests = next.requests;
    result.events.push(...next.events);
    result.added.push(...next.added);
    result.errors.push(...next.errors);
    for (const [session, time] of next.uncertain)
      result.uncertain.set(
        session,
        Math.min(time, result.uncertain.get(session) ?? time),
      );
    for (const id of next.affected) result.affected.add(id);
    for (const id of next.estimatedParts) result.estimatedParts.add(id);
  }
  return result;
}

/** Compare compaction windows after the recommended TTLs, without summing independent savings. */
export function compactionChecks(options: {
  scan: Scan;
  catalog: Catalog;
  summaries: CompactionCharge[];
  estimates: Map<string, UsageEstimate>;
  ttls: TtlCheckResult[];
  since: number | undefined;
  now: number;
}): CompactionCheckResult[] {
  const { scan, catalog, now } = options;
  const checks: CompactionCheckResult[] = [];
  for (const profile of scan.profiles) {
    const requests = [...scan.cacheContext.requests, ...scan.requests]
      .filter((r) => r.profiles.includes(profile.id))
      .sort((a, b) => a.time - b.time || a.id.localeCompare(b.id));
    const events = [...scan.cacheContext.boundaries, ...scan.events].filter(
      (e) => e.profiles.includes(profile.id),
    );
    const selectedIds = new Set(scan.requests.map((r) => r.id));
    const selected = requests.filter((r) => selectedIds.has(r.id));
    if (!selected.length) continue;
    const ttlResults = options.ttls.filter((t) => t.profile === profile.id);
    const ttls: Record<"main" | "subagent", Ttl | null> = {
      main:
        ttlResults.find((t) => t.bucket === "main")?.recommended ??
        profile.ttl.main.value,
      subagent:
        ttlResults.find((t) => t.bucket === "subagent")?.recommended ??
        profile.ttl.subagent.value,
    };
    const firstSelected = selected.reduce(
      (n, r) => Math.min(n, r.time),
      Infinity,
    );
    const firstBoundary = scan.events
      .filter((e) => e.profiles.includes(profile.id) && e.kind === "compaction")
      .reduce((n, e) => Math.min(n, e.time), Infinity);
    const since =
      options.since ??
      ttlResults
        .filter((t) => t.windowBasisKind === "ttl-transition")
        .reduce(
          (n, t) => Math.max(n, t.since ?? -Infinity),
          Math.min(firstSelected, firstBoundary),
        );
    const replay = (
      rs: Request[],
      estimates = options.estimates,
      extraEvents: StructuralEvent[] = [],
      estimatedParts = new Set<string>(),
    ) =>
      replayParts({
        requests: rs,
        events: [...events, ...extraEvents],
        estimates,
        catalog,
        ttls: { ...ttls, unknown: null },
        estimatedParts,
        since,
      });
    const baseline = replay(requests).filter((i) => i.selected);
    const summaryIds = new Set(
      events.filter((e) => e.time >= since).map((e) => e.id),
    );
    const unmatched = addCosts(
      options.summaries
        .filter((s) => summaryIds.has(s.event) && s.request === null)
        .map((s) => s.cost),
    );
    const fixed = addCosts([
      unmatched,
      ...selected.filter((r) => r.time >= since).map(serverCost),
    ]);
    const recordedCost = addCosts([fixed, ...baseline.map(total)]);
    const observedSummaryCost = addCosts([
      unmatched,
      ...baseline.filter((i) => i.p.kind === "compaction").map(total),
    ]);
    const calibration = compactionSamples({
      requests,
      events,
      summaries: options.summaries,
    });
    const observedModels = [
      ...new Set(
        baseline
          .filter((i) => i.p.kind === "message" || i.p.kind === "unknown")
          .map((i) => i.p.model),
      ),
    ];
    // One effective key has one aggregate winner, including global/environment controls.
    const groups = new Map<
      string,
      { control: ReturnType<typeof windowControl>; models: (string | null)[] }
    >();
    for (const model of observedModels) {
      const control = windowControl(
        profile,
        profile.harness === "codex" ? null : model,
      );
      const group = groups.get(control.owner) ?? { control, models: [] };
      group.models.push(model);
      groups.set(control.owner, group);
    }
    for (const { control, models } of groups.values()) {
      const model = models.length === 1 ? models[0]! : null;
      const relevant = baseline
        .filter((i) => i.p.kind === "message" || i.p.kind === "unknown")
        .filter((i) => models.includes(i.p.model) || i.p.model === null);
      if (!relevant.length) continue;
      const minimum =
        profile.harness === "claude" ? CLAUDE_WINDOW_BOUNDS.minimum : 1;
      const ceiling =
        control.value ??
        relevant.reduce((n, i) => Math.max(n, i.p.usage.input ?? 0), 0);
      const candidates = new Set<number>();
      if (profile.harness === "claude") candidates.add(minimum);
      // Economic discontinuities and measured replacement geometry form a small,
      // evidence-derived family instead of one replay per distinct request size.
      for (const i of relevant)
        for (const tier of catalog.models.get(
          `${i.request.provider}:${i.p.model}`,
        )?.tiers ?? [])
          candidates.add(tier.size);
      const geometries = new Map<string, number[]>();
      for (const sample of calibration.filter((s) =>
        models.includes(s.p.model),
      )) {
        const key = JSON.stringify([
          sample.request.provider,
          sample.p.model,
          sample.request.agent,
          sample.p.tier,
          sample.p.speed,
          sample.p.geo,
        ]);
        const sizes = geometries.get(key) ?? [];
        sizes.push(sample.prefix);
        geometries.set(key, sizes);
      }
      const ordinary = relevant.reduce(
        (n, i) =>
          Math.max(
            n,
            i.p.usage.input === null || prefix(i.request, i.p) === null
              ? 0
              : i.p.usage.input - prefix(i.request, i.p)!,
          ),
        0,
      );
      for (const sizes of geometries.values())
        candidates.add(median(sizes) + ordinary);
      if (control.value !== null) candidates.add(control.value);
      const windows = [...candidates]
        .filter(
          (n) =>
            n >= minimum &&
            n <= ceiling &&
            (profile.harness !== "claude" || n <= CLAUDE_WINDOW_BOUNDS.maximum),
        )
        .sort((a, b) => a - b);
      const raw = (window: number, larger: boolean, cold: boolean) => {
        const simulation = simulate({
          requests,
          events,
          estimates: options.estimates,
          window,
          models,
          catalog,
          samples: calibration,
          since,
          larger,
          cold,
          ttls,
        });
        const after = replay(
          simulation.requests,
          new Map(),
          simulation.events,
          simulation.estimatedParts,
        ).filter((i) => i.selected);
        if (control.comparisonUnknown)
          simulation.errors.push(control.comparisonUnknown);
        const errors = simulation.errors.length
          ? [unknownCost([...new Set(simulation.errors)].join("; "))]
          : [];
        return {
          simulation,
          after,
          errors,
          cost: addCosts([
            fixed,
            ...after.map(total),
            ...simulation.added.map((a) => a.summaryCost),
            ...errors,
          ]),
        };
      };
      const current =
        control.value === null ? null : raw(control.value, false, false);
      const largerCurrent =
        control.value === null ? null : raw(control.value, true, false);
      const coldCurrent =
        control.value === null ? null : raw(control.value, false, true);
      const baselineCost = current?.cost ?? recordedCost;
      const run = (window: number, larger: boolean, cold: boolean) => {
        const before = larger ? largerCurrent : cold ? coldCurrent : current;
        const point =
          window === control.value && before
            ? before
            : raw(window, larger, cold);
        const previous = before?.after ?? baseline;
        // One pair per request that gains or loses a summary, so the unpriced
        // bound compares against single differences rather than their total.
        const summaries = new Map<
          string,
          { baseline: Cost[]; alternative: Cost[] }
        >();
        const summary = (request: string) => {
          const entry = summaries.get(request) ?? {
            baseline: [],
            alternative: [],
          };
          summaries.set(request, entry);
          return entry;
        };
        for (const a of before?.simulation.added ?? [])
          summary(a.request).baseline.push(a.summaryCost);
        for (const a of point.simulation.added)
          summary(a.request).alternative.push(a.summaryCost);
        const pairs = [...summaries.values()].map((s) => ({
          baseline: addCosts(s.baseline),
          alternative: addCosts(s.alternative),
        }));
        const uncertain = (r: Request) =>
          [before?.simulation.uncertain, point.simulation.uncertain].some(
            (sessions) => (sessions?.get(r.session) ?? Infinity) <= r.time,
          );
        for (const [index, item] of point.after.entries()) {
          const original = previous[index]!;
          if (uncertain(item.request)) {
            const reason =
              "session replay is unknown after a request it cannot model";
            pairs.push({
              baseline: unknownCost(reason),
              alternative: unknownCost(reason),
            });
            continue;
          }
          if (JSON.stringify(original.p) === JSON.stringify(item.p)) continue;
          const baselineParts = [
            original.components.cache,
            original.components.ordinary,
          ];
          const alternativeParts = [
            item.components.cache,
            item.components.ordinary,
          ];
          const oldOutput = outputUnit(catalog, original).totalUsd;
          const newOutput = outputUnit(catalog, item).totalUsd;
          if (
            oldOutput === null ||
            newOutput === null ||
            oldOutput !== newOutput
          ) {
            baselineParts.push(original.components.output);
            alternativeParts.push(item.components.output);
          }
          pairs.push({
            baseline: addCosts(baselineParts),
            alternative: addCosts(alternativeParts),
          });
        }
        const beforeErrors = before?.errors ?? [];
        const baselineComparisonCost = addCosts([
          ...pairs.map((p) => p.baseline),
          ...beforeErrors,
        ]);
        const comparisonCost = addCosts([
          ...pairs.map((p) => p.alternative),
          ...point.errors,
        ]);
        // Per-session failures are unpriced pairs; an unknown current control still blocks a price.
        const paired = pairedSavings(pairs, skippedLines(scan, profile.id));
        const savingsUsd =
          control.comparisonUnknown || !paired.decided ? null : paired.usd;
        const coverage = { priced: paired.priced, total: paired.total };
        return {
          ...point,
          baselineComparisonCost,
          comparisonCost,
          savingsUsd,
          coverage,
        };
      };
      const scenarios: WindowScenario[] = windows.map((window) => {
        const point = run(window, false, false);
        const larger = run(window, true, false);
        const cold = run(window, false, true);
        return {
          window,
          cost: point.cost,
          baselineComparisonCost: point.baselineComparisonCost,
          comparisonCost: point.comparisonCost,
          savingsUsd: point.savingsUsd,
          coverage: point.coverage,
          added: point.simulation.added,
          affectedRequests: point.simulation.affected.size,
          cacheEvidence: point.after.flatMap((i) =>
            i.evidence ? [i.evidence] : [],
          ),
          sensitivity: {
            largerSummarySavingsUsd: larger.savingsUsd,
            coldSummarySavingsUsd: cold.savingsUsd,
            rereadBreakEvenUsd:
              point.savingsUsd === null ? null : Math.max(0, point.savingsUsd),
          },
        };
      });
      const priced = scenarios.filter((s) => s.savingsUsd !== null);
      const best = priced.reduce<WindowScenario | null>(
        (best, s) =>
          best === null ||
          s.savingsUsd! > best.savingsUsd! ||
          (s.savingsUsd === best.savingsUsd && s.window === control.value)
            ? s
            : best,
        null,
      );
      const winning = best && best.savingsUsd! > 0 ? best : null;
      const recommended =
        winning?.window ??
        (priced.length === scenarios.length && control.value !== null
          ? control.value
          : null);
      const recent = relevant.at(-1)!.request;
      const version = recent.version?.split(".").map(Number);
      const perModelSupported =
        profile.harness !== "claude" ||
        (!!version &&
          version.length === 3 &&
          version.every((v) => Number.isSafeInteger(v) && v >= 0) &&
          (version[0]! > 2 ||
            (version[0] === 2 &&
              (version[1]! > 1 ||
                (version[1] === 1 &&
                  version[2]! >= (control.minimumClaudePatch ?? 288))))));
      const windowKey =
        profile.harness === "claude"
          ? "autoCompactWindow"
          : "model_auto_compact_token_limit";
      const historicalConflict = events.some(
        (e) =>
          e.kind === "settings" &&
          e.time >= since &&
          typeof e.settings[windowKey] === "number" &&
          e.settings[windowKey] !== control.value &&
          (e.model === null || models.includes(e.model)),
      );
      const applicable =
        control.applicable &&
        perModelSupported &&
        (profile.harness !== "claude" || !models.includes(null));
      const profiles = [...new Set(selected.flatMap((r) => r.profiles))];
      const savings = winning?.savingsUsd ?? (recommended !== null ? 0 : null);
      checks.push({
        check: "compaction-window",
        bucket: "all",
        model,
        models,
        profile: profile.id,
        historyProfiles: profiles,
        evidence: recommended === null ? "unknown" : "estimated",
        action:
          recommended !== null && applicable
            ? recommended === control.value
              ? "keep"
              : "change"
            : "advisory",
        current: control.value,
        recommended,
        owner: control.owner,
        since,
        until: now,
        windowBasis:
          options.since !== undefined
            ? "explicit --since; current controls applied to history"
            : "available selected history after the TTL comparison windows; current controls applied to history",
        baseline:
          control.value === null
            ? "recorded requests after available Claude TTL recommendations (otherwise current controls); current window unknown, advisory comparison only"
            : "current window applied to recorded requests after available Claude TTL recommendations (otherwise current controls); observed summaries retained once; savings are incremental and checks must not be summed",
        requests: selected.filter((r) => r.time >= since).length,
        baselineCost,
        observedSummaryCost,
        ttls,
        scenarios,
        usageEstimates: requests
          .filter(
            (r) =>
              r.time >= since ||
              baseline.some((i) =>
                i.evidence?.predecessors.some((p) => p.request === r.id),
              ),
          )
          .flatMap((r) =>
            options.estimates.has(r.id) ? [options.estimates.get(r.id)!] : [],
          ),
        cacheEvidence: baseline.flatMap((i) =>
          i.evidence ? [i.evidence] : [],
        ),
        coverage:
          (winning ?? scenarios.find((s) => s.window === control.value))
            ?.coverage ?? null,
        savingsUsd: savings,
        monthlySavingsUsd:
          savings !== null && now > since
            ? (savings * 30 * 86400000) / (now - since)
            : null,
        breakEven:
          savings !== null && savings > 0
            ? `$${savings.toFixed(4)} of additional re-read or changed-work cost erases the modeled saving over this window`
            : null,
        assumptions: [
          control.reason,
          "candidate windows are the current explicit limit, provider minimum, catalog tier boundaries and median observed resumed-prefix geometry plus ordinary input; one aggregate winner is chosen for each owning setting; this is not an exhaustive optimizer over integer thresholds",
          "only earlier compactions are modeled; every observed compaction is retained once even if an earlier pass might replace it; higher windows cannot reconstruct details already lost",
          "extra summaries read the whole modeled context at ordinary input rates; output and resumed prefix use cohort medians from linked billed summaries and first subsequent requests; first resumed prefix is cold and charged in that request, never added twice",
          "first post-boundary prefix includes fixed instructions and retained/new context, not just summary output; counts do not establish content identity; summaries may use the recorded summary model rather than the conversation model",
          "ordinary requests, outputs, tools and incremental growth are held fixed until each observed boundary; proportional quality or behavioral improvements are not assumed",
          "available TTL recommendations form the baseline; where no priced TTL recommendation exists, the current explicit control is used and unknown affected TTL charges remain unpriced",
          "unknown unchanged costs cancel only in the comparison; full totals remain unknown; affected context tiers reprice the entire request including output",
          "changed prefixes are fully reused only within the chosen Claude lifetime or the documented default 30m billed Codex lifetime, with stable prefix identity assumed; expiry makes the new prefix cold and cannot inherit external warming of the old transcript",
          "cold-summary sensitivity charges 1h Claude writes or 12h billed Codex writes; larger-summary sensitivity uses observed cohort maxima; neither sensitivity vetoes the aggregate estimate",
          "shared history is conditional for this current profile; model/check scenarios overlap and their savings must not be added",
        ],
        uncertainty: [
          ...new Set([
            "compaction loses detail; re-reading files, repeated tool calls or changed reasoning may erase savings; that work is unmeasured, not priced at zero",
            "observed boundary timestamps, precomputed flags and durations do not prove summary generation time or idle attribution; unlogged/discarded summaries remain unknown",
            "compaction trigger timing and replacement-prefix identity are estimated from token counts, not observed counterfactual behavior; no samples means affected candidates remain unpriced",
            ...(!control.applicable ? [control.reason] : []),
            ...(!perModelSupported
              ? [
                  `current client support for this window control is not established (requires Claude v2.1.${control.minimumClaudePatch ?? 288})`,
                ]
              : []),
            ...(historicalConflict
              ? [
                  "historical recorded limits differ from current files; this replay applies current controls rather than inferring historical ownership",
                ]
              : []),
            ...(profiles.length > 1
              ? ["shared history has ambiguous profile attribution"]
              : []),
            ...scenarios.flatMap((s) => [
              ...s.comparisonCost.unknown,
              ...s.cost.unknown,
            ]),
            ...selected.flatMap((r) => r.issues),
          ]),
        ],
        sources: [...SOURCES, ...PRICE_SOURCES],
      });
    }
  }
  return checks;
}

/** Unreadable lines in a profile's transcripts; each could have been a request. */
export function skippedLines(scan: Scan, profile: string): number {
  return scan.snapshots
    .filter((s) => s.profiles.includes(profile))
    .reduce((n, s) => n + s.skippedLines, 0);
}
