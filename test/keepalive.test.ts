import { test } from "node:test";
import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import type { Profile, Request, Scan } from "../src/model.js";
import { compactionControls, idleControls } from "../src/controls.js";
import { keepaliveChecks } from "../src/keepalive.js";
import { compactionCharges, analyze } from "../src/replay.js";
import { scan, usage } from "../src/reader.js";
import { formatReport } from "../src/report.js";
import { claudeUsage, makeRequest, prices, workspace } from "./helpers.js";
const M = 60_000;
function request(
  id: string,
  minute: number,
  overrides: Partial<Request> = {},
): Request {
  const r = makeRequest({
    id,
    time: minute * M,
    endTime: minute * M,
    ...overrides,
  });
  r.parts[0]!.usage = claudeUsage({
    write: 100000,
    output: 1,
    ttl: "1h",
    input: 1,
  });
  return r;
}
function data(requests: Request[]): Scan {
  const config = {
    promptCacheTtl: "1h",
    autoCompactWindow: 400000,
    idleCompaction: false,
  };
  const profile: Profile = {
    id: "profile",
    harness: requests[0]!.harness,
    root: "synthetic",
    aliases: [],
    histories: [],
    settingsFile: "settings.json",
    provider: requests[0]!.provider,
    settings: config,
    ttl: {
      main: {
        value: "1h",
        owner: "settings.json:promptCacheTtl",
        reason: "explicit",
      },
      subagent: {
        value: "5m",
        owner: "settings.json:subagentPromptCacheTtl",
        reason: "explicit",
      },
    },
    compaction: compactionControls({
      harness: requests[0]!.harness,
      user: config,
      managed: {},
      env: {},
      file: "settings.json",
      managedFile: "managed.json",
    }),
    idle: idleControls({
      scopes: [{ config, file: "settings.json" }],
      env: {},
    }),
  };
  return {
    snapshotStartedAt: new Date(0).toISOString(),
    snapshotFinishedAt: new Date(0).toISOString(),
    since: null,
    roots: [],
    profiles: [profile],
    snapshots: [],
    requests,
    events: [],
    cacheContext: { requests: [], boundaries: [] },
    exposure: { components: [], requests: [], invocations: [], visibility: [] },
    coverage: {
      requests: requests.length,
      sourceKnown: requests.length,
      tierKnown: requests.length,
      missingUsage: 0,
      deferredFiles: 0,
      issues: [],
    },
  };
}
async function check(s: Scan, now = 200) {
  const catalog = await prices();
  return keepaliveChecks({
    scan: s,
    catalog,
    summaries: compactionCharges(s, catalog),
    estimates: new Map(),
    ttls: [],
    windows: [],
    since: undefined,
    now: now * M,
  })[0]!;
}
function liveness(s: Scan, minute: number, state: "closed" | "asleep") {
  s.events.push({
    id: "lifecycle",
    origins: [{ session: "session", location: { file: "synthetic", line: 1 } }],
    harness: "claude",
    session: "session",
    time: minute * M,
    kind: "client",
    profiles: ["profile"],
    location: { file: "synthetic", line: 1 },
    request: null,
    model: null,
    version: null,
    client: null,
    cwd: null,
    trigger: null,
    precomputed: null,
    preTokens: null,
    postTokens: null,
    durationMs: null,
    tool: null,
    toolId: null,
    settings: {},
    idleAttribution: "unknown",
    pricing: null,
    liveness: state,
  });
}
test("Claude prices a winning cap, charges full final periods and keeps unknown current state advisory", async () => {
  const s = data([request("a", 0), request("b", 100)]);
  const c = await check(s, 101);
  assert.equal(c.action, "advisory");
  assert.equal(c.current, null);
  assert.equal(c.recommended, "ping:3300000ms");
  const cap = c.scenarios.find((s) => s.capMs !== null)!;
  assert.equal(cap.pings, 2);
  assert.equal(cap.finalPings, 1);
  assert.ok(cap.pingCost.totalUsd! > 0);
  assert.ok(cap.savingsUsd! > 0);
  assert.equal(cap.sensitivity.closedSavingsUsd, 0);
  assert.match(c.uncertainty.join(" "), /no records does not mean off/);
  const report = analyze({ scan: s, catalog: await prices(), now: 101 * M });
  assert.match(formatReport(report, { details: true }), /Keepalive comparison/);
});
test("known supported current policy can start, change, keep, and stop pings", async () => {
  const s = data([request("a", 0), request("b", 100)]);
  const p = s.profiles[0]!;
  p.keepalive = {
    policy: "no-keepalive",
    mechanism: "claude-asyncRewake",
    owner: "observed-hook:cap",
    resolved: true,
    supported: true,
    intervalMs: 55 * M,
    capMs: null,
    reason: "verified host-adapter control",
  };
  assert.equal((await check(s)).action, "change");
  p.keepalive.policy = "ping:3300000ms";
  p.keepalive.capMs = 55 * M;
  assert.equal((await check(s)).action, "keep");
  p.keepalive.policy = "ping:6600000ms";
  p.keepalive.capMs = 110 * M;
  assert.equal((await check(s)).action, "change");
  const costly = data([request("a", 0), request("b", 100)]);
  for (const r of costly.requests) r.parts[0]!.usage.output = 100000;
  costly.profiles[0]!.keepalive = { ...p.keepalive };
  costly.profiles[0]!.compaction.default.value = 1000000;
  const stopped = await check(costly);
  assert.equal(stopped.recommended, "no-keepalive");
  assert.equal(stopped.action, "change");
  assert.ok(stopped.savingsUsd! > 0);
  costly.profiles[0]!.keepalive!.mechanism = "unknown";
  assert.equal((await check(costly)).action, "advisory");
});
test("a long current cap is charged even when the observed final gap is shorter", async () => {
  const s = data([request("a", 0), request("b", 100)]);
  s.profiles[0]!.keepalive = {
    policy: "ping:86400000ms",
    mechanism: "claude-asyncRewake",
    owner: "hook",
    resolved: true,
    supported: true,
    intervalMs: 55 * M,
    capMs: 24 * 60 * M,
    reason: "observed",
  };
  const c = await check(s, 101);
  const cap = c.scenarios.find((s) => s.capMs === 24 * 60 * M)!;
  assert.equal(cap.finalPings, 26);
  assert.equal(cap.pings, 27);
  assert.ok(
    cap.savingsUsd! < c.scenarios.find((s) => s.capMs === 55 * M)!.savingsUsd!,
  );
});
test("recorded closure/sleep ends pings without stretching an idle activation fraction", async () => {
  for (const state of ["closed", "asleep"] as const) {
    const s = data([request("a", 0), request("b", 100)]);
    liveness(s, 20, state);
    const c = await check(s, 101);
    const cap = c.scenarios.find((s) => s.capMs !== null)!;
    assert.equal(cap.pings, 1);
    assert.match(c.periods[0]!.lifetimeBasis, /recorded/);
  }
});
test("explicit ping attribution removes both its charge and its warmth from the no-ping path", async () => {
  const a = request("a", 0),
    ping = request("p", 55),
    b = request("b", 100);
  ping.keepalive = { mechanism: "claude-loop", task: "task", attributed: true };
  ping.parts[0]!.usage = claudeUsage({
    read: 100000,
    write: 2,
    output: 1,
    ttl: "1h",
    input: 1,
  });
  b.parts[0]!.usage = claudeUsage({
    read: 100004,
    write: 2,
    output: 1,
    ttl: "1h",
    input: 1,
  });
  const c = await check(data([a, ping, b]));
  assert.equal(c.recordedPings, 1);
  assert.ok(c.recordedPingCost.totalUsd! > 0);
  const base = c.scenarios[0]!;
  assert.ok(base.comparisonCost.totalUsd! > 1.5);
  assert.equal(c.periods.length, 2);
  ping.keepalive.attributed = false;
  const ambiguous = await check(data([a, ping, b]));
  assert.equal(ambiguous.recommended, null);
  assert.equal(ambiguous.evidence, "unknown");
});
test("Codex includes returns beyond 30 minutes at actual write and read prices but stays advisory", async () => {
  const rs = [
    request("a", 0, {
      harness: "codex",
      provider: "openai",
      model: "gpt-6.1-sol",
      api: "responses",
    }),
    request("b", 70, {
      harness: "codex",
      provider: "openai",
      model: "gpt-6.1-sol",
      api: "responses",
    }),
  ];
  for (const r of rs) {
    r.parts[0]!.model = "gpt-6.1-sol";
    r.parts[0]!.usage = usage(
      {
        input_tokens: 100001,
        output_tokens: 1,
        input_tokens_details: {
          cached_tokens: 0,
          cache_write_tokens: 100000,
          cache_write_tokens_details: {
            ttl_30m_tokens: 100000,
            ttl_12h_tokens: 0,
          },
        },
      },
      "codex",
    );
  }
  const c = await check(data(rs), 71);
  assert.equal(c.action, "advisory");
  assert.ok(c.scenarios.some((s) => s.capMs !== null && s.savingsUsd! > 0));
  assert.equal(c.periods[0]!.intervalMs, 29.5 * M);
  assert.ok(c.scenarios.every((s) => !s.idle));
  rs[0]!.parts[0]!.tier = null;
  assert.equal((await check(data(rs))).recommended, null);
});
test("a 12-hour Codex write is not rewritten on 30-minute reads", async () => {
  const rs = [
    request("a", 0, {
      harness: "codex",
      provider: "openai",
      model: "gpt-6.1-sol",
      api: "responses",
    }),
    request("b", 70, {
      harness: "codex",
      provider: "openai",
      model: "gpt-6.1-sol",
      api: "responses",
    }),
  ];
  for (const r of rs) {
    r.parts[0]!.model = "gpt-6.1-sol";
    r.parts[0]!.usage = usage(
      {
        input_tokens: 100000,
        output_tokens: 1,
        input_tokens_details: {
          cached_tokens: r.id === "a" ? 0 : 100000,
          cache_write_tokens: r.id === "a" ? 100000 : 0,
          cache_write_tokens_details: {
            ttl_30m_tokens: 0,
            ttl_12h_tokens: r.id === "a" ? 100000 : 0,
          },
        },
      },
      "codex",
    );
  }
  const c = await check(data(rs));
  assert.equal(c.recommended, "no-keepalive");
  assert.ok(c.scenarios.find((s) => s.capMs !== null)!.savingsUsd! < 0);
});
test("native task IDs need an explicit cache-only declaration; text never attributes pings", async () => {
  const w = await workspace();
  try {
    const file = join(w.claude, "projects/project/session.jsonl");
    const rows = [
      {
        type: "user",
        uuid: "u",
        sessionId: "s",
        timestamp: "2026-11-01T00:00:00Z",
        scheduledTaskId: "job",
        message: { content: "arbitrary private content" },
      },
      {
        type: "assistant",
        uuid: "a",
        parentUuid: "u",
        sessionId: "s",
        timestamp: "2026-11-01T00:00:01Z",
        message: {
          id: "a",
          model: "claude-opus-5-5",
          usage: {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_input_tokens: 1000,
            cache_creation_input_tokens: 0,
          },
        },
      },
      {
        type: "user",
        uuid: "u2",
        sessionId: "s",
        timestamp: "2026-11-01T01:00:00Z",
        message: { content: "keepalive ping" },
      },
      {
        type: "assistant",
        uuid: "b",
        parentUuid: "u2",
        sessionId: "s",
        timestamp: "2026-11-01T01:00:01Z",
        message: {
          id: "b",
          model: "claude-opus-5-5",
          usage: {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_input_tokens: 1000,
            cache_creation_input_tokens: 0,
          },
        },
      },
    ];
    await writeFile(file, rows.map((r) => JSON.stringify(r)).join("\n") + "\n");
    const normal = await scan(w.options);
    assert.equal(normal.requests[0]!.keepalive?.attributed, false);
    assert.equal(normal.requests[1]!.keepalive, undefined);
    const declared = await scan({ ...w.options, keepaliveTasks: ["job"] });
    assert.equal(declared.requests[0]!.keepalive?.attributed, true);
    assert.ok(!JSON.stringify(declared).includes("arbitrary private content"));
  } finally {
    await w.dispose();
  }
});

function calibrated(): Scan {
  const a = request("a", 0),
    b = request("b", 100);
  for (const r of [a, b]) {
    r.parts[0]!.usage = claudeUsage({
      write: 300000,
      output: 1,
      ttl: "1h",
      input: 1,
    });
    r.client = "cli";
    r.source = "repl_main_thread";
  }
  const s = data([a, b]);
  const summary = request("sample-summary", 1, {
    session: "sample",
    source: "compact",
  });
  summary.parts[0]!.kind = "compaction";
  summary.parts[0]!.usage = claudeUsage({
    read: 300000,
    write: 0,
    output: 2000,
  });
  const resumed = request("sample-resume", 3, {
    session: "sample",
    client: "cli",
    source: "repl_main_thread",
  });
  resumed.parts[0]!.usage = claudeUsage({
    read: 5000,
    write: 15000,
    output: 1,
    ttl: "1h",
  });
  s.requests.push(summary, resumed);
  s.requests.sort((a, b) => a.time - b.time);
  s.events.push({
    id: "calibration",
    origins: [{ session: "sample", location: { file: "synthetic", line: 1 } }],
    harness: "claude",
    session: "sample",
    time: 2 * M,
    kind: "compaction",
    profiles: ["profile"],
    location: { file: "synthetic", line: 1 },
    request: summary.id,
    model: summary.model,
    version: summary.version,
    client: "cli",
    cwd: null,
    trigger: "manual",
    precomputed: false,
    preTokens: 300000,
    postTokens: 2000,
    durationMs: 1000,
    tool: null,
    toolId: null,
    settings: {},
    idleAttribution: "unknown",
    pricing: {
      model: summary.model,
      provider: summary.provider,
      tier: "standard",
      speed: "standard",
      geo: "global",
    },
  });
  return s;
}
test("idle summaries and pings compete in the same replay, including the near-expiry trigger", async () => {
  const s = calibrated();
  const c = await check(s, 101);
  const none = c.scenarios.find((s) => s.policy === "no-keepalive")!;
  const idle = c.scenarios.find((s) => s.policy === "idle-compaction")!;
  const ping = c.scenarios.find((s) => s.capMs !== null && !s.idle)!;
  const joint = c.scenarios.find((s) => s.capMs === ping.capMs && s.idle)!;
  assert.ok(idle);
  assert.notEqual(joint.comparisonCost.totalUsd, null);
  assert.ok(idle.savingsUsd! > 0);
  const independentlyAdded =
    idle.comparisonCost.totalUsd! +
    ping.comparisonCost.totalUsd! -
    none.comparisonCost.totalUsd!;
  assert.notEqual(joint.comparisonCost.totalUsd, independentlyAdded);
  assert.notEqual(joint.pingCost.totalUsd, ping.pingCost.totalUsd);
  assert.ok(c.scenarios.every((s) => s.comparisonCost.totalUsd! >= 0));
});
test("an attributed applied idle boundary is undone before adding hypothetical pings", async () => {
  const s = calibrated();
  const summary = request("actual-summary", 54, {
    session: "session",
    source: "compact",
  });
  summary.parts[0]!.kind = "compaction";
  summary.parts[0]!.usage = claudeUsage({
    read: 300000,
    write: 0,
    output: 2000,
  });
  const b = s.requests.find((r) => r.id === "b")!;
  b.parts[0]!.usage = claudeUsage({
    read: 5000,
    write: 15000,
    output: 1,
    ttl: "1h",
  });
  const template = s.events[0]!;
  s.events.push({
    ...template,
    id: "actual-boundary",
    session: "session",
    origins: [{ session: "session", location: { file: "synthetic", line: 2 } }],
    time: 55 * M,
    request: summary.id,
    idleAttribution: "notice",
    trigger: "auto",
    precomputed: true,
    preTokens: 300002,
  });
  s.requests.push(summary);
  s.requests.sort((a, b) => a.time - b.time);
  const c = await check(s, 101);
  assert.notEqual(c.scenarios[0]!.comparisonCost.totalUsd, null);
  assert.ok(c.scenarios[0]!.comparisonCost.totalUsd! > 4);
  assert.ok(
    c.scenarios.some((s) => s.idle && s.comparisonCost.totalUsd !== null),
  );
});
test("the aggregate estimate can win while closure sensitivity ties or loses", async () => {
  const s = data([request("a", 0), request("b", 100)]);
  const c = await check(s);
  assert.notEqual(c.recommended, "no-keepalive");
  const best = c.scenarios.find((s) => s.policy === c.recommended)!;
  assert.equal(best.sensitivity.closedSavingsUsd, 0);
  assert.ok(best.savingsUsd! > 0);
});
test("an unsupported model cache policy and missing billed tier remain unpriced", async () => {
  const r = request("a", 0, {
    harness: "codex",
    provider: "openai",
    model: "gpt-5.4",
    api: "responses",
  });
  r.parts[0]!.model = "gpt-5.4";
  const c = await check(data([r]));
  assert.equal(c.recommended, null);
  assert.equal(c.action, "advisory");
});

test("a candidate mechanism without verified capped support stays advisory", async () => {
  const s = data([request("a", 0), request("b", 100)]);
  s.profiles[0]!.keepalive = {
    policy: "no-keepalive",
    mechanism: "claude-loop",
    supported: false,
    resolved: true,
    owner: "CronCreate",
    intervalMs: null,
    capMs: null,
    reason: "no verified cap controller",
  };
  assert.equal((await check(s)).action, "advisory");
});
test("a faster observed cadence is priced separately from the modeled candidate", async () => {
  const s = data([request("a", 0), request("b", 100)]);
  s.profiles[0]!.keepalive = {
    policy: "ping:3300000ms",
    mechanism: "claude-asyncRewake",
    supported: true,
    resolved: true,
    owner: "observed-hook",
    intervalMs: 5 * M,
    capMs: 55 * M,
    reason: "verified",
  };
  const c = await check(s);
  assert.equal(c.current, "ping:3300000ms@300000ms");
  assert.equal(c.action, "change");
  assert.ok(
    c.scenarios.find((p) => p.policy === c.current)!.pings >
      c.scenarios.find((p) => p.policy === c.recommended)!.pings,
  );
});
test("long-cap unreturned lifetime has a priced break-even and an extra-work budget", async () => {
  const s = data([request("a", 0), request("b", 100)]);
  s.profiles[0]!.keepalive = {
    policy: "ping:172800000ms",
    mechanism: "claude-asyncRewake",
    supported: true,
    resolved: true,
    owner: "hook",
    intervalMs: 55 * M,
    capMs: 48 * 60 * M,
    reason: "verified",
  };
  const c = await check(s, 101),
    long = c.scenarios.find((p) => p.capMs === 48 * 60 * M)!;
  assert.ok(long.savingsUsd! < 0);
  assert.ok(long.sensitivity.breakEvenOpenMs! > 0);
  assert.ok(
    c.scenarios.find((p) => p.policy === c.recommended)!.sensitivity
      .extraWorkBreakEvenUsdPerPing! > 0,
  );
});
test("observed misses inside a TTL are preserved in the no-ping alternative", async () => {
  const s = data([request("a", 0), request("b", 10), request("c", 100)]);
  const c = await check(s, 101);
  // All three recorded writes remain billed without hypothetical pings.
  assert.ok(c.scenarios[0]!.comparisonCost.totalUsd! > 2.4);
});
test("known request duration cannot permit a ping before generation finishes", async () => {
  const s = data([request("a", 0, { endTime: 70 * M }), request("b", 100)]);
  const c = await check(s, 101),
    cap = c.scenarios.find((p) => p.capMs !== null)!;
  assert.equal(cap.pings, 1);
  assert.equal(cap.finalPings, 1);
});
