import type { ExposureScan, analyzeExposure } from "./exposure.js";
import type {
  BackgroundControls,
  BackgroundCheckResult,
} from "./background.js";

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
/** A current window control, without inventing a tuned model default. */
export interface WindowControl {
  value: number | null;
  owner: string;
  reason: string;
  applicable: boolean;
  minimumClaudePatch?: number;
  comparisonUnknown?: string;
}
/** Current native idle controls, with ownership and unresolved scopes kept explicit. */
export interface IdleControl {
  value: boolean | null;
  owner: string;
  threshold: number | null;
  thresholdOwner: string;
  autoCompact: boolean | null;
  reason: string;
  resolved: boolean;
  ttl?: Profile["ttl"];
  compaction?: Profile["compaction"];
}
/** A selected profile's current settings and their owning controls. */
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
  compaction: {
    default: WindowControl;
    models: Record<string, WindowControl>;
  };
  idle?: IdleControl;
  /** Current project controls observed at recorded working directories. */
  idleDirectories?: Record<string, IdleControl>;
  /** Native background controls; historical activation is never inferred from these. */
  background?: BackgroundControls;
  /** Current controls at recorded project directories, with unresolved scopes explicit. */
  backgroundDirectories?: Record<string, BackgroundControls>;
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
/** Shared recommendation metadata; savings are relative to the named baseline. */
export interface CheckBase<Value> {
  profile: string;
  historyProfiles: string[];
  evidence: Evidence;
  action: "change" | "keep" | "advisory";
  current: Value | null;
  recommended: Value | null;
  owner: string;
  since: number | null;
  until: number | null;
  windowBasis: string;
  baseline: string;
  /** Missing calls and cache effects that can change this replay's baseline. */
  baselineCoverage?: string[];
  requests: number;
  savingsUsd: number | null;
  monthlySavingsUsd: number | null;
  breakEven: string | null;
  assumptions: string[];
  uncertainty: string[];
  sources: string[];
}
/** A TTL comparison preserves cache and fixed costs separately. */
export interface TtlCheckResult extends CheckBase<Ttl> {
  check: "claude-cache-ttl";
  windowBasisKind: "explicit" | "ttl-transition" | "available";
  bucket: "main" | "subagent";
  cost: Record<Ttl, Cost>;
  summaryCost: Record<Ttl, Cost>;
  /** Cache charges used to compare TTLs; ordinary input/output are fixed. */
  comparisonCost: Record<Ttl, Cost>;
  /** Ordinary input, output, server fees and proxies held identical in both scenarios. */
  fixedCost: Cost;
  usageEstimates: UsageEstimate[];
  cacheEvidence: CacheReplayEvidence[];
  sensitivity: {
    coldPrefix1hUsd: number | null;
    coldPrefix1hComparisonUsd: number | null;
    warmReuseBreakEvenFraction: number | null;
    unlinked: {
      noBilledAttempt: Record<Ttl, Cost>;
      coldCache: Record<Ttl, Cost>;
    } | null;
  };
}
/** One extra modeled compaction and the observations supporting its geometry. */
export interface AddedCompaction {
  request: string;
  inputTokens: number;
  summaryTokens: number;
  prefixTokens: number;
  basis: { event: string; request: string }[];
  summaryCost: Cost;
  prefixCost: Cost;
}
/** One candidate window, net of added summaries and subsequent prefix charges. */
export interface WindowScenario {
  window: number;
  cost: Cost;
  baselineComparisonCost: Cost;
  comparisonCost: Cost;
  savingsUsd: number | null;
  added: AddedCompaction[];
  affectedRequests: number;
  cacheEvidence: CacheReplayEvidence[];
  sensitivity: {
    largerSummarySavingsUsd: number | null;
    coldSummarySavingsUsd: number | null;
    rereadBreakEvenUsd: number | null;
  };
}
/** A compaction check runs after the TTL choices and retains observed boundaries. */
export interface CompactionCheckResult extends CheckBase<number> {
  check: "compaction-window";
  bucket: "all";
  model: string | null;
  models: (string | null)[];
  baselineCost: Cost;
  observedSummaryCost: Cost;
  ttls: Record<"main" | "subagent", Ttl | null>;
  scenarios: WindowScenario[];
  usageEstimates: UsageEstimate[];
  cacheEvidence: CacheReplayEvidence[];
}
/** Setting-specific evidence accompanies one common recommendation model. */
export type CheckResult =
  | TtlCheckResult
  | CompactionCheckResult
  | IdleCheckResult
  | BackgroundCheckResult;
/** One request gap or final observation period, without an assumed session end. */
export interface IdlePeriod {
  request: string;
  resume: string | null;
  session: string;
  contextTokens: number | null;
  gapMs: number;
  status: "eligible" | "ineligible" | "unknown" | "censored";
  reason: string;
  confirmed: boolean;
  returnTiming: "before-expiry" | "after-expiry" | "near-expiry" | "final";
  outcome: "applied" | "not-applied" | "unobserved";
  event: string | null;
  version: string | null;
  client: string | null;
  flagState: string | null;
  activation: number | null;
  activationBasis: string;
  summaryCost: Cost;
  calibration?: {
    events: string[];
    summaryTokens: number;
    prefixTokens: number;
  };
}
/** An aggregate idle policy comparison at one named TTL/window configuration. */
export interface IdleScenario {
  baseline: string;
  applicable: boolean;
  ttls: Record<"main" | "subagent", Ttl | null>;
  windows: { owner: string; models: (string | null)[]; value: number | null }[];
  periods: IdlePeriod[];
  cost: Record<"on" | "off", Cost>;
  /** Only changed charges, relative to the shared recorded-request baseline. */
  adjustment: Record<"on" | "off", Cost>;
  deltaUsd: number | null;
  monthlyDeltaUsd: number | null;
  byContext: {
    tokens: number | null;
    periods: number;
    deltaUsd: number | null;
  }[];
  activationCohorts: {
    version: string;
    client: string;
    flagState: string;
    applied: number;
    eligible: number;
    fraction: number;
  }[];
  sensitivity: {
    closedDeltaUsd: number | null;
    openDeltaUsd: number | null;
    largerSummaryDeltaUsd: number | null;
    lateTriggerDeltaUsd: number | null;
    lifetimeBreakEvenFraction: number | null;
    rereadBreakEvenUsdPerResumedSummary: number | null;
  };
  unknown: string[];
}
/** Idle on/off runs after the TTL and window decisions, with a current-policy comparison. */
export interface IdleCheckResult extends CheckBase<boolean> {
  check: "idle-compaction";
  bucket: "main";
  threshold: number | null;
  currentSettings: IdleScenario;
  afterRecommendations: IdleScenario;
  usageEstimates: UsageEstimate[];
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
  /** Historical component counts and invocation evidence, without content. */
  exposure: ExposureScan;
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
/** Stable versioned machine output, independent of terminal presentation. */
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
  exposure: ReturnType<typeof analyzeExposure>;
  checks: CheckResult[];
  assumptions: string[];
}
