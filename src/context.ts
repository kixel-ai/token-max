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
  /** API-price equivalent over the priced requests only. */
  costUsd: number;
  /** costUsd scaled to every request that carried it; null when none could be priced. */
  estimatedUsd: number | null;
  /** Observed uses in the window; null when uses are not observable. Skills count only model calls, since typing one still works when it is hidden. */
  calls: number | null;
  lastUsed: number | null;
  /** Whether the profile's most recent request still carried it. */
  current: boolean;
  /** estimatedUsd divided by uses, counting no use as one; null without both. */
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
/** A piece as it appears in one request. */
interface Placed extends Piece {
  /** Tools and the system prompt lead the request in recorded order; everything else rides in messages. */
  prefix: boolean;
  /** The component revision it came from, so changed content is not mistaken for carried content. */
  revision: string;
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
 * shape. A piece is charged at the cache state of its position in the prompt:
 * tools and the system prompt lead in recorded order. Message attachments have
 * no recorded offset, so each takes the latest position its history allows:
 * the end of the session's previous request when that request carried it too,
 * else the end of this request, where newly added content goes.
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
  const rows = new Map<string, ContextCost & { time: number }>();
  const latest = new Map<string, { time: number; keys: Set<string> }>();
  const previous = new Map<
    string,
    { input: number | null; keys: Set<string> }
  >();
  const ordered = scan.requests
    .map((exposure) => ({ exposure, r: requests.get(exposure.request) }))
    .filter(
      (e): e is { exposure: (typeof scan.requests)[number]; r: Request } =>
        !!e.r && e.exposure.linkage === "wire" && e.r.harness === "claude",
    )
    .sort((a, b) => a.r.time - b.r.time);
  for (const { exposure, r } of ordered) {
    const profiles = [...r.profiles].sort();
    const owner = json(profiles);
    const parts: Placed[] = exposure.components.flatMap((id, index) => {
      let cached = piecesOf.get(id);
      if (!cached) {
        cached = pieces(components.get(id)!, components);
        piecesOf.set(id, cached);
      }
      const prefix = index < exposure.prefix;
      return cached.map((piece) => ({ ...piece, prefix, revision: id }));
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
    const size = (piece: Placed) => piece.characters * (ratio ?? 0);
    const total = parts.reduce((n, piece) => n + size(piece), 0);
    // A calibration that overshoots the recorded input cannot place these pieces.
    const placed = priced && total <= u!.input!;
    const unique = new Map(
      parts.map((piece) => [`${owner}:${piece.key}`, piece]),
    );
    const keys = new Set(unique.keys());
    for (const [key, piece] of unique) {
      const row = rows.get(key) ?? {
        profiles,
        kind: piece.kind,
        name: piece.name,
        control: piece.control,
        requests: 0,
        priced: 0,
        tokens: null,
        costUsd: 0,
        estimatedUsd: null,
        calls: null,
        lastUsed: null,
        current: false,
        costPerUseUsd: null,
        verdict: null,
        time: -Infinity,
      };
      row.requests++;
      rows.set(key, row);
    }
    const revision = (piece: Placed) => `${piece.revision}:${piece.key}`;
    const prior = previous.get(r.session);
    previous.set(r.session, {
      input: u?.input ?? null,
      keys: new Set(parts.filter((x) => !x.prefix).map(revision)),
    });
    if (placed) {
      const end = u!.input!;
      // Cache slots in prefix order: reads, then 1h writes, then 5m writes, then ordinary input.
      const slots: [number, number][] = [
        [u!.read!, rates!.read],
        [u!.write === 0 ? 0 : u!.write1h!, rates!.write1h],
        [u!.write === 0 ? 0 : u!.write5m!, rates!.write5m],
        [Infinity, rates!.ordinary],
      ];
      const charge = (start: number, tokens: number) => {
        let cost = 0;
        let floor = 0;
        for (const [length, price] of slots) {
          cost +=
            Math.max(
              0,
              Math.min(floor + length, start + tokens) - Math.max(floor, start),
            ) * price;
          floor += length;
        }
        return cost;
      };
      const head = parts.filter((piece) => piece.prefix);
      // Attachments are placed only when the session's previous request bounds
      // them: one it carried lies within its input, and a new one was appended
      // after it. A cold request writes everything, so position matters little.
      const bounded = (prior && prior.input !== null) || u!.read === 0;
      const carried = parts.filter(
        (piece) => !piece.prefix && prior?.keys.has(revision(piece)),
      );
      const fresh = parts.filter(
        (piece) => !piece.prefix && !prior?.keys.has(revision(piece)),
      );
      const sum = (list: Placed[]) => list.reduce((n, x) => n + size(x), 0);
      const prefixEnd = sum(head);
      const starts = new Map<Placed, number>();
      let offset = 0;
      for (const piece of head) {
        starts.set(piece, offset);
        offset += size(piece);
      }
      const stack = (list: Placed[], last: number) => {
        let offset = Math.max(prefixEnd, last - sum(list));
        for (const piece of list) {
          starts.set(piece, offset);
          offset += size(piece);
        }
      };
      stack(carried, Math.min(prior?.input ?? end, end));
      stack(fresh, end);
      const charged = new Map<string, { tokens: number; cost: number }>();
      for (const piece of parts) {
        if (!piece.prefix && !bounded) continue;
        const key = `${owner}:${piece.key}`;
        const tokens = size(piece);
        const prior = charged.get(key) ?? { tokens: 0, cost: 0 };
        charged.set(key, {
          tokens: prior.tokens + tokens,
          cost: prior.cost + charge(starts.get(piece)!, tokens),
        });
      }
      for (const [key, sum] of charged) {
        const row = rows.get(key)!;
        row.priced++;
        row.costUsd += sum.cost;
        if (r.time >= row.time) {
          row.time = r.time;
          row.tokens = Math.round(sum.tokens);
        }
      }
    }
    const last = latest.get(owner);
    if (!last || r.time >= last.time) latest.set(owner, { time: r.time, keys });
  }
  // Copied histories can leave a call and the requests around it with different
  // profile sets, so each call goes to the row whose profiles differ least.
  const families = new Map<string, ContextCost[]>();
  for (const row of rows.values()) {
    if (
      row.kind !== "tool" &&
      row.kind !== "mcp-server" &&
      row.kind !== "skill"
    )
      continue;
    const family = `${row.kind}:${row.name}`;
    families.set(family, [...(families.get(family) ?? []), row]);
    row.calls = 0;
  }
  const last = new Map<ContextCost, number>();
  const credit = (
    family: string,
    profiles: string[],
    time: number,
    calls = 1,
  ) => {
    let best: ContextCost | null = null;
    let distance = Infinity;
    for (const row of families.get(family) ?? []) {
      const shared = profiles.filter((p) => row.profiles.includes(p)).length;
      const d = row.profiles.length + profiles.length - 2 * shared;
      if (shared && d < distance) {
        best = row;
        distance = d;
      }
    }
    if (!best) return;
    best.calls! += calls;
    last.set(best, Math.max(last.get(best) ?? -Infinity, time));
  };
  for (const c of scan.toolCalls) {
    const family = c.name.startsWith("mcp__")
      ? `mcp-server:${c.name.split("__")[1]}`
      : `tool:${c.name.startsWith("Artifact") ? "Artifact" : c.name}`;
    credit(family, c.profiles, c.lastUsed, c.calls);
  }
  const skills = new Map<string, string>();
  for (const row of rows.values())
    if (row.kind === "skill")
      skills.set(identity(`skill:${row.name}`), `skill:${row.name}`);
  for (const i of scan.invocations)
    // Typed requests keep working when a skill is hidden from the model, so only
    // the model's own calls show what its listing is for.
    if (
      i.kind === "skill-call" &&
      i.status !== "reference" &&
      skills.has(i.target)
    )
      credit(skills.get(i.target)!, i.profiles, i.time);
  for (const [key, row] of rows) {
    row.current = latest.get(json(row.profiles))?.keys.has(key) ?? false;
    row.estimatedUsd = row.priced
      ? (row.costUsd * row.requests) / row.priced
      : null;
    row.lastUsed = last.get(row) ?? null;
    delete (row as Partial<ContextCost & { time: number }>).time;
    if (row.calls !== null)
      row.costPerUseUsd =
        row.estimatedUsd === null
          ? null
          : row.estimatedUsd / Math.max(1, row.calls);
    if (row.control && row.current && row.costPerUseUsd !== null)
      row.verdict = row.costPerUseUsd > input.maxCostPerUseUsd ? "off" : "keep";
  }
  return [...rows.values()].sort(
    (a, b) => (b.estimatedUsd ?? -1) - (a.estimatedUsd ?? -1),
  );
}
