import type { Cost, Harness, Location, Request, Snapshot } from "./model.js";
import {
  count,
  id,
  identity,
  label,
  object,
  ScanError,
  timestamp,
} from "./safe.js";
import { pricePart, unknownCost, type Catalog } from "./prices.js";

/** Each context category has exactly one downstream owner. */
export type Category =
  | "skill-listing"
  | "skill-body"
  | "mcp-discovery"
  | "mcp-schema"
  | "mcp-result"
  | "instructions"
  | "agents"
  | "hooks"
  | "plugin-instructions"
  | "app-instructions"
  | "managed-instructions"
  | "other";
/** A transient input becomes counts and fingerprints only. */
export interface Size {
  characters: number | null;
  recordedTokens: number | null;
  fingerprint: string;
}
/** One revision of one historical component, with no retained content. */
export interface Component extends Size {
  id: string;
  key: string;
  category: Category;
  owner: "skills" | "context";
  group: string | null;
  provenance: string;
  locations: Location[];
  firstSeen: number;
  current: "unresolved";
  scope: "component" | "aggregate";
  members: string[];
  targets: string[];
  /** Hashed entry metadata connects names and paths without retaining descriptions. */
  skill?: { name: string; path: string | null; description: string | null };
  catalog?: { budgetExceeded: boolean };
}
/** Actual calls, requested skills and weak script references are different evidence. */
export interface Invocation {
  id: string;
  target: string;
  group: string | null;
  kind:
    | "skill-request"
    | "skill-reference"
    | "skill-call"
    | "skill-load"
    | "mcp-call"
    | "mcp-resource"
    | "mcp-prompt"
    | "script-reference";
  status: "requested" | "called" | "completed" | "failed" | "reference";
  via: "typed" | "mention" | "wrapper" | "tool" | "completion" | "script";
  session: string;
  sessions: string[];
  identityBasis: "recorded" | "content-occurrence";
  harness: Harness;
  client: string | null;
  version: string | null;
  time: number;
  profiles: string[];
  locations: Location[];
}
/** A request-local view of historical state, including unresolved visibility. */
export interface Exposure {
  request: string;
  components: string[];
  candidates: string[];
  removed: string[];
  inherited: string[];
  deferred: string[];
  linkage: "wire" | "chronological" | "unknown";
  coverage: "complete-text" | "partial" | "unknown";
  references: {
    base: string | null;
    keep: number | null;
    tail: string[];
    items: string[] | null;
  } | null;
  completeCharacters: number | null;
  wireModel: string | null;
  reasons: string[];
}
/** Coverage is scoped to a concrete harness, client, version, component and window. */
export interface Visibility {
  harness: Harness;
  client: string | null;
  version: string | null;
  profiles: string[];
  component: string | null;
  since: number;
  until: number;
  exposure: "partial" | "unknown";
  directCalls: "observed" | "unknown";
  nestedCalls: "partial" | "unknown";
  typedSkills: "observed" | "unknown";
  automaticUnused: false;
  reasons: string[];
}
/** Parser output shared by later skills and context checks. */
export interface ExposureScan {
  components: Component[];
  requests: Exposure[];
  invocations: Invocation[];
  visibility: Visibility[];
}
/** Character estimates are calibrated only within a matching model and client cohort. */
export interface Calibration {
  cohort: string;
  samples: { request: string; characters: number; tokens: number }[];
  tokensPerCharacter: number;
  range: [number, number];
  assumptions: string[];
}
/** Prefer recorded counts; a missing calibration leaves token and dollar estimates unknown. */
export function measureSize(input: { text: unknown; tokens?: unknown }): Size {
  return {
    characters: typeof input.text === "string" ? characters(input.text) : null,
    recordedTokens: count(input.tokens),
    fingerprint: identity(
      typeof input.text === "string" ? input.text : "opaque",
    ),
  };
}

function characters(value: string): number {
  let result = 0;
  for (const _ of value) result++;
  return result;
}
function array(value: unknown): unknown[] {
  if (value === undefined) return [];
  if (!Array.isArray(value)) throw new ScanError("invalid context list");
  return value;
}
function record(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new ScanError("invalid context object");
  return object(value);
}
function text(value: unknown): string {
  if (typeof value !== "string") throw new ScanError("invalid context text");
  return value;
}
function completeText(params: Record<string, unknown>): boolean {
  function content(value: unknown): boolean {
    if (typeof value === "string") return true;
    let complete = true;
    for (const v of array(value)) {
      const block = record(v);
      if (block.type === "text") text(block.text);
      else if (block.type === "tool_use") {
        text(block.id);
        text(block.name);
        record(block.input);
      } else if (block.type === "tool_result") {
        text(block.tool_use_id);
        if (block.content !== undefined && !content(block.content))
          complete = false;
      } else complete = false;
    }
    return complete;
  }
  let complete = content(params.system);
  for (const v of array(params.messages)) {
    const message = record(v);
    if (message.role !== "user" && message.role !== "assistant")
      throw new ScanError("invalid wire message role");
    if (message.content === undefined)
      throw new ScanError("missing wire message content");
    if (!content(message.content)) complete = false;
  }
  for (const v of array(params.tools)) {
    const tool = record(v);
    if (tool.input_schema === undefined) complete = false;
    else record(tool.input_schema);
  }
  return complete;
}

function cohort(r: Request): string {
  return identity(
    JSON.stringify([r.harness, r.model, r.provider, r.client, r.version]),
  );
}
function target(kind: "skill" | "mcp", value: string): string {
  return identity(`${kind}:${value}`);
}
function group(value: string): string | null {
  const plugin = /^(?:mcp__)?([^:]+):/.exec(value);
  const mcp = /^mcp__(.+?)(?:__|$)/.exec(value);
  return plugin
    ? identity(`plugin:${plugin[1]}`)
    : mcp
      ? identity(`mcp-server:${mcp[1]}`)
      : null;
}

/** Collect derived metadata in the reader's frozen, single parsing pass. */
export class ExposureCollector {
  private components = new Map<string, Component>();
  private invocations = new Map<string, Invocation>();
  private observations = new Map<
    string,
    { exposure: Omit<Exposure, "request">; session: string }
  >();
  private file: Snapshot | null = null;
  private active = new Map<string, string>();
  private pendingRemoved = new Set<string>();
  private session = "";
  private client: string | null = null;
  private version: string | null = null;
  private uncertain = true;
  private wire: Omit<Exposure, "request"> | null = null;
  private deferred = new Set<string>();
  private digests = new Map<string, string[]>();
  private views = new Map<string, Omit<Exposure, "request">>();
  private knownSkills = new Set<string>();
  private userAliases = new Map<string, Map<string, number>>();
  private invalidated = false;
  private catalogSpans = new Map<
    string,
    { units: number; prefix: number; digest: string }
  >();
  private pendingTools = new Map<
    string,
    {
      target: string;
      group: string | null;
      kind: Invocation["kind"];
    }
  >();

  private component(input: {
    category: Category;
    key: string;
    content: unknown;
    location: Location;
    time: number;
    provenance: string;
    opaque?: boolean;
    group?: string | null;
    deferred?: boolean;
    member?: boolean;
    targets?: string[];
  }): string {
    const key = identity(`${input.category}:${input.key}`);
    const size = measureSize({
      text: input.opaque ? undefined : input.content,
    });
    if (input.opaque)
      size.fingerprint = identity(JSON.stringify(input.content));
    const revision = identity(`${key}:${size.fingerprint}`);
    const prior = this.active.get(key);
    if (!input.deferred && !input.member && prior && prior !== revision)
      this.pendingRemoved.add(prior);
    if (input.member) {
      /* Entry spans are owned by their aggregate, never additive exposure. */
    } else if (input.deferred) this.deferred.add(revision);
    else this.active.set(key, revision);
    const existing = this.components.get(revision);
    if (existing) {
      existing.locations.push(input.location);
      existing.firstSeen = Math.min(existing.firstSeen, input.time);
    } else
      this.components.set(revision, {
        ...size,
        id: revision,
        key,
        category: input.category,
        owner: input.category.startsWith("skill-") ? "skills" : "context",
        group: input.group ?? null,
        provenance: input.provenance,
        locations: [input.location],
        firstSeen: input.time,
        current: "unresolved",
        scope:
          !input.member &&
          (input.category === "skill-listing" || input.category === "other")
            ? "aggregate"
            : "component",
        members: [],
        targets:
          input.targets ??
          (input.member
            ? [target("skill", input.key)]
            : input.category === "skill-body"
              ? [target("skill", input.key)]
              : input.category.startsWith("mcp-")
                ? [target("mcp", input.key)]
                : []),
      });
    if (input.category === "skill-body" && typeof input.content === "string") {
      const prefix = input.content.split("\n", 1)[0]!;
      this.catalogSpans.set(revision, {
        units: input.content.length,
        prefix: prefix.length,
        digest: identity(prefix),
      });
    }
    return revision;
  }
  private clear(category?: Category) {
    for (const [key, revision] of this.active) {
      if (!category || this.components.get(revision)!.category === category) {
        this.pendingRemoved.add(revision);
        this.active.delete(key);
      }
    }
  }
  private remove(category: Category, name: string) {
    const key = identity(`${category}:${name}`);
    const previous = this.active.get(key);
    if (previous) this.pendingRemoved.add(previous);
    this.active.delete(key);
  }
  private invocation(input: {
    target: string;
    group: string | null;
    kind: Invocation["kind"];
    via: Invocation["via"];
    status: Invocation["status"];
    call: string;
    time: number;
    location: Location;
    identityBasis?: Invocation["identityBasis"];
  }) {
    const invocationId = identity(
      `${this.file!.harness}:${input.call}:${input.kind}`,
    );
    const prior = this.invocations.get(invocationId);
    if (prior) {
      prior.locations.push(input.location);
      prior.sessions = [...new Set([...prior.sessions, this.session])];
      prior.profiles = [
        ...new Set([...prior.profiles, ...this.file!.profiles]),
      ];
      if (input.status === "completed" || input.status === "failed")
        prior.status = input.status;
      return;
    }
    this.invocations.set(invocationId, {
      id: invocationId,
      target: input.target,
      group: input.group,
      kind: input.kind,
      via: input.via,
      status: input.status,
      session: this.session,
      sessions: [this.session],
      identityBasis: input.identityBasis ?? "recorded",
      harness: this.file!.harness,
      client: this.client,
      version: this.version,
      time: input.time,
      profiles: [...this.file!.profiles],
      locations: [input.location],
    });
  }

  private view(value: Omit<Exposure, "request">): Omit<Exposure, "request"> {
    const key = identity(JSON.stringify(value));
    const previous = this.views.get(key);
    if (previous) return previous;
    this.views.set(key, value);
    return value;
  }

  /** Invalidate context after tolerated corrupt pre-window metadata, without inventing removals. */
  invalidate() {
    this.active.clear();
    this.pendingRemoved.clear();
    this.deferred.clear();
    this.digests.clear();
    this.pendingTools.clear();
    this.knownSkills.clear();
    this.wire = null;
    this.uncertain = true;
    this.invalidated = true;
  }

  /** Observe one raw record transiently, retaining no raw object or content strings. */
  observe(input: {
    raw: Record<string, unknown>;
    snapshot: Snapshot;
    location: Location;
  }) {
    const { raw, snapshot, location } = input;
    if (this.file !== snapshot) {
      this.file = snapshot;
      this.active = new Map();
      this.pendingRemoved = new Set();
      this.session = identity(snapshot.file);
      this.client = null;
      this.version = null;
      this.uncertain = true;
      this.wire = null;
      this.deferred = new Set();
      this.digests = new Map();
      this.pendingTools = new Map();
      this.knownSkills = new Set();
      this.userAliases = new Map();
      this.invalidated = false;
    }
    const p = object(raw.payload);
    if (snapshot.harness === "claude" && typeof raw.sessionId === "string")
      this.session = /[/\\]subagents[/\\]/.test(snapshot.file)
        ? identity(
            `${id(raw.sessionId)}:${snapshot.file.split(/[/\\]/).at(-1)}`,
          )
        : id(raw.sessionId);
    if (snapshot.harness === "claude") {
      this.client = label(raw.entrypoint) ?? this.client;
      this.version = label(raw.version) ?? this.version;
    }
    if (raw.type === "session_meta") {
      this.session = id(p.id ?? p.session_id);
      this.client = label(p.originator) ?? label(p.source);
      this.version = label(p.cli_version);
    }
    const time = timestamp(raw.timestamp);
    const add = (
      category: Category,
      key: string,
      content: unknown,
      provenance: string,
      opaque = false,
    ) =>
      this.component({
        category,
        key,
        content,
        provenance,
        opaque,
        time: time ?? 0,
        location,
        group: group(key),
      });
    const listing = (key: string, body: string, provenance: string) => {
      const aggregate = add("skill-listing", key, body, provenance);
      this.components.get(aggregate)!.catalog = {
        budgetExceeded: /Exceeded skills context budget\./.test(body),
      };
      const prefix = body.split("\n", 1)[0]!;
      this.catalogSpans.set(aggregate, {
        units: body.length,
        prefix: prefix.length,
        digest: identity(prefix),
      });
      const members: string[] = [];
      const matches = [
        ...body.matchAll(/^- ([a-zA-Z0-9_.:-]+)(?:: ([^\n]*))?$/gm),
      ];
      matches.forEach((match) => {
        const name = target("skill", match[1]!);
        const path = / \(file: ([^\n]+)\)$/.exec(match[2] ?? "");
        const pathTarget = path ? target("skill", path[1]!) : null;
        this.knownSkills.add(name);
        const member = this.component({
          category: "skill-listing",
          key: path?.[1] ?? match[1]!,
          content: match[0],
          provenance: "rendered-listing-entry-span",
          location,
          time: time ?? 0,
          member: true,
          group: group(match[1]!),
          targets: pathTarget ? [name, pathTarget] : [name],
        });
        this.components.get(member)!.skill = {
          name,
          path: pathTarget,
          description:
            match[2] === undefined
              ? null
              : identity(path ? match[2]!.slice(0, path.index) : match[2]),
        };
        members.push(member);
      });
      this.components.get(aggregate)!.members = members;
      return aggregate;
    };
    if (
      raw.type === "compacted" ||
      (raw.type === "system" && raw.subtype === "compact_boundary")
    ) {
      this.clear();
      this.uncertain = true;
      this.wire = null;
      this.deferred.clear();
    }
    if (raw.type === "world_state") {
      const state = record(p.state);
      if (p.full !== undefined && typeof p.full !== "boolean")
        throw new ScanError("invalid world state mode");
      if (p.full === true) this.clear();
      for (const [field, category] of [
        ["host_skills", "skill-listing"],
        ["agents_md", "instructions"],
        ["managed_developer_instructions", "managed-instructions"],
      ] as const) {
        if (!(field in state)) continue;
        const value = state[field];
        if (value === null) {
          this.clear(category);
          continue;
        }
        const v = record(value);
        const body = field === "host_skills" ? v.body : v.text;
        // Structured entries describe availability/fingerprints, not rendered sizes.
        if (body !== undefined) {
          this.clear(category);
          if (category === "skill-listing")
            listing(field, text(body), "world-state-rendered");
          else add(category, field, text(body), "world-state-rendered");
        } else if (Object.keys(v).length) {
          this.clear(category);
          add(category, field, v, "world-state-metadata-only", true);
        }
      }
      for (const [field, category] of [
        ["tools", "mcp-discovery"],
        ["top_level_tools", "mcp-schema"],
      ] as const) {
        if (!(field in state)) continue;
        for (const [name, value] of Object.entries(record(state[field]))) {
          if (value === null) this.remove(category, name);
          else
            add(
              category,
              name,
              text(value),
              field === "tools"
                ? "deferred-namespace-description"
                : "opaque-schema-identity",
              field === "top_level_tools",
            );
        }
      }
      // Boolean policy flags cannot establish the rendered plugin/app instruction body.
      for (const [field, category] of [
        ["plugins_instructions", "plugin-instructions"],
        ["apps_instructions", "app-instructions"],
      ] as const)
        if (field in state)
          add(category, field, state[field], "world-state-policy-only", true);
    }
    if (raw.type === "attachment") {
      const a = record(raw.attachment);
      const kind = a.type;
      if (kind === "skill_listing") {
        for (const name of array(a.names))
          this.knownSkills.add(target("skill", text(name)));
        this.clear("skill-listing");
        listing("listing", text(a.content), "rendered-skill-listing");
      } else if (kind === "instructions") {
        for (const v of array(a.files)) {
          const f = record(v);
          add(
            "instructions",
            text(f.path),
            text(f.content),
            "instruction-attachment",
          );
        }
      } else if (kind === "prompt_snapshot") {
        this.clear("other");
        this.clear("mcp-schema");
        add(
          "other",
          "system-prompt",
          array(a.systemPrompt).map(text).join("\n"),
          "prompt-snapshot-system",
        );
        for (const v of array(a.tools)) {
          const tool = record(v);
          add(
            "mcp-schema",
            text(tool.name),
            JSON.stringify(tool),
            "prompt-snapshot-tool",
          );
        }
      } else if (
        [
          "deferred_tools_delta",
          "mcp_instructions_delta",
          "agent_listing_delta",
        ].includes(String(kind))
      ) {
        const category: Category =
          kind === "deferred_tools_delta"
            ? "mcp-discovery"
            : kind === "agent_listing_delta"
              ? "agents"
              : "plugin-instructions";
        const names = array(
          kind === "agent_listing_delta" ? a.addedTypes : a.addedNames,
        );
        const lines = array(
          kind === "mcp_instructions_delta" ? a.addedBlocks : a.addedLines,
        );
        const removed = array(
          kind === "agent_listing_delta" ? a.removedTypes : a.removedNames,
        );
        for (const name of removed) this.remove(category, text(name));
        if (a.isInitial === true) this.clear(category);
        // Preserve aggregate expansion when there is no verified one-to-one mapping.
        if (names.length === lines.length)
          names.forEach((name, i) =>
            add(category, text(name), text(lines[i]), "attachment-delta"),
          );
        else if (lines.length)
          add(
            category,
            `${String(kind)}:${location.line}`,
            lines.map(text).join("\n"),
            "unmapped-rendered-delta",
          );
        for (const v of array(a.surfacedDefinitions)) {
          const definition = record(v);
          const name = text(definition.name);
          this.remove("mcp-discovery", name);
          add(
            "mcp-schema",
            name,
            JSON.stringify(record(definition.definition)),
            "surfaced-tool-definition",
          );
        }
      } else if (kind === "deferred_tools_record") {
        // Deferred definitions exist locally but are not necessarily in the model input.
        for (const v of array(a.entries)) {
          const tool = record(v);
          this.component({
            category: "mcp-schema",
            key: text(tool.name),
            content: JSON.stringify(tool),
            provenance:
              tool.defer_loading === true
                ? "deferred-definition-not-exposed"
                : "eager-tool-record",
            time: time ?? 0,
            location,
            deferred: tool.defer_loading === true,
          });
        }
      } else if (raw.rendered !== undefined) {
        // Unknown attachment categories still contribute visible derived evidence.
        const rendered = array(raw.rendered)
          .map((v) => text(record(v).content))
          .join("\n");
        add(
          "other",
          String(raw.uuid ?? location.line),
          rendered,
          "unclassified-rendered-attachment",
        );
      }
    }
    if (raw.type === "response_item" && p.type === "additional_tools")
      for (const v of array(p.tools)) {
        const tool = record(v);
        add(
          "mcp-schema",
          text(tool.name),
          JSON.stringify(tool),
          "provided-additional-tools",
        );
      }

    // Full Messages API parameters can prove wire exposure; digest-only requests cannot.
    if (raw.type === "api-request") {
      const params = object(raw.params);
      const full =
        Array.isArray(params.messages) &&
        params.system !== undefined &&
        Array.isArray(params.tools);
      const refs = {
        base: typeof raw.base === "string" ? id(raw.base) : null,
        keep: count(raw.keep),
        tail: array(raw.tail).map((v) => id(v)),
        items: null as string[] | null,
      };
      const prior = refs.base ? this.digests.get(refs.base) : undefined;
      if (prior && (refs.keep === null || refs.keep > prior.length))
        throw new ScanError("invalid request digest prefix length");
      refs.items = refs.base
        ? prior
          ? [...prior.slice(0, refs.keep!), ...refs.tail]
          : null
        : refs.tail;
      if (typeof raw.requestDigest === "string" && refs.items)
        this.digests.set(id(raw.requestDigest), refs.items);
      let components: string[] = [];
      let completeCharacters: number | null = null;
      if (full) {
        // Match recorded catalog spans by hashes in actual wire text. Nothing from
        // an attachment is promoted merely because it preceded this request.
        const catalogs = [...this.active.values()].filter(
          (id) =>
            ["skill-listing", "skill-body"].includes(
              this.components.get(id)!.category,
            ) && this.catalogSpans.has(id),
        );
        const matched = new Set<string>();
        const strip = (value: unknown): unknown => {
          if (Array.isArray(value)) return value.map(strip);
          if (value && typeof value === "object")
            return Object.fromEntries(
              Object.entries(value).map(([key, v]) => [
                key,
                key === "text" || key === "content" ? strip(v) : v,
              ]),
            );
          if (typeof value !== "string") return value;
          let result = value;
          for (const catalog of catalogs) {
            if (matched.has(catalog)) continue;
            const span = this.catalogSpans.get(catalog)!;
            if (!span.units) continue;
            const starts = [
              0,
              ...Array.from(result.matchAll(/\n/g), (m) => m.index! + 1),
            ];
            for (const start of starts) {
              if (
                identity(result.slice(start, start + span.prefix)) !==
                  span.digest ||
                identity(result.slice(start, start + span.units)) !==
                  this.components.get(catalog)!.fingerprint
              )
                continue;
              matched.add(catalog);
              result =
                result.slice(0, start) + result.slice(start + span.units);
              break;
            }
          }
          return result;
        };
        const system = strip(params.system);
        // Message spans are matched too; only the system remainder is a separately
        // attributed component, so listings never overlap its context ownership.
        strip(params.messages);
        components = [
          add("other", "wire-system", JSON.stringify(system), "wire-system"),
        ];
        components.push(...matched);
        for (const v of array(params.tools)) {
          const tool = record(v);
          components.push(
            add(
              "mcp-schema",
              text(tool.name),
              JSON.stringify(tool),
              "wire-tool",
            ),
          );
        }
        if (completeText(params))
          completeCharacters = characters(
            JSON.stringify({
              system: params.system,
              messages: params.messages,
              tools: params.tools,
            }),
          );
      }
      this.wire = {
        components,
        candidates: full ? [] : [...this.active.values()],
        removed: [...this.pendingRemoved],
        inherited: [],
        deferred: [...this.deferred],
        linkage: full ? "wire" : "unknown",
        coverage: completeCharacters === null ? "partial" : "complete-text",
        references: refs,
        completeCharacters,
        wireModel: label(params.model),
        reasons: full
          ? [
              "serialized text includes wire framing; API tokenization is calibrated, not exact",
            ]
          : [
              "request digest/base/keep/tail hashes have no verified content mapping; attachment state is candidate exposure only",
            ],
      };
      this.observations.set(`${snapshot.file}:ref:${id(raw.id)}`, {
        exposure: this.view(this.wire),
        session: this.session,
      });
    }

    const message = object(raw.message);
    const item =
      raw.type === "event_msg" && p.type === "item_completed"
        ? record(p.item)
        : p;
    const contents =
      snapshot.harness === "claude"
        ? message.content
        : (item.content ?? (p.type === "user_message" ? p.message : undefined));
    const user =
      raw.type === "user" ||
      item.role === "user" ||
      item.type === "UserMessage" ||
      (raw.type === "event_msg" && p.type === "user_message");
    const invoke = (
      name: string,
      kind: Invocation["kind"],
      via: Invocation["via"],
      status: Invocation["status"],
      call: string,
      attribution: string | null = group(name),
      identityBasis: Invocation["identityBasis"] = "recorded",
    ) => {
      if (time === null) throw new ScanError("missing invocation timestamp");
      this.invocation({
        target: target(kind.startsWith("skill") ? "skill" : "mcp", name),
        identityBasis,
        group: attribution,
        kind,
        via,
        status,
        call,
        time,
        location,
      });
    };
    for (const [index, v] of (typeof contents === "string"
      ? [{ type: "text", text: contents }]
      : array(contents)
    ).entries()) {
      const b = object(v);
      let call =
        String(raw.uuid ?? item.id ?? `${snapshot.file}:${location.line}`) +
        `:${index}`;
      let identityBasis: Invocation["identityBasis"] = "recorded";
      if (user && snapshot.harness === "codex" && typeof b.text === "string") {
        // Native aliases lack a shared message id. Match their nth equal text occurrence,
        // keeping repeated messages in each source separate and disclosing the inference.
        const signature = identity(`${this.session}:${b.text}`);
        const source = `${String(raw.type)}:${String(p.type)}`;
        const sources =
          this.userAliases.get(signature) ?? new Map<string, number>();
        const occurrence = (sources.get(source) ?? 0) + 1;
        sources.set(source, occurrence);
        this.userAliases.set(signature, sources);
        call = `user:${signature}:${occurrence}`;
        identityBasis = "content-occurrence";
      }
      if (user && b.type === "skill") {
        this.knownSkills.add(target("skill", text(b.name)));
        invoke(text(b.name), "skill-request", "typed", "requested", call);
      }
      if (user && raw.isMeta !== true && typeof b.text === "string") {
        for (const match of b.text.matchAll(
          /(?:^|\s)\$([a-zA-Z0-9_-]+(?::[a-zA-Z0-9_-]+)?)/g,
        )) {
          const known = this.knownSkills.has(target("skill", match[1]!));
          invoke(
            match[1]!,
            known ? "skill-request" : "skill-reference",
            "mention",
            known ? "requested" : "reference",
            `${call}:${match.index}`,
            group(match[1]!),
            identityBasis,
          );
        }
        const command =
          /<command-name>\/?([a-zA-Z0-9_:-]+)<\/command-name>/.exec(b.text);
        if (command) {
          const known = this.knownSkills.has(target("skill", command[1]!));
          invoke(
            command[1]!,
            known ? "skill-request" : "skill-reference",
            "wrapper",
            known ? "requested" : "reference",
            call,
            group(command[1]!),
            identityBasis,
          );
        }
      }
      if (b.type === "tool_use") {
        const name = text(b.name);
        const args = object(b.input);
        if (name === "Skill") {
          const skill = text(args.skill);
          invoke(skill, "skill-call", "tool", "called", text(b.id));
          this.pendingTools.set(id(b.id), {
            target: target("skill", skill),
            group: group(skill),
            kind: "skill-call",
          });
        } else if (name.startsWith("mcp__")) {
          invoke(name, "mcp-call", "tool", "called", text(b.id));
          this.pendingTools.set(id(b.id), {
            target: target("mcp", name),
            group: group(name),
            kind: "mcp-call",
          });
        } else if (
          name === "Read" &&
          typeof args.file_path === "string" &&
          /[/\\]SKILL\.md$/.test(args.file_path)
        ) {
          invoke(args.file_path, "skill-load", "tool", "called", text(b.id));
          this.pendingTools.set(id(b.id), {
            target: target("skill", args.file_path),
            group: group(args.file_path),
            kind: "skill-load",
          });
        }
      }
      if (b.type === "tool_result" && typeof b.tool_use_id === "string") {
        const tool = this.pendingTools.get(id(b.tool_use_id));
        if (tool) {
          if (time === null)
            throw new ScanError("missing invocation timestamp");
          this.invocation({
            target: tool.target,
            group: tool.group,
            kind: tool.kind,
            via: "completion",
            status: b.is_error === true ? "failed" : "completed",
            call: b.tool_use_id,
            time,
            location,
          });
          if (b.is_error !== true)
            this.component({
              category: tool.kind.startsWith("skill")
                ? "skill-body"
                : "mcp-result",
              key: id(b.tool_use_id),
              targets: [tool.target],
              group: tool.group,
              content:
                typeof b.content === "string"
                  ? b.content
                  : JSON.stringify(b.content),
              provenance: "observed-tool-result-not-wire-proof",
              time,
              location,
            });
        }
      }
    }
    if (
      raw.type === "event_msg" &&
      p.type === "item_completed" &&
      item.type === "McpToolCall"
    ) {
      const name = `mcp__${text(item.server)}__${text(item.tool)}`;
      invoke(
        name,
        "mcp-call",
        "completion",
        object(item.result).isError === true || item.status === "failed"
          ? "failed"
          : "completed",
        text(item.id),
        typeof item.pluginId === "string"
          ? identity(`plugin:${item.pluginId}`)
          : group(name),
      );
      if (item.result !== undefined)
        add(
          "mcp-result",
          text(item.id),
          JSON.stringify(item.result),
          "observed-mcp-result-not-wire-proof",
        );
    }
    if (
      raw.type === "event_msg" &&
      p.type === "item_completed" &&
      item.type === "CommandExecution"
    ) {
      const reads = array(item.parsed_cmd)
        .map(object)
        .filter(
          (c) =>
            c.type === "read" &&
            typeof c.path === "string" &&
            /[/\\]SKILL\.md$/.test(c.path),
        );
      for (const read of reads) {
        invoke(
          text(read.path),
          "skill-load",
          "completion",
          item.exit_code === 0 ? "completed" : "failed",
          `${text(item.id)}:${id(read.path)}`,
        );
        if (
          reads.length === 1 &&
          array(item.parsed_cmd).length === 1 &&
          item.exit_code === 0 &&
          typeof item.aggregated_output === "string"
        )
          add(
            "skill-body",
            text(read.path),
            item.aggregated_output,
            "identified-file-read-output",
          );
      }
    }
    if (
      raw.type === "response_item" &&
      ["function_call", "custom_tool_call"].includes(String(p.type))
    ) {
      const bare = text(p.name);
      const name = bare.startsWith("mcp__")
        ? bare
        : typeof p.namespace === "string" && p.namespace.startsWith("mcp__")
          ? `${p.namespace}__${bare}`
          : `${typeof p.namespace === "string" ? p.namespace + "." : ""}${bare}`;
      const call = text(p.id ?? p.call_id);
      if (name.startsWith("mcp__"))
        invoke(name, "mcp-call", "tool", "called", call);
      else if (
        /^(?:(?:mcp|functions)\.)?(?:read_resource|read_mcp_resource|list_resources|list_mcp_resources|list_mcp_resource_templates|get_prompt)$/.test(
          name,
        )
      )
        invoke(
          name,
          name.endsWith("get_prompt") ? "mcp-prompt" : "mcp-resource",
          "tool",
          "called",
          call,
        );
      // Code mentions do not establish that any nested call executed.
      const code = p.arguments ?? p.input;
      if (typeof code === "string")
        for (const match of code.matchAll(/\b(mcp__[a-zA-Z0-9_]+)\s*\(/g))
          invoke(
            match[1]!,
            "script-reference",
            "script",
            "reference",
            `${call}:${match.index}`,
          );
    }
    const current = [...this.active.values()];
    const derived: Omit<Exposure, "request"> = (raw.type === "assistant" &&
    typeof raw.requestRef === "string"
      ? this.observations.get(`${snapshot.file}:ref:${id(raw.requestRef)}`)
          ?.exposure
      : undefined) ?? {
      components: [],
      candidates: current,
      removed: [...this.pendingRemoved],
      inherited: [],
      deferred: [...this.deferred],
      linkage: current.length ? "chronological" : "unknown",
      coverage: current.length ? "partial" : "unknown",
      references: null,
      completeCharacters: null,
      wireModel: null,
      reasons: [
        "chronological state is not proof of exact request exposure",
        ...(this.invalidated
          ? [
              "invalid pre-window component metadata; exposure cannot be reconstructed completely",
            ]
          : []),
        ...(this.uncertain
          ? ["initial/inherited or post-compaction exposure is incomplete"]
          : []),
      ],
    };
    if (
      raw.type === "assistant" ||
      raw.type === "token_usage_record" ||
      (raw.type === "event_msg" && p.type === "token_count")
    )
      this.observations.set(`${snapshot.file}:${location.line}`, {
        exposure: this.view(derived),
        session: this.session,
      });
    if (
      raw.type === "token_usage_record" ||
      (raw.type === "event_msg" && p.type === "token_count") ||
      raw.type === "assistant"
    )
      this.pendingRemoved.clear();
    if (current.length) this.uncertain = false;
  }

  /** Link derived views to deduplicated requests, retaining conflicts instead of choosing a copy. */
  finish(requests: Request[], since: number | null): ExposureScan {
    const requestsById = new Map(requests.map((r) => [r.id, r]));
    const exposures: Exposure[] = [];
    const visibility: Visibility[] = [];
    for (const r of requests) {
      const views = r.locations.flatMap((l) => {
        const observed = r.reference
          ? this.observations.get(`${l.file}:ref:${r.reference}`)
          : undefined;
        const view = observed ?? this.observations.get(`${l.file}:${l.line}`);
        return view ? [view.exposure] : [];
      });
      const first = views[0];
      const equivalent = (v: Omit<Exposure, "request">) =>
        JSON.stringify([
          v.components,
          v.candidates,
          v.deferred,
          v.linkage,
          v.coverage,
          v.references,
          v.completeCharacters,
          v.wireModel,
        ]);
      const same =
        first && views.every((v) => equivalent(v) === equivalent(first));
      const exposure: Exposure = same
        ? { ...first, request: r.id }
        : {
            request: r.id,
            components: [],
            candidates: [
              ...new Set(
                views.flatMap((v) => [...v.components, ...v.candidates]),
              ),
            ],
            removed: [],
            inherited: [],
            deferred: [...new Set(views.flatMap((v) => v.deferred))],
            linkage: "unknown",
            coverage: "unknown",
            references: null,
            completeCharacters: null,
            wireModel: null,
            reasons: [
              views.length
                ? "copied/inherited histories disagree on exposure"
                : "no observed request context",
            ],
          };
      exposure.removed = [...new Set(views.flatMap((v) => v.removed))];
      exposure.inherited = [
        ...exposure.components,
        ...exposure.candidates,
      ].filter(
        (id) => since !== null && this.components.get(id)!.firstSeen < since,
      );
      exposures.push(exposure);
    }
    const selectedCalls = [...this.invocations.values()].filter(
      (i) => since === null || i.time >= since,
    );
    const windows = new Map<
      string,
      {
        r: Request;
        component: string | null;
        since: number;
        until: number;
        sessions: Set<string>;
      }
    >();
    for (const e of exposures) {
      const r = requestsById.get(e.request)!;
      for (const component of [...e.components, ...e.candidates].length
        ? [...e.components, ...e.candidates]
        : [null]) {
        const key = JSON.stringify([
          r.harness,
          r.client,
          r.version,
          [...r.profiles].sort(),
          component,
        ]);
        const w = windows.get(key) ?? {
          r,
          component,
          since: since ?? r.time,
          until: r.endTime,
          sessions: new Set<string>(),
        };
        w.sessions.add(r.session);
        w.since = Math.min(w.since, r.time);
        w.until = Math.max(w.until, r.endTime);
        windows.set(key, w);
      }
    }
    for (const w of windows.values()) {
      const { r, component } = w;
      const calls = selectedCalls.filter(
        (i) =>
          i.sessions.some((session) => w.sessions.has(session)) &&
          i.harness === r.harness &&
          i.client === r.client &&
          i.version === r.version,
      );
      const windowStart =
        since ?? Math.min(w.since, ...calls.map((i) => i.time));
      const windowEnd = Math.max(w.until, ...calls.map((i) => i.time));
      visibility.push({
        harness: r.harness,
        client: r.client,
        version: r.version,
        profiles: r.profiles,
        component,
        since: windowStart,
        until: windowEnd,
        exposure: component === null ? "unknown" : "partial",
        directCalls: calls.some((i) => i.kind === "mcp-call")
          ? "observed"
          : "unknown",
        nestedCalls: calls.some(
          (i) => i.via === "completion" && i.kind === "mcp-call",
        )
          ? "partial"
          : "unknown",
        typedSkills: calls.some((i) => i.via === "typed")
          ? "observed"
          : "unknown",
        automaticUnused: false,
        reasons: [
          "call visibility describes the client/version window, not proof of use by each component",
          "no supported client/version establishes complete nested-call or typed-skill recording",
          "historical exposure and current effective enablement are independent",
        ],
      });
    }
    const used = new Set(
      exposures.flatMap((e) => [
        ...e.components,
        ...e.candidates,
        ...e.removed,
        ...e.inherited,
        ...e.deferred,
      ]),
    );
    for (const id of used)
      for (const member of this.components.get(id)!.members) used.add(member);
    return {
      components: [...this.components.values()].filter((c) => used.has(c.id)),
      requests: exposures,
      invocations: [...this.invocations.values()].filter(
        (i) => since === null || i.time >= since,
      ),
      visibility,
    };
  }
}

/** Calibrate complete text-only wire inputs against observed, single-request input totals. */
export function calibrate(
  scan: ExposureScan,
  requests: Request[],
): Calibration[] {
  const result = new Map<string, Calibration>();
  const requestsById = new Map(requests.map((r) => [r.id, r]));
  for (const exposure of scan.requests) {
    const r = requestsById.get(exposure.request);
    if (
      !r ||
      !r.model ||
      !r.client ||
      !r.version ||
      r.evidence !== "measured" ||
      r.parts.length !== 1 ||
      exposure.coverage !== "complete-text" ||
      !exposure.completeCharacters ||
      exposure.wireModel !== r.model
    )
      continue;
    const tokens = r.parts[0]!.usage.input;
    if (tokens === null || tokens === 0) continue;
    const key = cohort(r);
    const c = result.get(key) ?? {
      cohort: key,
      samples: [],
      tokensPerCharacter: 0,
      range: [Infinity, -Infinity] as [number, number],
      assumptions: [
        "empirical text-only wire serialization ratio includes framing; transfer to components is estimated",
        "observed sample extrema are sensitivity scenarios, not guaranteed bounds",
      ],
    };
    const ratio = tokens / exposure.completeCharacters;
    c.samples.push({
      request: r.id,
      characters: exposure.completeCharacters,
      tokens,
    });
    c.range = [Math.min(c.range[0], ratio), Math.max(c.range[1], ratio)];
    c.tokensPerCharacter =
      c.samples.reduce((n, s) => n + s.tokens, 0) /
      c.samples.reduce((n, s) => n + s.characters, 0);
    result.set(key, c);
  }
  return [...result.values()];
}

/** Convert a component using recorded tokens first, otherwise a matching empirical calibration. */
export function estimateTokens(
  size: Size,
  calibration?: Calibration,
): { estimate: number | null; range: [number, number] | null; basis: string } {
  if (size.recordedTokens !== null)
    return {
      estimate: size.recordedTokens,
      range: [size.recordedTokens, size.recordedTokens],
      basis: "recorded component tokens",
    };
  if (size.characters === null || !calibration)
    return {
      estimate: null,
      range: null,
      basis: "characters only; no complete matching text calibration",
    };
  return {
    estimate: Math.round(size.characters * calibration.tokensPerCharacter),
    range: [
      Math.floor(size.characters * calibration.range[0]),
      Math.ceil(size.characters * calibration.range[1]),
    ],
    basis: `empirical calibration ${calibration.cohort}`,
  };
}

/** Reprice a complete before/after input change, including the whole request's context tier. */
export function priceExposureChange(input: {
  catalog: Catalog;
  request: Request;
  removedTokens: number;
  addedTokens: number;
  position?: {
    read: number;
    write5m: number;
    write1h: number;
    ordinary: number;
  };
}): {
  gross: Cost;
  fixedBehaviorNet: Cost;
  range: [number, number] | null;
  assumptions: string[];
} {
  const { request: r, catalog } = input;
  const removed = count(input.removedTokens)!;
  const added = count(input.addedTokens)!;
  const assumptions = [
    "fixed behavior holds output and tool execution identical; this is an API-price equivalent",
    "remaining cache geometry is held fixed; actual cache-key changes and future reuse can differ",
    "use the observed replacement listing/schema footprint, including backfill and expansion; independent savings must not be summed",
  ];
  const unknown = (reason: string) => ({
    gross: unknownCost(reason),
    fixedBehaviorNet: unknownCost(reason),
    range: null,
    assumptions,
  });
  if (r.parts.length !== 1 || r.parts[0]!.usage.input === null)
    return unknown("single-part total request usage required");
  const p = r.parts[0]!;
  const u = p.usage;
  if (removed > u.input!)
    throw new ScanError("component removal exceeds request input");
  if (u.read === null || u.write === null)
    return unknown("request cache geometry unknown");
  if (r.harness !== "claude" && u.write !== 0)
    return unknown(
      "Codex write lifetime positions unavailable for this context change",
    );
  if (
    r.harness === "claude" &&
    u.write !== 0 &&
    (u.write5m === null || u.write1h === null)
  )
    return unknown("write lifetime allocation unknown");
  const slots = {
    read: u.read,
    write5m: u.write5m ?? 0,
    write1h: u.write1h ?? 0,
    ordinary: u.input! - u.read - u.write,
  };
  if (r.harness === "codex") slots.ordinary = u.input! - u.read;
  function cost(remove: typeof slots, replacement: number): Cost {
    const next = {
      ...u,
      input: u.input! - removed + replacement,
      read: u.read! - remove.read,
      write: u.write! - remove.write5m - remove.write1h,
      write5m: slots.write5m - remove.write5m,
      write1h: slots.write1h - remove.write1h,
    };
    // Replacement is ordinary input unless a caller provides a richer recorded counterfactual.
    return pricePart(catalog, r, { ...p, usage: next });
  }
  const baseline = pricePart(catalog, r, p);
  const allocations: (typeof slots)[] = [];
  if (input.position) {
    const position = input.position;
    if (
      Object.keys(slots).some(
        (k) =>
          count(position[k as keyof typeof slots])! >
          slots[k as keyof typeof slots],
      ) ||
      Object.values(position).reduce((n, v) => n + count(v)!, 0) !== removed
    )
      throw new ScanError("invalid component cache positions");
    allocations.push(position);
    assumptions.push(
      "recorded cache positions supplied by caller; replacement allocated to ordinary input",
    );
  } else {
    // Evaluate every vertex of the bounded allocation polytope, avoiding arbitrary cache assumptions.
    const keys = Object.keys(slots) as (keyof typeof slots)[];
    for (const free of keys) {
      const rest = keys.filter((k) => k !== free);
      for (let mask = 0; mask < 1 << rest.length; mask++) {
        const a = { read: 0, write5m: 0, write1h: 0, ordinary: 0 };
        rest.forEach((k, i) => (a[k] = mask & (1 << i) ? slots[k] : 0));
        a[free] = removed - Object.values(a).reduce((n, v) => n + v, 0);
        if (a[free] >= 0 && a[free] <= slots[free]) allocations.push(a);
      }
    }
    assumptions.push(
      "cache positions absent; proportional allocation is the estimate, feasible cache allocations supply sensitivity; replacement is ordinary input",
    );
  }
  if (baseline.totalUsd === null)
    return unknown("baseline request cost unknown; no zero billing assumed");
  const changes = allocations.map((a) => cost(a, added));
  if (changes.some((c) => c.totalUsd === null))
    return unknown("counterfactual request price unknown");
  const delta = (after: Cost): Cost => ({
    knownUsd: baseline.totalUsd! - after.totalUsd!,
    totalUsd: baseline.totalUsd! - after.totalUsd!,
    unknown: [],
    assumptions: [
      ...assumptions,
      ...baseline.assumptions,
      ...after.assumptions,
    ],
  });
  const proportional =
    input.position ??
    (Object.fromEntries(
      Object.entries(slots).map(([k, v]) => [
        k,
        u.input === 0 ? 0 : (removed * v) / u.input!,
      ]),
    ) as typeof slots);
  const gross = delta(cost(proportional, 0));
  const net = delta(cost(proportional, added));
  const values = changes.map((c) => baseline.totalUsd! - c.totalUsd!);
  return {
    gross,
    fixedBehaviorNet: net,
    range: [Math.min(...values), Math.max(...values)],
    assumptions,
  };
}

/** Build shared measurement output without making skills or context recommendations. */
export function analyzeExposure(
  scan: ExposureScan,
  requests: Request[],
  catalog: Catalog,
) {
  const calibration = calibrate(scan, requests);
  const requestsById = new Map(requests.map((r) => [r.id, r]));
  const componentsById = new Map(scan.components.map((c) => [c.id, c]));
  const calibrationByCohort = new Map(calibration.map((c) => [c.cohort, c]));
  return {
    calibration,
    requests: scan.requests.map((e) => {
      const r = requestsById.get(e.request)!;
      const c = calibrationByCohort.get(cohort(r));
      return {
        request: r.id,
        components: [...e.components, ...e.candidates].map((id) => {
          const component = componentsById.get(id)!;
          const tokens = estimateTokens(component, c);
          const priced =
            tokens.estimate === null ||
            !e.components.includes(id) ||
            (r.parts[0]?.usage.input !== null &&
              tokens.estimate > (r.parts[0]?.usage.input ?? 0))
              ? null
              : priceExposureChange({
                  catalog,
                  request: r,
                  removedTokens: tokens.estimate,
                  addedTokens: 0,
                });
          return {
            component: id,
            exposure: e.components.includes(id) ? "wire" : "candidate",
            tokens,
            gross:
              priced?.gross ??
              unknownCost(
                tokens.estimate !== null &&
                  tokens.estimate > (r.parts[0]?.usage.input ?? Infinity)
                  ? "calibrated component estimate exceeds observed request input; comparison unsupported"
                  : tokens.basis,
              ),
            grossRangeUsd: priced?.range ?? null,
            fixedBehaviorNet: unknownCost(
              "replacement exposure/backfill and behavior not established; gross removal is not net savings",
            ),
          };
        }),
      };
    }),
    assumptions: [
      "current effective component ownership remains unresolved; installed metadata is not historical exposure",
      "candidate exposure is not priced; no complete visibility guarantee, automatic unused finding or aggregate independent savings",
      "shared plugin overhead has a single context owner; skill and MCP links are attribution only",
    ],
  };
}
