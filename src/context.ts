import type { Request } from "./model.js";
import {
  calibrate,
  cohort,
  type Component,
  type ExposureScan,
} from "./exposure.js";
import { unitRates, type Catalog } from "./prices.js";
import { identity } from "./safe.js";

/** What one piece of always-sent context cost over the window, and how to turn it off. */
export interface ContextCost {
  profiles: string[];
  kind: "tool" | "mcp-server" | "skill" | "instructions" | "system" | "other";
  name: string;
  /** The settings change that removes it, or null when no setting does. */
  control: string | null;
  /** Selected requests that carried it, and how many of those could be priced. */
  requests: number;
  priced: number;
  /** Tokens it took in the most recent request that carried it. */
  tokens: number | null;
  /** API-price equivalent over the priced requests. */
  costUsd: number;
  /** Observed uses in the window; null when uses are not observable. */
  calls: number | null;
  lastUsed: number | null;
  /** Whether the profile's most recent request still carried it. */
  current: boolean;
}

interface Piece {
  key: string;
  kind: ContextCost["kind"];
  name: string;
  control: string | null;
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
      control: `"permissions": {"deny": [${json(`mcp__${server}`)}]}`,
    };
  }
  // The Artifact tool's helpers (ArtifactComments, ArtifactData) go with it.
  if (name.startsWith("Artifact"))
    return {
      key: "tool:Artifact",
      kind: "tool",
      name: "Artifact",
      control: `"enableArtifact": false`,
    };
  if (name === "Workflow")
    return {
      key: "tool:Workflow",
      kind: "tool",
      name,
      control: `"enableWorkflows": false`,
    };
  return {
    key: `tool:${name}`,
    kind: "tool",
    name,
    control: CORE_TOOLS.has(name)
      ? null
      : `"permissions": {"deny": [${json(name)}]}`,
  };
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
          control: `"skillOverrides": {${json(m.name)}: "user-invocable-only"}`,
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
    case "plugin-instructions":
      return [
        {
          key: `mcp:${name.replace(/[^A-Za-z0-9_-]/g, "_")}`,
          kind: "mcp-server",
          name: name.replace(/[^A-Za-z0-9_-]/g, "_"),
          control: `"permissions": {"deny": [${json(`mcp__${name.replace(/[^A-Za-z0-9_-]/g, "_")}`)}]}`,
          characters,
        },
      ];
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
    if (
      (i.kind === "skill-call" || i.kind === "skill-request") &&
      i.status !== "reference"
    ) {
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
    }
  }
  return [...rows.values()].sort((a, b) => b.costUsd - a.costUsd);
}
