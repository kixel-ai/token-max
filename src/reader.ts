import { ExposureCollector } from "./exposure.js";
import { open } from "node:fs/promises";
import { basename, isAbsolute } from "node:path";
import type {
  BillingPart,
  Location,
  Request,
  Scan,
  Snapshot,
  StructuralEvent,
  Usage,
} from "./model.js";
import {
  discover,
  discoverIdleOwners,
  discoverCodexSpeedOwners,
  discoverContextOwners,
  structuralSettings,
  type DiscoveryOptions,
} from "./discovery.js";
import {
  count,
  id,
  identity,
  label,
  object,
  ScanError,
  timestamp,
} from "./safe.js";

/** Select a time window and local roots, with optional cooperative cancellation. */
export interface ScanOptions extends DiscoveryOptions {
  since?: number;
}

function sum(values: (number | null)[]): number | null {
  if (values.some((v) => v === null)) return null;
  const result = values.reduce<number>((a, b) => a + b!, 0);
  if (!Number.isSafeInteger(result))
    throw new ScanError("token count sum exceeds safe integer range");
  return result;
}

/** Normalize usage without treating missing billing counters as zero. */
export function usage(value: unknown, harness: "claude" | "codex"): Usage {
  const u = object(value);
  const details = object(u.input_tokens_details);
  const creation = object(u.cache_creation);
  const writes = object(details.cache_write_tokens_details);
  const read = count(
    harness === "claude"
      ? u.cache_read_input_tokens
      : (u.cached_input_tokens ?? details.cached_tokens),
  );
  const write = count(
    harness === "claude"
      ? u.cache_creation_input_tokens
      : (u.cache_write_input_tokens ?? details.cache_write_tokens),
  );
  const write5m =
    harness === "claude" ? count(creation.ephemeral_5m_input_tokens) : null;
  const write1h =
    harness === "claude" ? count(creation.ephemeral_1h_input_tokens) : null;
  const write30m = harness === "codex" ? count(writes.ttl_30m_tokens) : null;
  const write12h = harness === "codex" ? count(writes.ttl_12h_tokens) : null;
  const input = count(u.input_tokens);
  const result: Usage = {
    input: harness === "claude" ? sum([input, read, write]) : input,
    output: count(u.output_tokens),
    read,
    write,
    write5m,
    write1h,
    write30m,
    write12h,
    reasoning: count(
      u.reasoning_output_tokens ??
        object(u.output_tokens_details).reasoning_tokens ??
        object(u.output_tokens_details).thinking_tokens,
    ),
  };
  if (result.input !== null && (read ?? 0) + (write ?? 0) > result.input)
    throw new ScanError("cache subsets exceed total input");
  if (result.output !== null && (result.reasoning ?? 0) > result.output)
    throw new ScanError("reasoning subset exceeds output");
  const split =
    harness === "claude" ? sum([write5m, write1h]) : sum([write30m, write12h]);
  if (split !== null && write !== null && split !== write)
    throw new ScanError("cache write lifetime counts disagree with total");
  return result;
}

function part(
  raw: unknown,
  model: string | null,
  kind: BillingPart["kind"],
): BillingPart {
  const u = object(raw);
  return {
    kind,
    model: label(u.model) ?? model,
    usage: usage(u, "claude"),
    tier: label(u.service_tier),
    speed: label(u.speed),
    geo: label(u.inference_geo),
  };
}

function claudeParts(raw: unknown, model: string | null): BillingPart[] {
  const u = object(raw);
  if (
    u.iterations === undefined ||
    u.iterations === null ||
    (Array.isArray(u.iterations) && u.iterations.length === 0)
  )
    return [part(u, model, "message")];
  if (!Array.isArray(u.iterations))
    throw new ScanError("invalid usage iterations");
  // Top-level totals overlap executor iterations and omit compaction/advisor usage.
  return u.iterations.map((value) => {
    const iteration = object(value);
    const kind =
      iteration.type === "compaction"
        ? "compaction"
        : iteration.type === "advisor_message"
          ? "advisor"
          : iteration.type === "message"
            ? "message"
            : "unknown";
    const p = part(value, model, kind);
    return {
      ...p,
      tier: p.tier ?? (kind === "advisor" ? null : label(u.service_tier)),
      speed: p.speed ?? (kind === "advisor" ? null : label(u.speed)),
      geo: p.geo ?? label(u.inference_geo),
    };
  });
}

interface Context {
  session: string;
  agent: Request["agent"];
  model: string | null;
  provider: string | null;
  client: string | null;
  version: string | null;
  turn: string | null;
  trigger: string | null;
  tier: string | null;
  source: string | null;
  cwd: string | null;
  pricing: StructuralEvent["pricing"];
}

function event(
  snapshot: Snapshot,
  location: Location,
  context: Context,
  time: number,
  kind: StructuralEvent["kind"],
  key: unknown,
): StructuralEvent {
  return {
    id: identity(
      `${snapshot.harness}:${kind}:${typeof key === "string" ? key : `${snapshot.file}:${location.line}`}`,
    ),
    origins: [{ session: context.session, location }],
    harness: snapshot.harness,
    session: context.session,
    time,
    kind,
    profiles: [...snapshot.profiles],
    location,
    request: null,
    model: context.model,
    version: context.version,
    client: context.client,
    cwd: context.cwd,
    trigger: null,
    precomputed: null,
    preTokens: null,
    postTokens: null,
    durationMs: null,
    tool: null,
    toolId: null,
    settings: {},
    idleAttribution: "unknown",
    pricing: context.pricing ? { ...context.pricing } : null,
  };
}

function request(
  snapshot: Snapshot,
  location: Location,
  context: Context,
  time: number,
  key: unknown,
): Request {
  return {
    evidence: "measured",
    id: `${snapshot.harness}:${id(key)}`,
    harness: snapshot.harness,
    session: context.session,
    agent: context.agent,
    time,
    endTime: time,
    model: context.model,
    provider: context.provider,
    api: snapshot.harness === "claude" ? "messages" : "responses",
    source: context.source,
    trigger: context.trigger,
    reference: null,
    turn: context.turn,
    version: context.version,
    client: context.client,
    cwd: context.cwd,
    parts: [],
    serverTools: null,
    profiles: [...snapshot.profiles],
    locations: [location],
    accounting: "individual",
    attempt: null,
    requestedSpeed: { tier: context.tier, speed: null },
    issues: [],
  };
}

function numericTools(raw: unknown): Record<string, number> | null {
  if (raw === undefined || raw === null) return null;
  if (typeof raw !== "object" || Array.isArray(raw))
    throw new ScanError("invalid server tool usage");
  const result: Record<string, number> = {};
  for (const [key, value] of Object.entries(object(raw))) {
    const name = label(key);
    if (name) {
      const requests = count(value);
      if (requests === null)
        throw new ScanError("missing server-tool request count");
      result[name] = requests;
    }
  }
  return result;
}

/** Read only newline-terminated records from a frozen byte boundary. */
export async function readSnapshot(
  snapshot: Snapshot,
  consume: (record: Record<string, unknown>, location: Location) => void,
  signal?: AbortSignal,
): Promise<void> {
  let handle;
  try {
    handle = await open(snapshot.file, "r");
  } catch {
    throw new ScanError("cannot open snapshotted transcript", {
      file: snapshot.file,
      line: 0,
    });
  }
  try {
    let info;
    try {
      info = await handle.stat();
    } catch {
      throw new ScanError("cannot inspect snapshotted transcript", {
        file: snapshot.file,
        line: 0,
      });
    }
    if (
      info.dev !== snapshot.device ||
      info.ino !== snapshot.inode ||
      info.size < snapshot.bytes
    )
      throw new ScanError(
        "transcript changed identity or shrank after snapshot",
        { file: snapshot.file, line: 0 },
      );
    let position = 0;
    let chunks: Buffer[] = [];
    let pending = 0;
    const buffer = Buffer.alloc(64 * 1024);
    while (position < snapshot.bytes) {
      signal?.throwIfAborted();
      let bytesRead: number;
      try {
        ({ bytesRead } = await handle.read(
          buffer,
          0,
          Math.min(buffer.length, snapshot.bytes - position),
          position,
        ));
      } catch {
        throw new ScanError("cannot read snapshotted transcript", {
          file: snapshot.file,
          line: snapshot.lines + 1,
        });
      }
      if (!bytesRead)
        throw new ScanError("transcript shrank during scan", {
          file: snapshot.file,
          line: snapshot.lines + 1,
        });
      position += bytesRead;
      let start = 0;
      for (let index = 0; index < bytesRead; index++) {
        if (buffer[index] !== 10) continue;
        chunks.push(Buffer.from(buffer.subarray(start, index)));
        snapshot.lines++;
        const location = { file: snapshot.file, line: snapshot.lines };
        let raw: unknown;
        try {
          const text = new TextDecoder("utf-8", { fatal: true }).decode(
            Buffer.concat(chunks),
          );
          raw = JSON.parse(text);
        } catch {
          throw new ScanError("invalid complete transcript record", location);
        }
        if (raw === null || typeof raw !== "object" || Array.isArray(raw))
          throw new ScanError("expected a transcript object", location);
        try {
          consume(object(raw), location);
        } catch (error) {
          if (error instanceof ScanError)
            throw new ScanError(error.message, location);
          throw error;
        }
        chunks = [];
        pending = 0;
        start = index + 1;
      }
      if (start < bytesRead) {
        const chunk = Buffer.from(buffer.subarray(start, bytesRead));
        chunks.push(chunk);
        pending += chunk.length;
      }
    }
    snapshot.deferredBytes = pending;
  } finally {
    await handle.close();
  }
}

/** Parse request and structural metadata, deduplicating streams and inherited history. */
export async function scan(options: ScanOptions = {}): Promise<Scan> {
  const snapshotStartedAt = new Date().toISOString();
  const discovery = await discover(options);
  const snapshotFinishedAt = new Date().toISOString();
  const exposure = new ExposureCollector();
  const requests = new Map<string, Request>();
  const events = new Map<string, StructuralEvent>();
  // Only explicit compaction lineage can connect a notice to its boundary.
  // Ordinary messages stop the walk; a missing parent is not a historical match.
  const compactParents = new Map<
    string,
    { parent: string | null; boundary: string | null } | null
  >();
  const noticeParents = new Map<string, Set<string | null>>();
  const refs = new Map<string, Request>();
  const issues = new Set<string>();
  const modernTotals = new Set<string>();
  const legacy: { request: Request; totalKey: string | null }[] = [];
  function addEvent(e: StructuralEvent) {
    const previous = events.get(e.id);
    if (previous) {
      previous.profiles = [...new Set([...previous.profiles, ...e.profiles])];
      previous.origins.push(...e.origins);
    } else events.set(e.id, e);
  }
  function add(r: Request) {
    const previous = requests.get(r.id);
    if (!previous) {
      requests.set(r.id, r);
      return;
    }
    previous.profiles = [...new Set([...previous.profiles, ...r.profiles])];
    previous.locations.push(...r.locations);
    previous.time = Math.min(previous.time, r.time);
    previous.endTime = Math.max(previous.endTime, r.endTime);
    // A streamed message's usage is a running snapshot, never an increment.
    const oldOutput = previous.parts.reduce(
      (n, p) => n + (p.usage.output ?? 0),
      0,
    );
    const newOutput = r.parts.reduce((n, p) => n + (p.usage.output ?? 0), 0);
    if (
      r.parts.length > previous.parts.length ||
      (r.parts.length === previous.parts.length &&
        newOutput >= oldOutput &&
        r.endTime >= previous.endTime)
    ) {
      for (let index = 0; index < r.parts.length; index++) {
        const p = r.parts[index]!;
        const old = previous.parts[index];
        if (!old || old.kind !== p.kind || old.model !== p.model) continue;
        for (const key of Object.keys(p.usage) as (keyof Usage)[]) {
          if (p.usage[key] === null) p.usage[key] = old.usage[key];
        }
      }
      previous.parts = r.parts;
      previous.evidence = r.evidence;
      previous.serverTools = r.serverTools ?? previous.serverTools;
    }
    previous.reference ??= r.reference;
    previous.source ??= r.source;
    previous.provider ??= r.provider;
    previous.issues = [...new Set([...previous.issues, ...r.issues])];
  }
  for (const snapshot of discovery.snapshots) {
    const isSubagent = /[/\\]subagents[/\\]/.test(snapshot.file);
    const context: Context = {
      session: identity(snapshot.file),
      agent:
        snapshot.harness === "claude"
          ? isSubagent
            ? "subagent"
            : "main"
          : "unknown",
      model: null,
      provider: null,
      client: null,
      version: null,
      turn: null,
      trigger: null,
      tier: null,
      source: null,
      cwd: null,
      pricing: null,
    };
    let previousTotal: Usage | null = null;
    const consume = (raw: Record<string, unknown>, location: Location) => {
      const time = timestamp(raw.timestamp);
      const selected =
        time === null || options.since === undefined || time >= options.since;
      try {
        exposure.observe({ raw, snapshot, location });
      } catch (error) {
        if (selected || !(error instanceof ScanError)) throw error;
        exposure.invalidate();
        issues.add(
          "invalid pre-window component metadata; subsequent exposure is incomplete",
        );
      }
      const p = object(raw.payload);
      if (snapshot.harness === "claude") {
        if (typeof raw.cwd === "string" && isAbsolute(raw.cwd))
          context.cwd = raw.cwd;
        if (raw.sessionId !== undefined)
          context.session = isSubagent
            ? identity(`${id(raw.sessionId)}:${basename(snapshot.file)}`)
            : id(raw.sessionId);
        context.version = label(raw.version) ?? context.version;
        context.client = label(raw.entrypoint) ?? context.client;
        const recordKey =
          typeof raw.uuid === "string"
            ? identity(`${context.session}:${id(raw.uuid)}`)
            : null;
        const parentKey =
          typeof raw.parentUuid === "string"
            ? identity(`${context.session}:${id(raw.parentUuid)}`)
            : null;
        if (
          recordKey &&
          (raw.isCompactSummary === true ||
            raw.type === "attachment" ||
            (raw.type === "system" && raw.subtype === "compact_boundary"))
        ) {
          const link = {
            parent: parentKey,
            boundary:
              raw.type === "system"
                ? identity(`${snapshot.harness}:compaction:${raw.uuid}`)
                : null,
          };
          if (
            compactParents.has(recordKey) &&
            JSON.stringify(compactParents.get(recordKey)) !==
              JSON.stringify(link)
          )
            compactParents.set(recordKey, null);
          else compactParents.set(recordKey, link);
        }
        if (raw.type === "api-request") {
          if (time === null) throw new ScanError("missing request timestamp");
          const recordedId = typeof raw.id === "string" && raw.id.length > 0;
          const key =
            !selected && !recordedId
              ? `${snapshot.file}:${location.line}`
              : raw.id;
          const ref = id(key);
          const pending = request(snapshot, location, context, time, key);
          pending.reference = recordedId ? ref : null;
          pending.agent =
            raw.isSidechain === true || isSubagent ? "subagent" : "main";
          pending.source = label(raw.querySource);
          const params = object(raw.params);
          pending.model = label(params.model);
          pending.attempt = { maxOutputTokens: null };
          try {
            pending.attempt.maxOutputTokens = count(params.max_tokens);
          } catch (error) {
            if (selected || !(error instanceof ScanError)) throw error;
            pending.issues.push(
              "pre-window output budget is invalid; no budget estimate retained",
            );
          }
          pending.requestedSpeed = {
            tier: label(params.service_tier),
            speed: label(params.speed),
          };
          pending.parts = [
            part(
              {
                // Attempt parameters are requested, not proof of the billed tier/speed.
                inference_geo: params.inference_geo,
              },
              pending.model,
              "message",
            ),
          ];
          pending.evidence = "unknown";
          pending.issues.push(
            "api request has no linked assistant usage; failed/in-flight/hidden request billing is unknown",
          );
          const prior = refs.get(ref);
          if (prior)
            pending.profiles = [
              ...new Set([...prior.profiles, ...pending.profiles]),
            ];
          refs.set(ref, pending);
          return;
        }
        // Structural pricing context is needed for later boundaries even outside --since.
        if (raw.type === "assistant" && raw.isApiErrorMessage !== true) {
          const m = object(raw.message);
          const u = object(m.usage);
          if (m.model !== "<synthetic>" && label(m.model)) {
            context.model = label(m.model);
            context.pricing = {
              model: context.model,
              provider: context.provider,
              tier: label(u.service_tier),
              speed: label(u.speed),
              geo: label(u.inference_geo),
            };
          }
        }
        if (
          !selected &&
          raw.type !== "assistant" &&
          !(raw.type === "system" && raw.subtype === "compact_boundary")
        )
          return;
        if (raw.type === "assistant") {
          const m = object(raw.message);
          if (raw.isApiErrorMessage === true || m.model === "<synthetic>")
            return;
          if (time === null) throw new ScanError("missing request timestamp");
          if (selected && Object.keys(m).length === 0)
            throw new ScanError("missing assistant message");
          const key = m.id ?? raw.requestId ?? raw.requestRef;
          const r = request(
            snapshot,
            location,
            context,
            time,
            !selected && !(typeof key === "string" && key.length > 0)
              ? `${snapshot.file}:${location.line}`
              : key,
          );
          r.model = label(m.model);
          r.reference =
            typeof raw.requestRef === "string" ? id(raw.requestRef) : null;
          r.source = label(raw.querySource);
          r.agent =
            raw.isSidechain === true || isSubagent ? "subagent" : "main";
          try {
            r.parts = claudeParts(m.usage, r.model);
          } catch (error) {
            if (selected || !(error instanceof ScanError)) throw error;
            r.parts = [part({}, null, "unknown")];
            r.evidence = "unknown";
            r.issues.push(
              "pre-window usage is invalid; cache predecessor remains unknown",
            );
          }
          r.serverTools = selected
            ? numericTools(object(m.usage).server_tool_use)
            : null;
          if (!m.usage) {
            r.evidence = "unknown";
            r.issues.push("request usage is absent");
          }
          add(r);
          if (selected && Array.isArray(m.content))
            for (const block of m.content) {
              const b = object(block);
              if (b.type === "tool_use") {
                const e = event(
                  snapshot,
                  location,
                  context,
                  time,
                  "tool-start",
                  b.id,
                );
                e.tool = label(b.name);
                e.toolId = typeof b.id === "string" ? id(b.id) : null;
                e.request = r.id;
                addEvent(e);
              }
            }
        } else if (
          raw.type === "system" &&
          raw.subtype === "compact_boundary"
        ) {
          if (time === null)
            throw new ScanError("missing compaction timestamp");
          const meta = object(raw.compactMetadata);
          const e = event(
            snapshot,
            location,
            context,
            time,
            "compaction",
            raw.uuid,
          );
          e.trigger =
            meta.trigger === "auto" || meta.trigger === "manual"
              ? meta.trigger
              : null;
          e.precomputed =
            typeof meta.precomputed === "boolean" ? meta.precomputed : null;
          e.preTokens = selected ? count(meta.preTokens) : null;
          e.postTokens = selected ? count(meta.postTokens) : null;
          e.durationMs = selected ? count(meta.durationMs) : null;
          e.request =
            typeof raw.requestRef === "string"
              ? `ref:${id(raw.requestRef)}`
              : typeof meta.requestId === "string"
                ? `claude:${id(meta.requestId)}`
                : null;
          addEvent(e);
        } else if (
          raw.type === "system" &&
          raw.subtype === "informational" &&
          raw.level === "notice" &&
          typeof raw.content === "string" &&
          /^(Compacted while idle, before the prompt cache expired|Compacted using a summary written while you were away) ·(?:\s|\u00a0)*turn off with "idleCompaction": false in settings\.json$/.test(
            raw.content,
          )
        ) {
          if (time === null)
            throw new ScanError("missing idle notice timestamp");
          const e = event(
            snapshot,
            location,
            context,
            time,
            "idle-notice",
            raw.uuid,
          );
          e.idleAttribution = "notice";
          addEvent(e);
          const parents = noticeParents.get(e.id) ?? new Set<string | null>();
          parents.add(parentKey);
          noticeParents.set(e.id, parents);
        } else if (raw.type === "user") {
          if (time === null) return;
          const m = object(raw.message);
          let toolResult = false;
          if (Array.isArray(m.content))
            for (const block of m.content) {
              const b = object(block);
              if (b.type === "tool_result") {
                const e = event(
                  snapshot,
                  location,
                  context,
                  time,
                  "tool-end",
                  b.tool_use_id,
                );
                e.toolId =
                  typeof b.tool_use_id === "string" ? id(b.tool_use_id) : null;
                addEvent(e);
                toolResult = true;
              }
            }
          if (!toolResult && raw.isMeta !== true)
            addEvent(
              event(snapshot, location, context, time, "turn-start", raw.uuid),
            );
        } else if (raw.type === "system" && raw.subtype === "turn_duration") {
          if (time !== null) {
            const e = event(
              snapshot,
              location,
              context,
              time,
              "turn-end",
              raw.uuid,
            );
            e.durationMs = count(raw.durationMs);
            addEvent(e);
          }
        }
        return;
      }
      if (raw.type === "session_meta") {
        if (typeof p.cwd === "string" && isAbsolute(p.cwd)) context.cwd = p.cwd;
        context.session = id(p.id ?? p.session_id);
        context.provider = label(p.model_provider);
        context.client = label(p.originator) ?? label(p.source);
        context.source =
          label(p.source) ?? (object(p.source).subagent ? "subagent" : null);
        context.version = label(p.cli_version);
        context.agent =
          typeof p.source === "object" && object(p.source).subagent
            ? "subagent"
            : typeof p.source === "string"
              ? "main"
              : "unknown";
        if (time !== null && selected)
          addEvent(
            event(snapshot, location, context, time, "client", context.session),
          );
      } else if (raw.type === "turn_context") {
        if (typeof p.cwd === "string" && isAbsolute(p.cwd)) context.cwd = p.cwd;
        context.model = label(p.model);
        context.turn = typeof p.turn_id === "string" ? id(p.turn_id) : null;
        context.tier = label(p.service_tier);
        if (time !== null && selected) {
          const e = event(
            snapshot,
            location,
            context,
            time,
            "settings",
            p.turn_id,
          );
          e.settings = structuralSettings(p);
          addEvent(e);
        }
      } else if (raw.type === "token_usage_record") {
        if (!selected) return;
        if (time === null) throw new ScanError("missing request timestamp");
        const r = request(snapshot, location, context, time, p.response_id);
        r.turn = typeof p.turn_id === "string" ? id(p.turn_id) : context.turn;
        r.parts = [
          {
            kind: "message",
            model: r.model,
            usage: usage(p.usage, "codex"),
            // Native TokenUsageRecord has no billed service-tier field.
            tier: null,
            speed: null,
            geo: null,
          },
        ];
        const total = object(p.thread_token_usage);
        if (Object.keys(total).length)
          modernTotals.add(
            `${context.session}:${JSON.stringify(usage(total, "codex"))}`,
          );
        add(r);
      } else if (raw.type === "event_msg" && p.type === "token_count") {
        const info = object(p.info);
        if (Object.keys(info).length === 0) return;
        let total: Usage;
        try {
          total = usage(info.total_token_usage, "codex");
        } catch (error) {
          if (!selected) {
            previousTotal = null;
            return;
          }
          throw error;
        }
        const totalKey = `${context.session}:${JSON.stringify(total)}`;
        const repeated =
          previousTotal !== null &&
          JSON.stringify(total) === JSON.stringify(previousTotal);
        let u: Usage | null = null;
        if (!selected) {
          previousTotal = total;
          return;
        }
        if (Object.keys(object(info.last_token_usage)).length)
          u = usage(info.last_token_usage, "codex");
        else if (previousTotal) {
          u = { ...total };
          for (const key of Object.keys(u) as (keyof Usage)[]) {
            u[key] =
              total[key] === null || previousTotal[key] === null
                ? null
                : total[key]! - previousTotal[key]!;
          }
          if (Object.values(u).some((v) => v !== null && v < 0)) u = null;
        }
        previousTotal = total;
        if (repeated) return;
        if (time === null) throw new ScanError("missing usage timestamp");
        const r = request(
          snapshot,
          location,
          context,
          time,
          `${context.turn ?? context.session}:${time}:${JSON.stringify(total)}`,
        );
        r.accounting = info.last_token_usage ? "legacy-last" : "legacy-delta";
        r.evidence = info.last_token_usage ? "measured" : "estimated";
        if (!u) {
          u = usage({}, "codex");
          r.evidence = "unknown";
          r.issues.push(
            "cumulative usage has no verified request delta after start/reset",
          );
        }
        r.parts = [
          {
            kind: "message",
            model: r.model,
            usage: u,
            tier: null,
            speed: null,
            geo: null,
          },
        ];
        legacy.push({ request: r, totalKey });
      } else if (raw.type === "compacted" && selected) {
        if (time === null) throw new ScanError("missing compaction timestamp");
        const e = event(
          snapshot,
          location,
          context,
          time,
          "compaction",
          p.compaction_response_id ??
            p.window_id ??
            `${context.session}:${time}`,
        );
        e.request =
          typeof p.compaction_response_id === "string"
            ? `codex:${id(p.compaction_response_id)}`
            : null;
        addEvent(e);
      } else if (
        raw.type === "event_msg" &&
        ["task_started", "task_complete", "thread_settings_applied"].includes(
          String(p.type),
        )
      ) {
        if (p.type === "task_started") {
          context.turn =
            typeof p.turn_id === "string" ? id(p.turn_id) : context.turn;
          context.trigger =
            label(p.turn_trigger) ??
            label(object(p.turn_attribution).turn_trigger);
        }
        const applied = object(p.thread_settings);
        if (p.type === "thread_settings_applied") {
          context.tier =
            label(applied.service_tier) ??
            label(p.service_tier) ??
            context.tier;
          context.model = label(applied.model) ?? context.model;
          context.provider =
            label(applied.model_provider_id) ?? context.provider;
        }
        if (time !== null && selected) {
          const e = event(
            snapshot,
            location,
            context,
            time,
            p.type === "task_started"
              ? "turn-start"
              : p.type === "task_complete"
                ? "turn-end"
                : "settings",
            p.turn_id ?? `${context.session}:${time}`,
          );
          e.settings = structuralSettings(
            p.type === "thread_settings_applied" ? applied : p,
          );
          addEvent(e);
        }
      } else if (raw.type === "response_item" && selected && time !== null) {
        if (
          [
            "function_call",
            "custom_tool_call",
            "function_call_output",
            "custom_tool_call_output",
          ].includes(String(p.type))
        ) {
          const e = event(
            snapshot,
            location,
            context,
            time,
            String(p.type).endsWith("output") ? "tool-end" : "tool-start",
            p.call_id,
          );
          e.tool = label(p.name);
          e.toolId = typeof p.call_id === "string" ? id(p.call_id) : null;
          addEvent(e);
        }
      }
    };
    await readSnapshot(snapshot, consume, options.signal);
  }
  for (const { request: r, totalKey } of legacy)
    if (!totalKey || !modernTotals.has(totalKey)) add(r);
  const refToRequest = new Map<string, string>();
  const linkedRefs = new Set(
    [...requests.values()].flatMap((r) => (r.reference ? [r.reference] : [])),
  );
  for (const [ref, r] of refs) if (!linkedRefs.has(ref)) add(r);
  for (const r of requests.values()) {
    if (r.reference) {
      refToRequest.set(r.reference, r.id);
      const ref = refs.get(r.reference);
      if (ref) {
        r.source ??= ref.source;
        r.attempt = ref.attempt;
        if (ref.requestedSpeed) r.requestedSpeed = ref.requestedSpeed;
        r.time = Math.min(r.time, ref.time);
        r.profiles = [...new Set([...r.profiles, ...ref.profiles])];
      }
    }
    if (r.source?.startsWith("agent:")) r.agent = "subagent";
    if (r.source === "compact" || r.source === "compaction")
      for (const p of r.parts) p.kind = "compaction";
    if (r.provider === null) {
      const current = discovery.profiles.filter((p) =>
        r.profiles.includes(p.id),
      );
      const providers = new Set(current.map((p) => p.provider));
      if (providers.size === 1 && !providers.has(null)) {
        r.provider = current[0]!.provider;
        r.issues.push(
          "provider inferred from current profile settings applied to history",
        );
      }
    }
    if (r.harness === "claude" && r.provider === null) {
      const profiles = discovery.profiles.filter((p) =>
        r.profiles.includes(p.id),
      );
      if (profiles.every((p) => p.provider === null)) {
        r.provider = "anthropic";
        r.issues.push(
          "public Anthropic API equivalent; historical endpoint not recorded",
        );
      } else
        r.issues.push(
          "shared history has conflicting current provider conditions; historical endpoint unknown",
        );
    }
  }
  for (const e of events.values()) {
    if (e.request?.startsWith("ref:"))
      e.request = refToRequest.get(e.request.slice(4)) ?? null;
    if (e.harness === "claude" && e.pricing && e.pricing.provider === null) {
      const current = discovery.profiles.filter((p) =>
        e.profiles.includes(p.id),
      );
      const providers = new Set(current.map((p) => p.provider));
      if (providers.size === 1)
        e.pricing.provider = current[0]!.provider ?? "anthropic";
    }
  }
  const allRequests = [...requests.values()].sort(
    (a, b) => a.time - b.time || a.id.localeCompare(b.id),
  );
  const allEvents = [...events.values()].sort((a, b) => a.time - b.time);
  for (const notice of allEvents.filter((e) => e.kind === "idle-notice")) {
    const parents = noticeParents.get(notice.id);
    let key = parents?.size === 1 ? [...parents][0] : null;
    const visited = new Set<string>();
    while (key && !visited.has(key)) {
      visited.add(key);
      const link = compactParents.get(key);
      if (!link) break;
      if (link.boundary) {
        const boundary = events.get(link.boundary);
        if (
          boundary &&
          boundary.trigger === "auto" &&
          boundary.time <= notice.time &&
          boundary.idleAttribution !== "notice" &&
          boundary.origins.some((o) =>
            notice.origins.some((n) => n.session === o.session),
          )
        )
          boundary.idleAttribution = "notice";
        break;
      }
      key = link.parent;
    }
  }
  const beforeWindow = (value: { time: number }) =>
    options.since !== undefined && value.time < options.since;
  const sortedRequests = allRequests.filter((r) => !beforeWindow(r));
  const sortedEvents = allEvents.filter((e) => !beforeWindow(e));
  await discoverIdleOwners(options, discovery.profiles, allRequests);
  await discoverCodexSpeedOwners(options, discovery.profiles, allRequests);
  await discoverContextOwners(options, discovery.profiles, allRequests);
  for (const r of sortedRequests)
    for (const issue of r.issues) issues.add(issue);
  if (sortedEvents.some((e) => e.kind === "compaction"))
    issues.add(
      "only native informational idle notices attribute applied idle compaction; precomputed alone and final missing boundaries do not establish summary generation or absence",
    );
  return {
    ...discovery,
    snapshotStartedAt,
    snapshotFinishedAt,
    since: options.since ?? null,
    requests: sortedRequests,
    events: sortedEvents,
    exposure: exposure.finish(sortedRequests, options.since ?? null),
    cacheContext: {
      requests: allRequests.filter(beforeWindow),
      boundaries: allEvents.filter(
        (e) => beforeWindow(e) && e.kind === "compaction",
      ),
    },
    coverage: {
      requests: sortedRequests.length,
      sourceKnown: sortedRequests.filter(
        (r) => r.source !== null || r.trigger !== null,
      ).length,
      tierKnown: sortedRequests.filter((r) =>
        r.parts.every((p) => p.tier !== null),
      ).length,
      missingUsage: sortedRequests.filter((r) =>
        r.parts.some((p) => p.usage.input === null || p.usage.output === null),
      ).length,
      deferredFiles: discovery.snapshots.filter((s) => s.deferredBytes > 0)
        .length,
      issues: [...issues],
    },
  };
}
