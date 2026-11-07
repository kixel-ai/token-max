import type {
  BillingPart,
  CachePredecessor,
  CacheReplayEvidence,
  Request,
  StructuralEvent,
  Ttl,
} from "./model.js";

const MINUTE = 60_000;
type Cohort = readonly (string | null)[];
interface StateBase {
  cohort: Cohort;
  origin: CachePredecessor;
  order: number;
}
type CacheState =
  | (StateBase & { kind: "known" | "estimated"; prefix: number })
  | (StateBase & { kind: "unknown"; prefix: number | null });
type Range = { min: CacheState; max: CacheState };

function merge(a: Range | null, b: Range | null): Range | null {
  if (!a) return b;
  if (!b) return a;
  return {
    min:
      (a.min.prefix ?? -Infinity) <= (b.min.prefix ?? -Infinity)
        ? a.min
        : b.min,
    max:
      (a.max.prefix ?? Infinity) >= (b.max.prefix ?? Infinity) ? a.max : b.max,
  };
}

// Prefix conversion is monotone, so range extrema suffice to prove invariance.
// The growing tree keeps missing-cohort histories from requiring quadratic scans.
class History {
  states: CacheState[] = [];
  anchor: CacheState | undefined;
  private capacity = 1;
  private tree: (Range | null)[] = [null, null];
  constructor(readonly cohort: Cohort) {}

  append(state: CacheState) {
    if (state.kind !== "unknown") {
      this.states = [];
      this.capacity = 1;
      this.tree = [null, null];
      this.anchor = state;
    }
    this.states.push(state);
    if (this.states.length > this.capacity) {
      this.capacity *= 2;
      this.tree = Array<Range | null>(this.capacity * 2).fill(null);
      for (let i = 0; i < this.states.length; i++)
        this.tree[this.capacity + i] = {
          min: this.states[i]!,
          max: this.states[i]!,
        };
      for (let i = this.capacity - 1; i > 0; i--)
        this.tree[i] = merge(this.tree[i * 2]!, this.tree[i * 2 + 1]!);
    } else {
      let index = this.capacity + this.states.length - 1;
      this.tree[index] = { min: state, max: state };
      while ((index = Math.floor(index / 2)) > 0)
        this.tree[index] = merge(
          this.tree[index * 2]!,
          this.tree[index * 2 + 1]!,
        );
    }
  }

  private after(value: number, field: "time" | "order") {
    let low = 0;
    let high = this.states.length;
    while (low < high) {
      const middle = Math.floor((low + high) / 2);
      const state = this.states[middle]!;
      if ((field === "time" ? state.origin.time : state.order) <= value)
        low = middle + 1;
      else high = middle;
    }
    return low;
  }

  candidates(order: number, time: number): CacheState[] {
    const start = this.after(order, "order");
    const expired = Math.max(start, this.after(time - 60 * MINUTE, "time"));
    const recent = Math.max(start, this.after(time - 5 * MINUTE, "time"));
    const result: CacheState[] = [];
    if (expired > start) result.push(this.states[expired - 1]!);
    if (recent < this.states.length) result.push(this.states.at(-1)!);
    let left = expired + this.capacity;
    let right = recent + this.capacity;
    let range: Range | null = null;
    while (left < right) {
      if (left % 2) range = merge(range, this.tree[left++]!);
      if (right % 2) range = merge(range, this.tree[--right]!);
      left = Math.floor(left / 2);
      right = Math.floor(right / 2);
    }
    if (range) result.push(range.min, range.max);
    return result;
  }
}

/** Recognize a recorded, unambiguous write lifetime without inventing a missing split. */
export function observedTtl(p: BillingPart): Ttl | null {
  if ((p.usage.write5m ?? 0) > 0 && p.usage.write1h === 0) return "5m";
  if ((p.usage.write1h ?? 0) > 0 && p.usage.write5m === 0) return "1h";
  return null;
}

function apply(options: {
  p: BillingPart;
  ttl: Ttl;
  previous: { time: number; prefix: number } | undefined;
  time: number;
  cold: boolean;
}): BillingPart {
  const { p, ttl, previous, time, cold } = options;
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
        Math.max(0, previous!.prefix - u.read),
      );
      u.write -= reusable;
      u.read += reusable;
    }
    if (ttl === "1h" && cold) {
      u.write += u.read;
      u.read = 0;
    }
  }
  u.write5m = ttl === "5m" ? u.write : 0;
  u.write1h = ttl === "1h" ? u.write : 0;
  return { ...p, usage: u };
}

function compatible(a: Cohort, b: Cohort): boolean {
  return a.every(
    (value, index) => value === null || b[index] === null || value === b[index],
  );
}

/** Replay explicit cache states, preserving unknown histories across cohorts and windows. */
export class CacheChain {
  private readonly sessions = new Map<string, Map<string, History>>();
  private readonly boundaries = new Map<string, number[]>();
  private readonly cursors = new Map<string, number>();
  private order = 0;

  constructor(events: StructuralEvent[]) {
    for (const event of events) {
      if (event.harness !== "claude" || event.kind !== "compaction") continue;
      for (const origin of event.origins) {
        const times = this.boundaries.get(origin.session) ?? [];
        times.push(event.time);
        this.boundaries.set(origin.session, times);
      }
    }
    for (const times of this.boundaries.values()) times.sort((a, b) => a - b);
  }

  replay(options: {
    request: Request;
    p: BillingPart;
    part: number;
    estimated: boolean;
    ttl: Ttl;
    cold: boolean;
    selected: boolean;
  }): {
    p: BillingPart;
    unknown: boolean;
    evidence: CacheReplayEvidence | null;
  } | null {
    const { request: r, p, ttl } = options;
    const histories =
      this.sessions.get(r.session) ?? new Map<string, History>();
    const times = this.boundaries.get(r.session) ?? [];
    let cursor = this.cursors.get(r.session) ?? 0;
    while (cursor < times.length && times[cursor]! <= r.time) {
      histories.clear();
      cursor++;
    }
    this.cursors.set(r.session, cursor);
    this.sessions.set(r.session, histories);
    const cohort: Cohort = [
      p.model,
      r.provider,
      p.speed,
      p.kind === "unknown" ? null : p.kind,
      r.agent === "unknown" ? null : r.agent,
    ];
    const key = JSON.stringify(cohort);
    const complete = cohort.every((value) => value !== null);
    const anchor = complete ? histories.get(key)?.anchor : undefined;
    const candidates = new Map<number, CacheState>();
    if (anchor) candidates.set(anchor.order, anchor);
    if (options.selected)
      for (const history of histories.values()) {
        if (!compatible(cohort, history.cohort)) continue;
        // A fully specified cohort supersedes its older alternatives. An omitted
        // dimension cannot supersede another cohort's evidence.
        const floor = anchor ?? history.anchor;
        if (!anchor && history.anchor)
          candidates.set(history.anchor.order, history.anchor);
        for (const state of history.candidates(floor?.order ?? -1, r.time))
          candidates.set(state.order, state);
      }
    let result: ReturnType<CacheChain["replay"]> = null;
    if (options.selected) {
      const states = [...candidates.values()];
      const versions: BillingPart[] = [];
      const replay = (previous: { time: number; prefix: number } | undefined) =>
        apply({ p, ttl, previous, time: r.time, cold: options.cold });
      if (!anchor) versions.push(replay(undefined));
      for (const state of states)
        for (const prefix of state.prefix === null
          ? [0, (p.usage.read ?? 0) + (p.usage.write ?? 0)]
          : [state.prefix])
          versions.push(replay({ time: state.origin.time, prefix }));
      const first = versions[0]!;
      const unknown =
        p.usage.read === null ||
        p.usage.write === null ||
        versions.some(
          (v) =>
            v.usage.read !== first.usage.read ||
            v.usage.write !== first.usage.write,
        );
      const state =
        !anchor || states.length !== 1 || states[0]!.kind === "unknown"
          ? "unknown"
          : states[0]!.kind;
      const reason =
        p.usage.read === null || p.usage.write === null
          ? "request cache usage is not recorded"
          : unknown
            ? "cache predecessor evidence cannot establish TTL-dependent reads/writes"
            : state === "unknown"
              ? "cache geometry is invariant across the possible recorded predecessor histories"
              : state === "estimated"
                ? "cache geometry uses an estimated predecessor prefix under the fixed-behavior replay"
                : "cache geometry uses the recorded predecessor prefix under the fixed-behavior replay";
      result = {
        p: unknown ? replay(undefined) : first,
        unknown,
        evidence:
          state === "known" && !unknown
            ? null
            : states.length || unknown
              ? {
                  request: r.id,
                  part: options.part,
                  ttl,
                  state,
                  predecessors: states.map((s) => s.origin),
                  outcome: unknown
                    ? "unknown"
                    : state === "estimated"
                      ? "estimated"
                      : "invariant",
                  reason,
                }
              : null,
      };
    }
    const prefix =
      p.usage.read === null || p.usage.write === null
        ? null
        : p.usage.read + p.usage.write;
    // A recorded zero-cache request cannot renew or erase an older cached prefix.
    if (prefix !== 0) {
      const kind =
        complete && prefix !== null
          ? options.estimated
            ? "estimated"
            : "known"
          : "unknown";
      const prefixEvidence =
        prefix === null
          ? "unknown"
          : options.estimated
            ? "estimated"
            : "measured";
      const reason = [
        prefixEvidence === "unknown"
          ? "request cache usage/execution is not established"
          : prefixEvidence === "estimated"
            ? "cache prefix estimated from measured request brackets"
            : "recorded cache prefix",
        ...(!complete
          ? ["cache cohort has unrecorded model/provider/speed/kind/agent"]
          : []),
      ].join("; ");
      const origin: CachePredecessor = {
        request: r.id,
        part: options.part,
        time: r.time,
        state: kind,
        prefixEvidence,
        cohortKnown: complete,
        reason,
      };
      const base = { cohort, origin, order: this.order++ };
      const state: CacheState =
        kind === "unknown"
          ? { ...base, kind, prefix }
          : { ...base, kind, prefix: prefix! };
      const history = histories.get(key) ?? new History(cohort);
      history.append(state);
      histories.set(key, history);
    }
    return result;
  }
}
