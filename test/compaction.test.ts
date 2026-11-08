import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { writeFile } from "node:fs/promises";
import type { Profile, Scan, StructuralEvent } from "../src/model.js";
import { compactionControls, windowControl } from "../src/controls.js";
import { analyze } from "../src/replay.js";
import { formatReport } from "../src/report.js";
import { scan, usage } from "../src/reader.js";
import { claudeUsage, makeRequest, prices, workspace } from "./helpers.js";

function data(count = 4): Scan {
  const profile: Profile = {
    id: "profile",
    harness: "claude",
    root: "synthetic",
    aliases: ["synthetic"],
    histories: [],
    settingsFile: "synthetic/settings.json",
    provider: "anthropic",
    settings: {},
    ttl: {
      main: {
        value: "5m",
        owner: "settings.json:promptCacheTtl",
        reason: "current explicit setting",
      },
      subagent: {
        value: "5m",
        owner: "settings.json:subagentPromptCacheTtl",
        reason: "current explicit setting",
      },
    },
    compaction: compactionControls({
      harness: "claude",
      user: {
        modelSettings: { "claude-opus-5-5": { autoCompactWindow: 400000 } },
      },
      managed: {},
      env: {},
      file: "settings.json",
      managedFile: "managed.json",
    }),
  };
  const boundary: StructuralEvent = {
    id: "sample-boundary",
    origins: [{ session: "sample", location: { file: "synthetic", line: 1 } }],
    harness: "claude",
    session: "sample",
    time: 0,
    kind: "compaction",
    profiles: [profile.id],
    location: { file: "synthetic", line: 1 },
    request: null,
    model: "claude-opus-5-5",
    version: "2.1.314",
    client: "sdk-ts",
    cwd: null,
    trigger: "auto",
    precomputed: false,
    preTokens: 300000,
    postTokens: 2000,
    durationMs: 1000,
    tool: null,
    toolId: null,
    settings: {},
    idleAttribution: "unknown",
    pricing: {
      provider: "anthropic",
      model: "claude-opus-5-5",
      tier: "standard",
      speed: "standard",
      geo: "global",
    },
  };
  const first = makeRequest({
    id: "sample-resume",
    session: "sample",
    time: 1000,
    endTime: 1000,
  });
  first.parts[0]!.usage = claudeUsage({ write: 20000 });
  first.parts.push({
    ...first.parts[0]!,
    kind: "compaction",
    usage: claudeUsage({ input: 300000, write: 0, output: 2000 }),
  });
  boundary.request = first.id;
  const requests = [first];
  for (let i = 0; i < count; i++) {
    const r = makeRequest({
      id: `large-${i}`,
      time: 3600000 * (i + 1),
      endTime: 3600000 * (i + 1),
    });
    r.parts[0]!.usage = claudeUsage({ write: 150000 });
    requests.push(r);
  }
  return {
    snapshotStartedAt: new Date(0).toISOString(),
    snapshotFinishedAt: new Date(0).toISOString(),
    exposure: {
      components: [],
      requests: [],
      invocations: [],
      visibility: [],
      toolCalls: [],
    },
    since: null,
    requests,
    events: [boundary],
    cacheContext: { requests: [], boundaries: [] },
    profiles: [profile],
    roots: [],
    snapshots: [],
    coverage: {
      requests: requests.length,
      sourceKnown: requests.length,
      tierKnown: requests.length,
      missingUsage: 0,
      deferredFiles: 0,
      skippedLines: 0,
      skippedFiles: 0,
      issues: [],
    },
  };
}

function compaction(report: ReturnType<typeof analyze>) {
  return report.checks.find((c) => c.check === "compaction-window")!;
}

function near(actual: number | null, expected: number) {
  assert.notEqual(actual, null);
  assert.ok(Math.abs(actual! - expected) < 1e-10, `${actual} != ${expected}`);
}

test("net window winner prices summary and new prefix once, retaining observed summaries", async () => {
  const s = data();
  const report = analyze({ scan: s, catalog: await prices(), now: 18000000 });
  const check = compaction(report);
  assert.equal(check.action, "change");
  assert.equal(check.current, 400000);
  assert.equal(check.recommended, 100000);
  assert.equal(
    check.owner,
    "settings.json:modelSettings.claude-opus-5-5.autoCompactWindow",
  );
  const scenario = check.scenarios.find((c) => c.window === 100000)!;
  near(scenario.savingsUsd, 1.96);
  near(check.baselineCost.totalUsd! - scenario.cost.totalUsd!, 1.96);
  assert.equal(scenario.added.length, 1);
  near(scenario.added[0]!.summaryCost.totalUsd, 0.64);
  near(scenario.added[0]!.prefixCost.totalUsd, 0.1);
  near(check.observedSummaryCost.totalUsd, 1.24);
  assert.deepEqual(scenario.added[0]!.basis, [
    { event: "sample-boundary", request: "sample-resume" },
  ]);
  assert.match(
    formatReport(report, { details: true }),
    /compaction loses detail/,
  );
  assert.match(check.breakEven!, /1.9600/);
  near(scenario.sensitivity.rereadBreakEvenUsd, 1.96);
  assert.equal(check.ttls.main, "5m");
});

test("window presentation keeps conditional baselines, every scenario and geometry evidence reachable", async () => {
  const report = analyze({
    scan: data(),
    catalog: await prices(),
    now: 18000000,
  });
  const check = compaction(report);
  const text = formatReport(report, { width: 40, details: true });
  const compact = text.replace(/\s/g, "");
  for (const value of [
    check.owner,
    check.baseline,
    ...check.models.filter((m) => m !== null),
    ...check.assumptions,
    ...check.uncertainty,
    ...check.sources,
  ])
    assert.ok(compact.includes(value.replace(/\s/g, "")), value);
  assert.ok(compact.includes("Current:400000tokens"));
  assert.ok(compact.includes("Suggested:100000tokens"));
  assert.ok(compact.includes("ConditionalTTLbaseline:main5m;subagent5m"));
  for (const scenario of check.scenarios) {
    assert.ok(compact.includes(`Candidatewindow:${scenario.window}tokens`));
    for (const added of scenario.added) {
      assert.ok(
        compact.includes(
          `at${added.request};${added.inputTokens}summaryinputtokens;${added.summaryTokens}summaryoutputtokens;${added.prefixTokens}resumed-prefixtokens`,
        ),
      );
      for (const basis of added.basis)
        assert.ok(
          compact.includes(
            `boundary${basis.event};resumedrequest${basis.request}`,
          ),
        );
    }
  }
  const unknown = structuredClone(report);
  const unresolved = compaction(unknown);
  unresolved.current = null;
  unresolved.recommended = null;
  unresolved.action = "advisory";
  unresolved.evidence = "unknown";
  unresolved.savingsUsd = null;
  unresolved.monthlySavingsUsd = null;
  unresolved.baselineCost = {
    knownUsd: 1.23,
    totalUsd: null,
    unknown: ["synthetic unknown charge"],
    assumptions: [],
  };
  const plain = formatReport(unknown, { details: true });
  assert.match(plain, /ADVISORY  compaction-window/);
  assert.match(
    plain,
    /Current-window baseline cost: unknown \(\$1\.23 known subtotal\)/,
  );
  assert.match(
    plain,
    /Estimated net saving vs baseline: unknown over window; unknown \/ 30 days/,
  );
});

test("summary overhead can lose, sensitivity cannot veto a positive aggregate estimate", async () => {
  const catalog = await prices();
  const one = compaction(analyze({ scan: data(1), catalog, now: 18000000 }));
  assert.equal(one.recommended, 100000);
  assert.equal(one.action, "change");
  const scenario = one.scenarios[0]!;
  near(scenario.savingsUsd, 0.01);
  assert.ok(scenario.sensitivity.coldSummarySavingsUsd! < 0);
  const larger = data(1);
  larger.events[0]!.postTokens = 3000;
  larger.requests[0]!.parts[1]!.usage.output = 3000;
  const loss = compaction(analyze({ scan: larger, catalog, now: 18000000 }));
  assert.equal(loss.recommended, 400000);
  assert.equal(loss.action, "keep");
  assert.ok(loss.scenarios[0]!.savingsUsd! < 0);
});

test("current smaller window is the comparison baseline, rather than old larger history", async () => {
  const s = data();
  s.profiles[0]!.compaction.models["claude-opus-5-5"]!.value = 100000;
  const check = compaction(
    analyze({ scan: s, catalog: await prices(), now: 18000000 }),
  );
  assert.equal(check.recommended, 100000);
  assert.equal(check.action, "keep");
  near(check.savingsUsd, 0);
  near(check.scenarios[0]!.savingsUsd, 0);
  near(check.baselineCost.totalUsd, check.scenarios[0]!.cost.totalUsd!);
});

test("unsupported affected usage is unpriced; unknown unchanged output cancels with provenance", async () => {
  const catalog = await prices();
  const s = data();
  s.requests[2]!.parts[0]!.usage.output = null;
  const check = compaction(analyze({ scan: s, catalog, now: 18000000 }));
  assert.equal(check.baselineCost.totalUsd, null);
  assert.equal(check.action, "change");
  near(check.savingsUsd, 1.96);
  const missing = data();
  missing.requests[2]!.parts[0]!.usage.input = null;
  missing.requests[2]!.evidence = "unknown";
  const unknown = compaction(
    analyze({ scan: missing, catalog, now: 18000000 }),
  );
  assert.equal(unknown.action, "advisory");
  assert.equal(unknown.scenarios[0]!.savingsUsd, null);
  assert.match(unknown.uncertainty.join(" "), /missing input/);
  const noSample = data();
  noSample.events[0]!.postTokens = null;
  noSample.requests[0]!.parts[1]!.usage.output = null;
  assert.equal(
    compaction(analyze({ scan: noSample, catalog, now: 18000000 }))
      .scenarios[0]!.savingsUsd,
    null,
  );
});

test("effective per-model controls honor managed scope, environment ownership and dated IDs", () => {
  const s = data();
  const p = s.profiles[0]!;
  const options = {
    harness: "claude" as const,
    user: {
      autoCompactWindow: 600000,
      modelSettings: { "claude-opus-5-5": { autoCompactWindow: 400000 } },
      env: { CLAUDE_CODE_AUTO_COMPACT_WINDOW: "300k" },
    },
    managed: {},
    env: {},
    file: "user.json",
    managedFile: "managed.json",
  };
  p.compaction = compactionControls(options);
  assert.equal(windowControl(p, "claude-opus-5-5-20261101").value, 300000);
  assert.equal(
    windowControl(p, "claude-opus-5-5").owner,
    "user.json:env.CLAUDE_CODE_AUTO_COMPACT_WINDOW",
  );
  p.compaction = compactionControls({
    ...options,
    user: { ...options.user, env: {} },
    managed: { autoCompactWindow: 500000 },
  });
  assert.equal(windowControl(p, "claude-opus-5-5").value, 500000);
  assert.equal(
    windowControl(p, "claude-opus-5-5").owner,
    "managed.json:autoCompactWindow",
  );
  p.compaction = compactionControls({
    ...options,
    env: { CLAUDE_CODE_AUTO_COMPACT_WINDOW: "200000" },
  });
  assert.equal(
    windowControl(p, "claude-opus-5-5").owner,
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
  );
  p.compaction = compactionControls({
    ...options,
    managed: { autoCompactEnabled: false },
  });
  assert.equal(windowControl(p, "claude-opus-5-5").applicable, false);
  assert.throws(
    () => compactionControls({ ...options, user: { autoCompactWindow: -1 } }),
    /invalid auto-compact window/,
  );
});

test("historical-only or unsupported client controls remain advisory, including unknown candidates", async () => {
  const catalog = await prices();
  const s = data();
  s.profiles[0]!.compaction.models["claude-opus-5-5"]!.value = null;
  s.profiles[0]!.compaction.models["claude-opus-5-5"]!.applicable = false;
  const unknownCurrent = compaction(
    analyze({ scan: s, catalog, now: 18000000 }),
  );
  assert.equal(unknownCurrent.recommended, 100000);
  assert.equal(unknownCurrent.action, "advisory");
  const old = data();
  for (const r of old.requests) r.version = "2.1.287";
  assert.equal(
    compaction(analyze({ scan: old, catalog, now: 18000000 })).action,
    "advisory",
  );
  for (const r of old.requests) r.version = "3.unverified";
  assert.equal(
    compaction(analyze({ scan: old, catalog, now: 18000000 })).action,
    "advisory",
  );
  const unsupported = data();
  unsupported.requests[2]!.parts[0]!.model = "unknown-model";
  assert.equal(
    compaction(analyze({ scan: unsupported, catalog, now: 18000000 }))
      .scenarios[0]!.savingsUsd,
    null,
  );
});

test("Codex standalone compaction uses linked billed summary and cold prefix at the full context tier", async () => {
  const s = data();
  const p = s.profiles[0]!;
  p.harness = "codex";
  p.settings = { model_auto_compact_token_limit: 400000 };
  p.compaction = compactionControls({
    harness: "codex",
    user: p.settings,
    managed: {},
    env: {},
    file: "config.toml",
    managedFile: "unused",
  });
  s.requests[0]!.parts.splice(1);
  for (const r of s.requests) {
    r.harness = "codex";
    r.api = "responses";
    r.provider = "openai";
    r.model = "gpt-6.1-sol";
    r.parts[0]!.model = r.model;
    r.parts[0]!.usage = usage(
      {
        input_tokens: r.id === "sample-resume" ? 20000 : 300000,
        cached_input_tokens: 0,
        cache_write_input_tokens: r.id === "sample-resume" ? 20000 : 300000,
        output_tokens: 10,
      },
      "codex",
    );
  }
  const summary = makeRequest({
    ...s.requests[0]!,
    id: "summary",
    time: -1000,
    endTime: -1000,
  });
  summary.parts = [
    {
      ...s.requests[0]!.parts[0]!,
      kind: "compaction",
      usage: usage(
        {
          input_tokens: 300000,
          cached_input_tokens: 0,
          cache_write_input_tokens: 0,
          output_tokens: 2000,
        },
        "codex",
      ),
    },
  ];
  s.requests.unshift(summary);
  const e = s.events[0]!;
  e.harness = "codex";
  e.request = summary.id;
  e.postTokens = null;
  e.model = "gpt-6.1-sol";
  const report = analyze({ scan: s, catalog: await prices(), now: 18000000 });
  const check = compaction(report);
  assert.equal(check.action, "change");
  assert.equal(check.recommended, 20000);
  near(check.observedSummaryCost.totalUsd, 1.23);
  const first = check.scenarios.find((c) => c.window === 20000)!.added[0]!;
  near(first.summaryCost.totalUsd, 1.23);
  near(first.prefixCost.totalUsd, 0.05);
  near(check.scenarios.find((c) => c.window === 20000)!.savingsUsd, 4.5702);
  const unpriced = structuredClone(s);
  const unsupported = unpriced.profiles[0]!;
  unsupported.settings.model_auto_compact_token_limit_scope =
    "body_after_prefix";
  unsupported.compaction = compactionControls({
    harness: "codex",
    user: unsupported.settings,
    managed: {},
    env: {},
    file: "config.toml",
    managedFile: "unused",
  });
  const u = compaction(
    analyze({ scan: unpriced, catalog: await prices(), now: 18000000 }),
  );
  assert.equal(u.action, "advisory");
  assert.equal(u.scenarios[0]!.savingsUsd, null);
});

test("bracketed Claude model controls scan safely and unresolved aliases stay advisory", async () => {
  const w = await workspace();
  try {
    const user = {
      autoCompactWindow: 600000,
      modelSettings: {
        "opus[1m]": { autoCompactWindow: 400000 },
        "claude-opus-4-6[1m]": { autoCompactWindow: 300000 },
      },
    };
    await writeFile(join(w.claude, "settings.json"), JSON.stringify(user));
    const result = await scan(w.options);
    const p = result.profiles.find((p) => p.harness === "claude")!;
    assert.equal(windowControl(p, "opus[1m]").value, 400000);
    assert.equal(windowControl(p, "claude-opus-4-6").value, null);
    assert.equal(windowControl(p, "claude-opus-4-6").applicable, false);
    assert.match(windowControl(p, "claude-opus-4-6").reason, /alias/);
    p.compaction = compactionControls({
      harness: "claude",
      user,
      managed: { autoCompactWindow: 500000 },
      env: {},
      file: "user.json",
      managedFile: "managed.json",
    });
    assert.equal(windowControl(p, "claude-opus-4-6").value, 500000);
    assert.equal(
      windowControl(p, "claude-opus-4-6").owner,
      "managed.json:autoCompactWindow",
    );
    await writeFile(
      join(w.claude, "settings.json"),
      JSON.stringify({ modelSettings: { "opus[1m]": null } }),
    );
    await assert.rejects(scan(w.options), /invalid per-model settings/);
  } finally {
    await w.dispose();
  }
});

test("discovery retains only whitelisted compaction controls and malformed selected limits fail", async () => {
  const w = await workspace();
  try {
    await writeFile(
      join(w.claude, "settings.json"),
      JSON.stringify({
        modelSettings: {
          "claude-opus-5-5": {
            autoCompactWindow: 400000,
            privateText: "PRIVATE",
          },
        },
      }),
    );
    await writeFile(
      join(w.codex, "config.toml"),
      "model_auto_compact_token_limit = 240000\nmodel_auto_compact_token_limit_scope = 'body_after_prefix'\n",
    );
    const result = await scan(w.options);
    assert.equal(
      windowControl(
        result.profiles.find((p) => p.harness === "claude")!,
        "claude-opus-5-5",
      ).value,
      400000,
    );
    const codex = result.profiles.find((p) => p.harness === "codex")!;
    assert.equal(codex.compaction.default.value, 240000);
    assert.equal(codex.compaction.default.applicable, false);
    assert.ok(!JSON.stringify(result).includes("PRIVATE"));
    await writeFile(
      join(w.codex, "config.toml"),
      "model_auto_compact_token_limit = 'bad'\n",
    );
    await assert.rejects(scan(w.options), /invalid compaction token limit/);
  } finally {
    await w.dispose();
  }
});

test("window replay uses TTL recommendations and never inherits old-prefix external warming", async () => {
  const s = data(2);
  const first = s.requests[1]!;
  const next = s.requests[2]!;
  next.time = first.time + 600000;
  next.endTime = next.time;
  next.parts[0]!.usage = claudeUsage({ write: 150000 });
  const report = analyze({ scan: s, catalog: await prices(), now: 18000000 });
  const check = compaction(report);
  assert.equal(check.ttls.main, "1h");
  near(check.scenarios[0]!.added[0]!.prefixCost.totalUsd, 0.16);
  const warm = check.scenarios[0]!.cost.totalUsd!;
  const external = makeRequest({
    ...next,
    id: "externally-warmed",
    time: next.time + 3600000,
    endTime: next.time + 3600000,
  });
  external.parts = [
    { ...next.parts[0]!, usage: claudeUsage({ read: 150000, write: 0 }) },
  ];
  s.requests.push(external);
  const cold = compaction(
    analyze({ scan: s, catalog: await prices(), now: 18000000 }),
  );
  assert.equal(cold.ttls.main, "1h");
  // The first new prefix expired: its 20k rewrite costs $0.16, not an inherited old read.
  near(cold.scenarios[0]!.cost.totalUsd! - warm, 0.1602);
  assert.equal(next.parts[0]!.usage.read, 0);
});

test("changing a message prefix preserves unchanged billed compaction iterations", async () => {
  const s = data(2);
  const first = s.requests[1]!;
  const next = s.requests[2]!;
  next.time = first.time + 600000;
  next.endTime = next.time;
  for (const r of [first, next])
    r.parts.push({
      ...r.parts[0]!,
      kind: "compaction",
      usage: claudeUsage({ read: 150000, write: 1000, ttl: "1h", output: 100 }),
    });
  const before = structuredClone(s.requests);
  const check = compaction(
    analyze({ scan: s, catalog: await prices(), now: 18000000 }),
  );
  assert.equal(check.ttls.main, "1h");
  assert.equal(check.ttls.subagent, "1h");
  near(check.savingsUsd, 0.426);
  assert.deepEqual(s.requests, before);
});

test("window estimates reuse bracketed attempt inputs and retain missing historical billing", async () => {
  const s = data(2);
  const before = s.requests[1]!;
  const after = s.requests[2]!;
  after.time = before.time + 2000;
  after.endTime = after.time;
  after.parts[0]!.usage = claudeUsage({ read: 150000, write: 0 });
  const attempt = makeRequest({
    id: "attempt",
    time: before.time + 1000,
    endTime: before.time + 1000,
    evidence: "unknown",
    reference: "attempt-ref",
    attempt: { maxOutputTokens: 100 },
  });
  attempt.parts[0]!.usage = usage({}, "claude");
  s.requests.splice(2, 0, attempt);
  const report = analyze({ scan: s, catalog: await prices(), now: 18000000 });
  const check = compaction(report);
  assert.equal(check.usageEstimates[0]!.evidence, "estimated");
  assert.deepEqual(check.usageEstimates[0]!.basis, [before.id, after.id]);
  assert.equal(check.baselineCost.totalUsd, null);
  assert.equal(check.scenarios[0]!.cost.totalUsd, null);
  near(check.savingsUsd, 0.062);
  assert.equal(attempt.parts[0]!.usage.input, null);
});

test("pre-window summary linkage calibrates selected work without charging old generation", async () => {
  const s = data();
  const summary = makeRequest({
    id: "old-summary",
    session: "sample",
    time: -1000,
    endTime: -1000,
  });
  summary.parts[0]!.kind = "compaction";
  summary.parts[0]!.usage = claudeUsage({
    input: 300000,
    write: 0,
    output: 2000,
  });
  s.requests[0]!.parts.splice(1);
  const event = s.events.pop()!;
  event.request = summary.id;
  event.preTokens = null;
  event.postTokens = null;
  s.cacheContext = { requests: [summary], boundaries: [event] };
  s.since = 1000;
  const report = analyze({ scan: s, catalog: await prices(), now: 18000000 });
  const check = compaction(report);
  assert.equal(check.since, 1000);
  assert.equal(check.requests, 5);
  near(check.observedSummaryCost.totalUsd, 0);
  assert.equal(report.compactions.length, 0);
  near(check.scenarios[0]!.added[0]!.summaryCost.totalUsd, 0.64);
  near(check.savingsUsd, 1.96);
});

test("an unsupported smaller window does not veto another candidate's positive aggregate", async () => {
  const s = data();
  s.requests[0]!.parts[0]!.usage = claudeUsage({ write: 150000 });
  for (const r of s.requests.slice(1))
    r.parts[0]!.usage = claudeUsage({ write: 300000 });
  const check = compaction(
    analyze({ scan: s, catalog: await prices(), now: 18000000 }),
  );
  assert.equal(check.scenarios[0]!.savingsUsd, null);
  assert.equal(check.recommended, 150000);
  assert.equal(check.action, "change");
  assert.ok(check.savingsUsd! > 0);
});

test("shared Claude controls produce one aggregate recommendation for all affected models", async () => {
  for (const environment of [false, true]) {
    const s = data();
    const model = "claude-haiku-5-5";
    const other = structuredClone(s.requests[0]!);
    other.id = "haiku-resume";
    other.session = "haiku-sample";
    other.model = model;
    for (const p of other.parts) p.model = model;
    const event = structuredClone(s.events[0]!);
    event.id = "haiku-boundary";
    event.session = other.session;
    event.origins[0]!.session = other.session;
    event.model = model;
    event.request = other.id;
    event.pricing!.model = model;
    s.requests.push(other);
    s.events.push(event);
    for (let i = 0; i < 3; i++) {
      const request = makeRequest({
        id: `haiku-${i}`,
        session: "haiku",
        model,
        time: (i + 1) * 3600000,
        endTime: (i + 1) * 3600000,
      });
      request.parts[0]!.model = model;
      request.parts[0]!.usage = claudeUsage({ write: 150000 });
      s.requests.push(request);
    }
    s.profiles[0]!.compaction = compactionControls({
      harness: "claude",
      user: { autoCompactWindow: 400000 },
      managed: {},
      env: environment ? { CLAUDE_CODE_AUTO_COMPACT_WINDOW: "400000" } : {},
      file: "settings.json",
      managedFile: "managed.json",
    });
    const checks = analyze({
      scan: s,
      catalog: await prices(),
      now: 18000000,
    }).checks.filter((c) => c.check === "compaction-window");
    assert.equal(checks.length, 1);
    assert.deepEqual(
      new Set(checks[0]!.models),
      new Set(["claude-opus-5-5", model]),
    );
    assert.equal(
      checks[0]!.owner,
      environment
        ? "CLAUDE_CODE_AUTO_COMPACT_WINDOW"
        : "settings.json:autoCompactWindow",
    );
    assert.equal(checks[0]!.recommended, 100000);
    assert.equal(checks[0]!.action, "change");
  }
});

test("small unknown iterations cancel while truly missing input leaves only its own session unpriced", async () => {
  const s = data();
  const small = makeRequest({
    id: "small-unknown",
    session: "separate-small",
    time: 1000,
    endTime: 1000,
  });
  small.parts[0]!.kind = "unknown";
  small.parts[0]!.usage = claudeUsage({ write: 5000 });
  s.requests.push(small);
  const check = compaction(
    analyze({ scan: s, catalog: await prices(), now: 18000000 }),
  );
  assert.equal(check.baselineCost.totalUsd, null);
  assert.equal(check.action, "change");
  near(check.savingsUsd, 1.96);
  small.parts[0]!.usage.input = null;
  const scenario = compaction(
    analyze({ scan: s, catalog: await prices(), now: 18000000 }),
  ).scenarios[0]!;
  near(scenario.savingsUsd, 1.96);
  assert.equal(scenario.coverage.total - scenario.coverage.priced, 1);
});

test("Claude historical window differences and structured TTL window alignment remain visible", async () => {
  const s = data();
  const e = structuredClone(s.events[0]!);
  e.id = "settings";
  e.kind = "settings";
  e.settings = { autoCompactWindow: 1000000 };
  s.events.push(e);
  const check = compaction(
    analyze({ scan: s, catalog: await prices(), now: 18000000 }),
  );
  assert.match(
    check.uncertainty.join(" "),
    /historical recorded limits differ/,
  );
  assert.equal(check.action, "change");
});

test("added summaries require billed output and do not confuse postTokens with output", async () => {
  const s = data();
  s.requests[0]!.parts.splice(1);
  s.events[0]!.request = null;
  s.events[0]!.postTokens = 60000;
  const report = analyze({ scan: s, catalog: await prices(), now: 18000000 });
  assert.equal(report.compactions[0]!.evidence, "estimated");
  assert.equal(compaction(report).scenarios[0]!.savingsUsd, null);
});
