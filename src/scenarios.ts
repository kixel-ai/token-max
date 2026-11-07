import type {
  BillingPart,
  CacheReplayEvidence,
  Cost,
  Request,
  StructuralEvent,
  Ttl,
  UsageEstimate,
} from "./model.js";
import { CacheChain } from "./cache.js";
import { priceComponents, unknownCost, type Catalog } from "./prices.js";

/** Cache policy bucket for a billed iteration, preserving unresolved attribution. */
export function cacheBucket(
  r: Request,
  p: BillingPart,
): "main" | "subagent" | "unknown" {
  if (r.agent === "subagent") return "subagent";
  if (p.kind === "unknown") return "unknown";
  if (p.kind === "compaction" || p.kind === "advisor") return "subagent";
  return r.agent;
}
/** One cache replay result; pre-window parts inform evidence without becoming billed. */
export interface ReplayedPart {
  request: Request;
  part: number;
  p: BillingPart;
  selected: boolean;
  components: { cache: Cost; ordinary: Cost; output: Cost };
  evidence: CacheReplayEvidence | null;
}
/** Replay the same cache histories and usage estimates for every setting consumer. */
export function replayParts(options: {
  requests: Request[];
  events: StructuralEvent[];
  estimates: Map<string, UsageEstimate>;
  catalog: Catalog;
  ttls: Record<"main" | "subagent" | "unknown", Ttl | null>;
  since: number;
  coldPrefixes?: boolean;
  attemptScenario?: "point" | "none" | "cold";
}): ReplayedPart[] {
  const chain = new CacheChain(options.events);
  const result: ReplayedPart[] = [];
  for (const r of options.requests) {
    const estimate = options.estimates.get(r.id);
    if (estimate?.part && options.attemptScenario === "none") continue;
    for (const [part, p] of (estimate?.part
      ? [estimate.part]
      : r.parts
    ).entries()) {
      const bucket = cacheBucket(r, p);
      const ttl = options.ttls[bucket];
      const selected = r.time >= options.since;
      const replayed =
        r.harness === "claude"
          ? chain.replay({
              request: r,
              p,
              part,
              estimated: !!estimate?.part || r.evidence === "estimated",
              ttl: ttl ?? "5m",
              cold: options.coldPrefixes ?? false,
              selected,
            })
          : { p, unknown: false, evidence: null };
      if (!replayed) continue;
      if (
        estimate?.part &&
        options.attemptScenario === "cold" &&
        r.harness === "claude"
      ) {
        const u = replayed.p.usage;
        u.write = u.write! + u.read!;
        u.read = 0;
        u.write5m = ttl === "5m" ? u.write : 0;
        u.write1h = ttl === "1h" ? u.write : 0;
      }
      const components = priceComponents(options.catalog, r, replayed.p);
      if (
        r.harness === "claude" &&
        (p.usage.read !== 0 || p.usage.write !== 0)
      ) {
        if (bucket === "unknown" || ttl === null)
          components.cache = unknownCost(
            bucket === "unknown"
              ? "cache TTL bucket is not established for this part"
              : "effective cache TTL policy is not established",
          );
        else if (replayed.unknown)
          components.cache = unknownCost(replayed.evidence!.reason);
      }
      result.push({
        request: r,
        part,
        p: replayed.p,
        selected,
        components,
        evidence: replayed.evidence,
      });
    }
  }
  return result;
}
