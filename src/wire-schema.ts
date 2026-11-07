import type { Schema } from "./protocol.js";
import type {
  BillingPart,
  IdleControl,
  Location,
  Profile,
  Request,
  RootStatus,
  Scan,
  Snapshot,
  SpeedControl,
  StructuralEvent,
  Usage,
  WindowControl,
} from "./model.js";
import type {
  Component,
  Exposure,
  ExposureScan,
  Invocation,
  Size,
  Visibility,
} from "./exposure.js";

// The invariant type witness makes missing fields (including optional future
// adapter fields), wrong enums and wrong element types fail the ordinary build.
type Codec<T> = Schema & { witness?: (value: T) => T; isOptional?: true };
const string: Codec<string> = { scalar: "string" };
const number: Codec<number> = { scalar: "number" };
function choices<T extends string | number | boolean>(
  ...values: T[]
): Codec<T> {
  return { union: values.map((literal) => ({ literal })) };
}
const boolean = choices(true, false);
function nullable<T>(schema: Codec<T>): Codec<T | null> {
  return { union: [schema, { literal: null }] };
}
function optional<T>(schema: Codec<T>): Codec<T | undefined> {
  return { ...(schema as Schema), isOptional: true };
}
function array<T>(schema: Codec<T>): Codec<T[]> {
  return { array: schema };
}
function record<T>(schema: Codec<T>): Codec<Record<string, T>> {
  return { properties: {}, values: schema };
}
function structure<T>(properties: { [K in keyof T]-?: Codec<T[K]> }): Codec<T> {
  return {
    properties: properties as Record<string, Schema>,
    optional: Object.entries(properties).flatMap(([key, value]) =>
      (value as Codec<unknown>).isOptional ? [key] : [],
    ),
  };
}
const scalar: Codec<string | number | boolean> = {
  union: [string, number, boolean],
};
const harness = choices("claude", "codex");
const ttl = nullable(choices("5m", "1h"));
const strings = array(string);
const location = structure<Location>({
  file: string,
  line: number,
  host: optional(string),
});
const usage = structure<Usage>({
  input: nullable(number),
  output: nullable(number),
  read: nullable(number),
  write: nullable(number),
  write5m: nullable(number),
  write1h: nullable(number),
  write30m: nullable(number),
  write12h: nullable(number),
  reasoning: nullable(number),
});
const part = structure<BillingPart>({
  kind: choices("message", "compaction", "advisor", "unknown"),
  model: nullable(string),
  usage,
  tier: nullable(string),
  speed: nullable(string),
  geo: nullable(string),
});
const request = structure<Request>({
  evidence: choices("measured", "estimated", "unknown"),
  id: string,
  identityBasis: optional(choices("recorded", "derived", "location")),
  origins: optional(
    array(
      structure<NonNullable<Request["origins"]>[number]>({
        host: string,
        session: string,
        profiles: strings,
        location,
      }),
    ),
  ),
  harness,
  session: string,
  agent: choices("main", "subagent", "unknown"),
  time: number,
  endTime: number,
  model: nullable(string),
  provider: nullable(string),
  api: choices("responses", "messages", "unknown"),
  source: nullable(string),
  trigger: nullable(string),
  reference: nullable(string),
  turn: nullable(string),
  version: nullable(string),
  client: nullable(string),
  cwd: nullable(string),
  parts: array(part),
  serverTools: nullable(record(number)),
  profiles: strings,
  locations: array(location),
  accounting: choices("individual", "legacy-last", "legacy-delta"),
  attempt: nullable(
    structure<NonNullable<Request["attempt"]>>({
      maxOutputTokens: nullable(number),
    }),
  ),
  requestedSpeed: optional(
    structure<NonNullable<Request["requestedSpeed"]>>({
      tier: nullable(string),
      speed: nullable(string),
    }),
  ),
  issues: strings,
});
const event = structure<StructuralEvent>({
  id: string,
  identityBasis: optional(choices("recorded", "derived")),
  origins: array(
    structure<StructuralEvent["origins"][number]>({
      session: string,
      location,
    }),
  ),
  harness,
  session: string,
  time: number,
  kind: choices(
    "compaction",
    "turn-start",
    "turn-end",
    "tool-start",
    "tool-end",
    "settings",
    "client",
    "idle-notice",
  ),
  profiles: strings,
  location,
  request: nullable(string),
  model: nullable(string),
  version: nullable(string),
  client: nullable(string),
  cwd: nullable(string),
  trigger: nullable(string),
  precomputed: nullable(boolean),
  preTokens: nullable(number),
  postTokens: nullable(number),
  durationMs: nullable(number),
  tool: nullable(string),
  toolId: nullable(string),
  settings: record(scalar),
  idleAttribution: choices("unknown", "notice"),
  pricing: nullable(
    structure<NonNullable<StructuralEvent["pricing"]>>({
      model: nullable(string),
      provider: nullable(string),
      tier: nullable(string),
      speed: nullable(string),
      geo: nullable(string),
    }),
  ),
});
const window = structure<WindowControl>({
  value: nullable(number),
  owner: string,
  reason: string,
  applicable: boolean,
  minimumClaudePatch: optional(number),
  comparisonUnknown: optional(string),
});
const compaction = structure<Profile["compaction"]>({
  default: window,
  models: record(window),
});
const ttlControl = structure<Profile["ttl"]["main"]>({
  value: ttl,
  owner: string,
  reason: string,
});
const ttls = structure<Profile["ttl"]>({
  main: ttlControl,
  subagent: ttlControl,
});
const idle = structure<IdleControl>({
  value: nullable(boolean),
  owner: string,
  threshold: nullable(number),
  thresholdOwner: string,
  autoCompact: nullable(boolean),
  reason: string,
  resolved: boolean,
  ttl: optional(ttls),
  compaction: optional(compaction),
});
const speed = structure<SpeedControl>({
  value: nullable(string),
  owner: string,
  replacement: string,
  resolved: boolean,
  reason: string,
});
const profile = structure<Profile>({
  id: string,
  host: optional(string),
  harness,
  root: string,
  aliases: strings,
  histories: strings,
  settingsFile: string,
  ttl: ttls,
  provider: nullable(string),
  settings: record(scalar),
  compaction,
  idle: optional(idle),
  speed: optional(speed),
  speedDirectories: optional(record(speed)),
  idleDirectories: optional(record(idle)),
});
const root = structure<RootStatus>({
  harness,
  host: optional(string),
  path: string,
  status: choices("selected", "excluded", "missing", "inaccessible"),
  origin: choices("default", "environment", "discovered", "explicit"),
});
const snapshot = structure<Snapshot>({
  file: string,
  host: optional(string),
  harness,
  profiles: strings,
  bytes: number,
  device: number,
  inode: number,
  deferredBytes: number,
  lines: number,
});
const size = {
  characters: nullable(number),
  recordedTokens: nullable(number),
  fingerprint: string,
} satisfies { [K in keyof Size]: Codec<Size[K]> };
const component = structure<Component>({
  ...size,
  id: string,
  key: string,
  category: choices(
    "skill-listing",
    "skill-body",
    "mcp-discovery",
    "mcp-schema",
    "mcp-result",
    "instructions",
    "agents",
    "hooks",
    "plugin-instructions",
    "app-instructions",
    "managed-instructions",
    "other",
  ),
  owner: choices("skills", "context"),
  group: nullable(string),
  provenance: string,
  locations: array(location),
  firstSeen: number,
  current: choices("unresolved"),
  scope: choices("component", "aggregate"),
  members: strings,
  targets: strings,
});
const invocation = structure<Invocation>({
  id: string,
  target: string,
  group: nullable(string),
  kind: choices(
    "skill-request",
    "skill-reference",
    "skill-call",
    "skill-load",
    "mcp-call",
    "mcp-resource",
    "mcp-prompt",
    "script-reference",
  ),
  status: choices("requested", "called", "completed", "failed", "reference"),
  via: choices("typed", "mention", "wrapper", "tool", "completion", "script"),
  session: string,
  sessions: strings,
  identityBasis: choices("recorded", "content-occurrence"),
  harness,
  client: nullable(string),
  version: nullable(string),
  time: number,
  profiles: strings,
  locations: array(location),
});
const exposure = structure<Exposure>({
  request: string,
  components: strings,
  candidates: strings,
  removed: strings,
  inherited: strings,
  deferred: strings,
  linkage: choices("wire", "chronological", "unknown"),
  coverage: choices("complete-text", "partial", "unknown"),
  references: nullable(
    structure<NonNullable<Exposure["references"]>>({
      base: nullable(string),
      keep: nullable(number),
      tail: strings,
      items: nullable(strings),
    }),
  ),
  completeCharacters: nullable(number),
  wireModel: nullable(string),
  reasons: strings,
});
const visibility = structure<Visibility>({
  harness,
  client: nullable(string),
  version: nullable(string),
  profiles: strings,
  component: nullable(string),
  since: number,
  until: number,
  exposure: choices("partial", "unknown"),
  directCalls: choices("observed", "unknown"),
  nestedCalls: choices("partial", "unknown"),
  typedSkills: choices("observed", "unknown"),
  automaticUnused: choices(false),
  reasons: strings,
});
const exposureScan = structure<ExposureScan>({
  components: array(component),
  requests: array(exposure),
  invocations: array(invocation),
  visibility: array(visibility),
});

/** The exhaustive shared Scan allowlist, checked against the analyzer's actual types. */
export const scanSchema: Codec<Scan> = structure<Scan>({
  hosts: optional(
    array(
      structure<NonNullable<Scan["hosts"]>[number]>({
        id: string,
        transport: choices("local", "ssh"),
        destination: nullable(string),
        platform: string,
        runtime: string,
        snapshotStartedAt: string,
        snapshotFinishedAt: string,
        coverage: structure<Scan["coverage"]>({
          requests: number,
          sourceKnown: number,
          tierKnown: number,
          missingUsage: number,
          deferredFiles: number,
          issues: strings,
        }),
      }),
    ),
  ),
  snapshotStartedAt: string,
  snapshotFinishedAt: string,
  since: nullable(number),
  roots: array(root),
  profiles: array(profile),
  snapshots: array(snapshot),
  requests: array(request),
  events: array(event),
  exposure: exposureScan,
  cacheContext: structure<Scan["cacheContext"]>({
    requests: array(request),
    boundaries: array(event),
  }),
  coverage: structure<Scan["coverage"]>({
    requests: number,
    sourceKnown: number,
    tierKnown: number,
    missingUsage: number,
    deferredFiles: number,
    issues: strings,
  }),
});
