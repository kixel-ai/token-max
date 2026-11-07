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
  issues: string[];
}
/** Structural context and timing evidence, never a billed boundary request. */
export interface StructuralEvent {
  id: string;
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
  savingsUsd: number | null;
  monthlySavingsUsd: number | null;
  breakEven: string | null;
  sensitivity: {
    coldPrefix1hUsd: number | null;
    warmReuseBreakEvenFraction: number | null;
  };
  assumptions: string[];
  uncertainty: string[];
  sources: string[];
}
/** The reusable parser output for downstream checks. */
export interface Scan {
  snapshotStartedAt: string;
  snapshotFinishedAt: string;
  roots: RootStatus[];
  profiles: Profile[];
  snapshots: Snapshot[];
  requests: Request[];
  events: StructuralEvent[];
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
