/** The two locally supported transcript formats. */
export type Harness = "claude" | "codex";
/** A documented Claude cache lifetime. */
export type Ttl = "5m" | "1h";
/** Evidence for a result, separate from whether it recommends a change. */
export type Evidence = "measured" | "estimated" | "guidance" | "unknown";
/** A safe source location without transcript content. */
export interface Location {
  file: string;
  line: number;
}
/** Normalized counts: reads and writes are subsets of input, reasoning of output. */
export interface Usage {
  input: number | null;
  output: number | null;
  read: number | null;
  write: number | null;
  write5m: number | null;
  write1h: number | null;
  write30m: number | null;
  write12h: number | null;
  reasoning: number | null;
}
/** One non-overlapping billed sampling iteration. */
export interface BillingPart {
  kind: "message" | "compaction" | "advisor" | "unknown";
  model: string | null;
  usage: Usage;
  tier: string | null;
  speed: string | null;
  geo: string | null;
}
/** One deduplicated model request, with no message content. */
export interface Request {
  evidence: "measured" | "estimated" | "unknown";
  id: string;
  harness: Harness;
  session: string;
  agent: "main" | "subagent" | "unknown";
  time: number;
  endTime: number;
  model: string | null;
  provider: string | null;
  api: "responses" | "messages" | "unknown";
  source: string | null;
  trigger: string | null;
  reference: string | null;
  turn: string | null;
  version: string | null;
  client: string | null;
  cwd: string | null;
  parts: BillingPart[];
  serverTools: Record<string, number> | null;
  profiles: string[];
  locations: Location[];
  accounting: "individual" | "legacy-last" | "legacy-delta";
  /** Recorded API-attempt budget; this does not establish execution or billing. */
  attempt: { maxOutputTokens: number | null } | null;
  issues: string[];
}
/** Structural context and timing evidence, never a billed boundary request. */
export interface StructuralEvent {
  id: string;
  /** All observed conversation aliases and safe locations for a copied event. */
  origins: { session: string; location: Location }[];
  harness: Harness;
  session: string;
  time: number;
  kind:
    | "compaction"
    | "turn-start"
    | "turn-end"
    | "tool-start"
    | "tool-end"
    | "settings"
    | "client"
    | "idle-notice";
  profiles: string[];
  location: Location;
  request: string | null;
  model: string | null;
  version: string | null;
  client: string | null;
  cwd: string | null;
  trigger: string | null;
  precomputed: boolean | null;
  preTokens: number | null;
  postTokens: number | null;
  durationMs: number | null;
  tool: string | null;
  toolId: string | null;
  settings: Record<string, string | number | boolean>;
  idleAttribution: "unknown" | "notice";
  /** Preceding metadata from this file, retained even before the selected window. */
  pricing: {
    model: string | null;
    provider: string | null;
    tier: string | null;
    speed: string | null;
    geo: string | null;
  } | null;
}
/** A selected profile's current user settings and their effective TTL controls. */
export interface Profile {
  id: string;
  harness: Harness;
  root: string;
  aliases: string[];
  histories: string[];
  settingsFile: string;
  ttl: Record<
    "main" | "subagent",
    { value: Ttl | null; owner: string; reason: string }
  >;
  provider: string | null;
  settings: Record<string, string | number | boolean>;
}
/** Discovery includes exclusions and failures so selection cannot imply full-host coverage. */
export interface RootStatus {
  harness: Harness;
  path: string;
  status: "selected" | "excluded" | "missing" | "inaccessible";
  origin: "default" | "environment" | "discovered" | "explicit";
}
/** File identity and byte boundary frozen before any transcript is parsed. */
export interface Snapshot {
  file: string;
  harness: Harness;
  profiles: string[];
  bytes: number;
  device: number;
  inode: number;
  deferredBytes: number;
  lines: number;
}
/** A price may be partially known without pretending the known subtotal is the total. */
export interface Cost {
  knownUsd: number;
  totalUsd: number | null;
  unknown: string[];
  assumptions: string[];
}
/** An observed boundary with either linked usage or one explicit summary estimate. */
export interface CompactionCharge {
  event: string;
  request: string | null;
  evidence: Evidence;
  cost: Cost;
  ttlCost: Record<Ttl, Cost>;
  assumptions: string[];
}
/** A comparison-only input estimate, with original measured coverage left untouched. */
export interface UsageEstimate {
  request: string;
  evidence: "estimated" | "unknown";
  basis: string[];
  part: BillingPart | null;
  maxOutputTokens: number | null;
  reason: string;
  assumptions: string[];
}
/** Cache-history witnesses distinguish measured prefixes from estimates and unresolved cohorts. */
export interface CachePredecessor {
  request: string;
  part: number;
  time: number;
  state: "known" | "estimated" | "unknown";
  prefixEvidence: "measured" | "estimated" | "unknown";
  cohortKnown: boolean;
  reason: string;
}
/** Evidence for one selected request's counterfactual cache geometry, independent of prices. */
export interface CacheReplayEvidence {
  request: string;
  part: number;
  ttl: Ttl;
  state: "known" | "estimated" | "unknown";
  predecessors: CachePredecessor[];
  outcome: "invariant" | "estimated" | "unknown";
  reason: string;
}
/** A check compares one setting against its named baseline, without additive savings. */
export interface CheckResult {
  check: string;
  profile: string;
  historyProfiles: string[];
  bucket: "main" | "subagent";
  evidence: Evidence;
  action: "change" | "keep" | "advisory";
  current: Ttl | null;
  recommended: Ttl | null;
  owner: string;
  since: number | null;
  until: number | null;
  windowBasis: string;
  baseline: string;
  requests: number;
  cost: Record<Ttl, Cost>;
  summaryCost: Record<Ttl, Cost>;
  /** Cache charges used to compare TTLs; ordinary input/output are fixed. */
  comparisonCost: Record<Ttl, Cost>;
  /** Ordinary input, output, server fees and proxies held identical in both scenarios. */
  fixedCost: Cost;
  usageEstimates: UsageEstimate[];
  cacheEvidence: CacheReplayEvidence[];
  savingsUsd: number | null;
  monthlySavingsUsd: number | null;
  breakEven: string | null;
  sensitivity: {
    coldPrefix1hUsd: number | null;
    coldPrefix1hComparisonUsd: number | null;
    warmReuseBreakEvenFraction: number | null;
    unlinked: {
      noBilledAttempt: Record<Ttl, Cost>;
      coldCache: Record<Ttl, Cost>;
    } | null;
  };
  assumptions: string[];
  uncertainty: string[];
  sources: string[];
}
/** The reusable parser output for downstream checks. */
export interface Scan {
  snapshotStartedAt: string;
  snapshotFinishedAt: string;
  /** Explicit billing selection bound, inherited by checks and never expanded. */
  since: number | null;
  roots: RootStatus[];
  profiles: Profile[];
  snapshots: Snapshot[];
  requests: Request[];
  events: StructuralEvent[];
  /** Pre-window Claude metadata supplies cache evidence, never selected billing or coverage. */
  cacheContext: { requests: Request[]; boundaries: StructuralEvent[] };
  coverage: {
    requests: number;
    sourceKnown: number;
    tierKnown: number;
    missingUsage: number;
    deferredFiles: number;
    issues: string[];
  };
}
/** Versioned machine output for the minimal report and later presentation work. */
export interface Report {
  schemaVersion: 1;
  generatedAt: string;
  scan: Scan;
  prices: { source: string; fetchedAt: string | null; offline: boolean };
  historical: {
    bucket: string;
    profiles: string[];
    requests: number;
    evidence: "estimated";
    cost: Cost;
  }[];
  compactions: CompactionCharge[];
  checks: CheckResult[];
  assumptions: string[];
}
