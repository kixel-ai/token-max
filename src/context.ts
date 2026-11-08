import type { Request } from "./model.js";
import {
  calibrate,
  cohort,
  type Component,
  type ExposureScan,
} from "./exposure.js";
import { unitRates, type Catalog } from "./prices.js";
import { identity } from "./safe.js";

/** One settings.json change: set `path` to `value`, or add `value` to the list at `path`. */
export interface Control {
  path: string[];
  value: string | boolean;
  append: boolean;
}

/** What one piece of always-sent context cost over the window, and how to turn it off. */
export interface ContextCost {
  profiles: string[];
  kind: "tool" | "mcp-server" | "skill" | "instructions" | "system" | "other";
  name: string;
  /** The settings change that removes it, or null when no setting does. */
  control: Control | null;
  /** Selected requests that carried it, and how many of those could be priced. */
  requests: number;
  priced: number;
  /** Tokens it took in the most recent request that carried it. */
  tokens: number | null;
  /** API-price equivalent over the priced requests. */
  costUsd: number;
  /** Observed uses in the window; null when uses are not observable. Skills count only model calls, since typing one still works when it is hidden. */
  calls: number | null;
  lastUsed: number | null;
  /** Whether the profile's most recent request still carried it. */
  current: boolean;
  /** Cost divided by uses, counting no use as one; null without a use count. */
  costPerUseUsd: number | null;
  /** "off" when it cost more per use than the limit; null when it cannot be switched off today or was never priced. */
  verdict: "off" | "keep" | null;
}

/** The settings.json fragment for one control, such as `"enableArtifact": false`. */
export function controlText(control: Control): string {
  let value: unknown = control.append ? [control.value] : control.value;
  for (const key of control.path.slice(1).reverse()) value = { [key]: value };
  return `${json(control.path[0])}: ${JSON.stringify(value, null, 1).replace(/\n\s*/g, " ").replace(/\[ /g, "[").replace(/ \]/g, "]").replace(/\{ /g, "{").replace(/ \}/g, "}")}`;
}

/** Merge controls into one settings.json object, adding to lists rather than replacing them. */
export function settingsPatch(controls: Control[]): Record<string, unknown> {
  const root: Record<string, unknown> = {};
  for (const c of controls) {
    let node = root;
    for (const key of c.path.slice(0, -1))
      node = (node[key] ??= {}) as Record<string, unknown>;
    const last = c.path[c.path.length - 1]!;
    if (!c.append) node[last] = c.value;
    else {
      const list = (node[last] ??= []) as unknown[];
      if (!list.includes(c.value)) list.push(c.value);
    }
  }
  return root;
}

interface Piece {
  key: string;
  kind: ContextCost["kind"];
  name: string;
  control: Control | null;
  characters: number;
}

const json = (value: unknown) => JSON.stringify(value);
// Claude Code cannot do its basic work without these, so they have no off switch here.
const CORE_TOOLS = new Set([
  "Bash",
  "Read",
  "Edit",
  "Write",
  "Skill",
  "ToolSearch",
  "DeferredToolPlaceholder",
]);

/** The setting that owns a tool, grouping a tool family under one switch. */
function tool(name: string): Omit<Piece, "characters"> {
  if (name.startsWith("mcp__")) {
    const server = name.split("__")[1] ?? name;
    return {
      key: `mcp:${server}`,
      kind: "mcp-server",
      name: server,
      control: deny(`mcp__${server}`),
    };
  }
  // The Artifact tool's helpers (ArtifactComments, ArtifactData) go with it.
  if (name.startsWith("Artifact"))
    return {
      key: "tool:Artifact",
      kind: "tool",
      name: "Artifact",
      control: { path: ["enableArtifact"], value: false, append: false },
    };
  if (name === "Workflow")
    return {
      key: "tool:Workflow",
      kind: "tool",
      name,
      control: { path: ["enableWorkflows"], value: false, append: false },
    };
  return {
    key: `tool:${name}`,
    kind: "tool",
    name,
    control: CORE_TOOLS.has(name) ? null : deny(name),
  };
}
function deny(rule: string): Control {
  return { path: ["permissions", "deny"], value: rule, append: true };
}

/** Split one component into the pieces a user can switch separately. */
function pieces(
  component: Component,
  components: Map<string, Component>,
): Piece[] {
  const characters = component.characters;
  if (characters === null) return [];
  const name = component.name;
  switch (component.category) {
    case "tool-schema":
    case "mcp-schema":
    case "mcp-discovery":
      return [{ ...tool(name), characters }];
    case "skill-listing": {
      const members = component.members
        .map((id) => components.get(id)!)
        .filter((m) => m.characters !== null);
      const listed = members.reduce((n, m) => n + m.characters!, 0);
      return [
        ...members.map((m) => ({
          key: `skill:${m.name}`,
          kind: "skill" as const,
          name: m.name,
          control: {
            path: ["skillOverrides", m.name],
            value: "user-invocable-only",
            append: false,
          },
          characters: m.characters!,
        })),
        {
          key: "other:skill-list",
          kind: "other",
          name: "Skill list framing",
          control: null,
          characters: Math.max(0, characters - listed),
        },
      ];
    }
    case "instructions":
      return [
        {
          key: `instructions:${name}`,
          kind: "instructions",
          name,
          control: null,
          characters,
        },
      ];
    case "plugin-instructions": {
      const server = name.replace(/[^A-Za-z0-9_-]/g, "_");
      return [
        {
          key: `mcp:${server}`,
          kind: "mcp-server",
          name: server,
          control: deny(`mcp__${server}`),
          characters,
        },
      ];
    }
    case "agents":
      return [
        {
          key: "other:agents",
          kind: "other",
          name: "Subagent list",
          control: null,
          characters,
        },
      ];
    case "other":
      // Other attachments are one-off conversation content, not recurring context.
      // A prompt snapshot repeats the system prompt the request shape already gives.
      return name === "wire-system"
        ? [
            {
              key: "system",
              kind: "system",
              name: "Claude Code system prompt",
              control: null,
              characters,
            },
          ]
        : [];
    case "skill-body":
    case "mcp-result":
      // Loaded on use, so they are the cost of using a feature, not of having it on.
      return [];
    default:
      return [
        {
          key: `other:${component.category}`,
          kind: "other",
          name: component.category,
          control: null,
          characters,
        },
      ];
  }
}

/**
 * Price each piece of context in every selected request that recorded its exact
 * shape. A piece is charged at the cache state of its position in the prompt
 * prefix (tools, then system, then message attachments).
 */
export function contextCosts(input: {
  scan: ExposureScan;
  requests: Request[];
  catalog: Catalog;
  /** Recommend turning a piece off when it cost more than this per use. */
  maxCostPerUseUsd: number;
}): ContextCost[] {
  const { scan, catalog } = input;
  const calibration = new Map(
    calibrate(scan, input.requests).map((c) => [c.cohort, c]),
  );
  const requests = new Map(input.requests.map((r) => [r.id, r]));
  const components = new Map(scan.components.map((c) => [c.id, c]));
  const piecesOf = new Map<string, Piece[]>();
  const rows = new Map<string, ContextCost>();
  const latest = new Map<string, { time: number; keys: Set<string> }>();
  for (const exposure of scan.requests) {
    const r = requests.get(exposure.request);
    if (!r || exposure.linkage !== "wire" || r.harness !== "claude") continue;
    const profiles = [...r.profiles].sort();
    const owner = json(profiles);
    const parts: Piece[] = exposure.components.flatMap((id) => {
      let cached = piecesOf.get(id);
      if (!cached) {
        cached = pieces(components.get(id)!, components);
        piecesOf.set(id, cached);
      }
      return cached;
    });
    const p = r.parts.length === 1 ? r.parts[0]! : null;
    const u = p?.usage;
    const ratio = calibration.get(cohort(r))?.tokensPerCharacter;
    const rates = p ? unitRates(catalog, r, p) : null;
    const priced =
      p &&
      u &&
      ratio !== undefined &&
      rates &&
      u.input !== null &&
      u.read !== null &&
      u.write !== null &&
      (u.write === 0 || (u.write1h !== null && u.write5m !== null));
    const total =
      ratio === undefined
        ? 0
        : parts.reduce((n, piece) => n + piece.characters * ratio, 0);
    // A calibration that overshoots the recorded input cannot place these pieces.
    const placed = priced && total <= u!.input!;
    // Cache slots in prefix order: reads, then 1h writes, then 5m writes, then ordinary input.
    const slots = placed
      ? [
          [u!.read!, rates!.read],
          [u!.write === 0 ? 0 : u!.write1h!, rates!.write1h],
          [u!.write === 0 ? 0 : u!.write5m!, rates!.write5m],
          [Infinity, rates!.ordinary],
        ]
      : [];
    let offset = 0;
    const keys = new Set<string>();
    const charged = new Map<string, { tokens: number; cost: number }>();
    for (const piece of parts) {
      const key = `${owner}:${piece.key}`;
      const first = !keys.has(key);
      keys.add(key);
      const row =
        rows.get(key) ??
        ({
          profiles,
          kind: piece.kind,
          name: piece.name,
          control: piece.control,
          requests: 0,
          priced: 0,
          tokens: null,
          costUsd: 0,
          calls: null,
          lastUsed: null,
          current: false,
          costPerUseUsd: null,
          verdict: null,
          time: -Infinity,
        } as ContextCost & { time: number });
      rows.set(key, row);
      if (first) row.requests++;
      if (!placed) continue;
      const tokens = piece.characters * ratio!;
      let cost = 0;
      let start = offset;
      let floor = 0;
      for (const [size, price] of slots as [number, number][]) {
        const end = floor + size;
        const overlap = Math.max(
          0,
          Math.min(end, start + tokens) - Math.max(floor, start),
        );
        cost += overlap * price;
        floor = end;
      }
      offset += tokens;
      const sum = charged.get(key) ?? { tokens: 0, cost: 0 };
      charged.set(key, { tokens: sum.tokens + tokens, cost: sum.cost + cost });
    }
    for (const [key, sum] of charged) {
      const row = rows.get(key)! as ContextCost & { time: number };
      row.priced++;
      row.costUsd += sum.cost;
      if (r.time >= row.time) {
        row.time = r.time;
        row.tokens = Math.round(sum.tokens);
      }
    }
    const last = latest.get(owner);
    if (!last || r.time >= last.time) latest.set(owner, { time: r.time, keys });
  }
  const skills = new Map<string, { calls: number; last: number }>();
  for (const i of scan.invocations)
    // Typed requests keep working when a skill is hidden from the model, so only
    // the model's own calls show what its listing is for.
    if (i.kind === "skill-call" && i.status !== "reference") {
      const key = `${json([...i.profiles].sort())}:${i.target}`;
      const prior = skills.get(key) ?? { calls: 0, last: -Infinity };
      skills.set(key, {
        calls: prior.calls + 1,
        last: Math.max(prior.last, i.time),
      });
    }
  for (const [key, row] of rows) {
    const owner = json(row.profiles);
    row.current = latest.get(owner)?.keys.has(key) ?? false;
    delete (row as Partial<ContextCost & { time: number }>).time;
    let calls: { calls: number; last: number } | null = null;
    if (row.kind === "tool" || row.kind === "mcp-server") {
      calls = { calls: 0, last: -Infinity };
      for (const c of scan.toolCalls) {
        if (json([...c.profiles].sort()) !== owner) continue;
        const match =
          row.kind === "mcp-server"
            ? c.name.startsWith(`mcp__${row.name}__`)
            : c.name === row.name ||
              (row.name === "Artifact" && c.name.startsWith("Artifact"));
        if (!match) continue;
        calls.calls += c.calls;
        calls.last = Math.max(calls.last, c.lastUsed);
      }
    } else if (row.kind === "skill")
      calls = skills.get(`${owner}:${identity(`skill:${row.name}`)}`) ?? {
        calls: 0,
        last: -Infinity,
      };
    if (calls) {
      row.calls = calls.calls;
      row.lastUsed = calls.calls ? calls.last : null;
      row.costPerUseUsd = row.costUsd / Math.max(1, calls.calls);
    }
    if (row.control && row.current && row.priced && row.costPerUseUsd !== null)
      row.verdict = row.costPerUseUsd > input.maxCostPerUseUsd ? "off" : "keep";
  }
  return [...rows.values()].sort((a, b) => b.costUsd - a.costUsd);
}
