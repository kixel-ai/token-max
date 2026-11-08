import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import type {
  CompactionCheckResult,
  Profile,
  Request,
  Scan,
  StructuralEvent,
  TtlCheckResult,
} from "../src/model.js";
import { idleControls, compactionControls } from "../src/controls.js";
import { idleChecks } from "../src/idle.js";
import { analyze, compactionCharges } from "../src/replay.js";
import { scan } from "../src/reader.js";
import { formatReport } from "../src/report.js";
import { claudeUsage, makeRequest, prices, workspace } from "./helpers.js";

const MINUTE = 60000;
function request(
  id: string,
  minute: number,
  tokens = 300000,
  overrides: Partial<Request> = {},
): Request {
  const r = makeRequest({
    id,
    session: "target",
    time: minute * MINUTE,
    endTime: minute * MINUTE,
    client: "cli",
    source: "repl_main_thread",
    ...overrides,
  });
  r.parts[0]!.usage = claudeUsage({ write: tokens, output: 0, ttl: "1h" });
  return r;
}
function event(overrides: Partial<StructuralEvent> = {}): StructuralEvent {
  return {
    id: "boundary",
    origins: [{ session: "sample", location: { file: "synthetic", line: 1 } }],
    harness: "claude",
    session: "sample",
    time: 2 * MINUTE,
    kind: "compaction",
    profiles: ["profile"],
    location: { file: "synthetic", line: 1 },
    request: "summary",
    model: "claude-opus-5-5",
    version: "2.1.314",
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
      model: "claude-opus-5-5",
      provider: "anthropic",
      tier: "standard",
      speed: "standard",
      geo: "global",
    },
    ...overrides,
  };
}
function data(target: Request[], boundaries: StructuralEvent[] = []): Scan {
  const summary = request("summary", 1, 0, {
    session: "sample",
    source: "compact",
  });
  summary.parts[0]!.kind = "compaction";
  summary.parts[0]!.usage = claudeUsage({
    read: 300000,
    write: 0,
    output: 2000,
  });
  const resumed = request("sample-resume", 3, 15000, {
    session: "sample",
    client: "cli",
  });
  resumed.parts[0]!.usage = claudeUsage({
    read: 5000,
    write: 15000,
    output: 0,
    ttl: "1h",
  });
  const scopes = [
    {
      config: { idleCompaction: false, autoCompactWindow: 400000 },
      file: "settings.json",
    },
  ];
  const profile: Profile = {
    id: "profile",
    harness: "claude",
    root: "synthetic",
    aliases: [],
    histories: [],
    settingsFile: "settings.json",
    provider: "anthropic",
    settings: { idleCompaction: false },
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
      harness: "claude",
      user: scopes[0]!.config,
      managed: {},
      env: {},
      file: "settings.json",
      managedFile: "managed.json",
    }),
    idle: idleControls({ scopes, env: {} }),
  };
  const requests = [summary, resumed, ...target].sort(
    (a, b) => a.time - b.time,
  );
  return {
    snapshotStartedAt: new Date(0).toISOString(),
    snapshotFinishedAt: new Date(0).toISOString(),
    since: null,
    requests,
    events: [event(), ...boundaries].sort((a, b) => a.time - b.time),
    profiles: [profile],
    roots: [],
    snapshots: [],
    cacheContext: { requests: [], boundaries: [] },
    exposure: {
      components: [],
      requests: [],
      invocations: [],
      visibility: [],
      toolCalls: [],
    },
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
async function check(
  s: Scan,
  now = 201,
  policies: { ttls?: TtlCheckResult[]; windows?: CompactionCheckResult[] } = {},
) {
  const catalog = await prices();
  return idleChecks({
    scan: s,
    catalog,
    summaries: compactionCharges(s, catalog),
    estimates: new Map(),
    ttls: policies.ttls ?? [],
    windows: policies.windows ?? [],
    since: s.since ?? undefined,
    now: now * MINUTE,
  })[0]!;
}
function near(actual: number | null, expected: number) {
  assert.notEqual(actual, null);
  assert.ok(Math.abs(actual! - expected) < 1e-9, `${actual} != ${expected}`);
}

test("idle on wins with full prefix, warm summary and expired resumed prefix, carried until convergence", async () => {
  const follow = request("follow", 201, 1000);
  follow.parts[0]!.usage = claudeUsage({
    read: 301000,
    write: 1000,
    output: 0,
    ttl: "1h",
  });
  const s = data([
    request("before", 100),
    request("return", 200, 301000),
    follow,
  ]);
  const c = await check(s, 202);
  assert.equal(c.action, "change");
  assert.equal(c.recommended, true);
  const a = c.afterRecommendations;
  const p = a.periods.find((p) => p.request === "before")!;
  near(p.summaryCost.totalUsd, 0.1);
  assert.equal(p.calibration!.prefixTokens, 20000);
  assert.equal(p.calibration!.summaryTokens, 2000);
  // First replaced prefix: 21k cold writes; following 22k reads 21k and writes 1k.
  // Off: 301k cold writes; following 302k reads 301k and writes 1k.
  near(a.deltaUsd, -2.196);
  near(a.cost.on.totalUsd! - a.cost.off.totalUsd!, a.deltaUsd!);
  assert.ok(a.sensitivity.rereadBreakEvenUsdPerResumedSummary! > 0);
  assert.equal(p.activation, 1);
  assert.match(p.activationBasis, /fallback/);
  assert.equal(p.returnTiming, "after-expiry");
});

test("unresumed summaries cost money; recent finals are censored rather than abandoned", async () => {
  const s = data([request("final", 100)]);
  const c = await check(s, 200);
  assert.equal(c.recommended, false);
  assert.equal(c.action, "keep");
  near(c.afterRecommendations.deltaUsd, 0.1);
  const final = c.afterRecommendations.periods.find(
    (p) => p.request === "final",
  )!;
  assert.equal(final.resume, null);
  near(final.summaryCost.totalUsd, 0.1);
  near(c.afterRecommendations.sensitivity.closedDeltaUsd, 0);
  near(c.afterRecommendations.sensitivity.openDeltaUsd, 0.1);
  const recent = await check(s, 150);
  assert.equal(
    recent.afterRecommendations.periods.find((p) => p.request === "final")!
      .status,
    "censored",
  );
  near(recent.afterRecommendations.deltaUsd, 0);
  s.profiles[0]!.idle!.value = true;
  const off = await check(s, 200);
  assert.equal(off.action, "change");
  assert.equal(off.recommended, false);
});

test("actual external cache evidence and near-expiry timing can make on lose", async () => {
  for (const minute of [158, 200]) {
    const back = request("return", minute, 1000);
    back.parts[0]!.usage = claudeUsage({
      read: 300000,
      write: 1000,
      output: 0,
      ttl: "1h",
    });
    const c = await check(data([request("before", 100), back]), minute + 1);
    assert.equal(c.recommended, false);
    near(c.afterRecommendations.deltaUsd, 0.161);
    assert.equal(
      c.afterRecommendations.periods.find((p) => p.request === "before")!
        .returnTiming,
      minute === 158 ? "near-expiry" : "after-expiry",
    );
    if (minute === 158)
      near(c.afterRecommendations.sensitivity.lateTriggerDeltaUsd, 0);
  }
});

test("aggregate winner can differ from most periods and sensitivity can cross zero", async () => {
  const target = [request("before", 100), request("return", 200, 301000)];
  for (let i = 0; i < 3; i++)
    target.push(request(`final-${i}`, 100, 300000, { session: `final-${i}` }));
  const c = await check(data(target), 201);
  assert.equal(c.recommended, true);
  near(c.afterRecommendations.deltaUsd, -1.84);
  const targets = [request("before", 100), request("return", 200, 21000)];
  for (let i = 0; i < 22; i++)
    targets.push(
      request(`uncertain-${i}`, 100, 300000, { session: `uncertain-${i}` }),
    );
  const e = event({
    id: "observed-idle",
    session: "target",
    origins: [{ session: "target", location: { file: "synthetic", line: 2 } }],
    time: 199 * MINUTE,
    idleAttribution: "notice",
    trigger: "auto",
    request: null,
  });
  const mixed = await check(data(targets, [e]), 201);
  assert.ok(mixed.afterRecommendations.sensitivity.closedDeltaUsd! < 0);
  assert.ok(mixed.afterRecommendations.sensitivity.openDeltaUsd! > 0);
  assert.equal(mixed.recommended, false);
  assert.equal(mixed.action, "keep");
  assert.ok(
    mixed.afterRecommendations.sensitivity.lifetimeBreakEvenFraction! > 0,
  );
  assert.ok(
    mixed.afterRecommendations.sensitivity.lifetimeBreakEvenFraction! < 1,
  );
});

test("missing full-prefix calibration and unknown eligibility stay unpriced", async () => {
  for (const mode of [
    "sample",
    "version",
    "desktop",
    "flags",
    "threshold",
    "window",
    "input",
  ] as const) {
    const s = data([request("final", 100)]);
    if (mode === "sample")
      s.requests = s.requests.filter((r) => r.id !== "summary");
    if (mode === "version") s.requests.at(-1)!.version = null;
    if (mode === "desktop") s.requests.at(-1)!.client = "claude-desktop";
    if (mode === "flags") s.requests.at(-1)!.version = "2.1.300";
    if (mode === "threshold") s.profiles[0]!.idle!.threshold = null;
    if (mode === "window") s.profiles[0]!.compaction.default.value = null;
    if (mode === "input") s.requests.at(-1)!.parts[0]!.usage.input = null;
    const c = await check(s, 200);
    assert.equal(c.afterRecommendations.deltaUsd, null, mode);
    assert.equal(c.recommended, null, mode);
    assert.equal(c.action, "advisory", mode);
  }
});

test("idle replay preserves comparison estimates without an explicit window", async () => {
  const s = data([request("estimated", 100)]);
  const r = s.requests.at(-1)!;
  const part = structuredClone(r.parts[0]!);
  r.parts[0]!.usage.input = null;
  s.profiles[0]!.compaction.default.value = null;
  const catalog = await prices();
  const c = idleChecks({
    scan: s,
    catalog,
    summaries: compactionCharges(s, catalog),
    estimates: new Map([
      [
        r.id,
        {
          request: r.id,
          evidence: "estimated",
          basis: ["synthetic recorded attempt input"],
          part,
          maxOutputTokens: null,
          reason: "comparison-only input estimate",
          assumptions: [],
        },
      ],
    ]),
    ttls: [],
    windows: [],
    since: 100 * MINUTE,
    now: 200 * MINUTE,
  })[0]!;
  assert.equal(c.currentSettings.periods[0]!.contextTokens, 300000);
  near(c.currentSettings.cost.off.knownUsd, 2.4);
  assert.equal(c.currentSettings.deltaUsd, null);
});

test("a completed or replaced turn clears abandoned tool waits", async () => {
  for (const toolId of [null, "abandoned"]) {
    for (const kind of ["turn-start", "turn-end"] as const) {
      const s = data([request("final", 100)]);
      const origin = [
        { session: "target", location: { file: "synthetic", line: 9 } },
      ];
      s.events.push(
        event({
          id: "abandoned",
          kind: "tool-start",
          toolId,
          session: "target",
          origins: origin,
          time: 50 * MINUTE,
        }),
        event({
          id: "completion",
          kind,
          session: "target",
          origins: origin,
          time: 90 * MINUTE,
        }),
      );
      s.events.sort((a, b) => a.time - b.time);
      const c = await check(s, 200);
      assert.equal(
        c.afterRecommendations.periods.find((p) => p.request === "final")!
          .status,
        "eligible",
      );
      near(c.afterRecommendations.deltaUsd, 0.1);
    }
  }
});

test("unsupported clients and prerequisites do not emit an idle toggle", async () => {
  for (const mode of [
    "sdk",
    "subagent",
    "tool",
    "size",
    "window",
    "ttl",
    "disabled",
    "old",
  ] as const) {
    const s = data([request("final", 100)]);
    const r = s.requests.at(-1)!;
    if (mode === "sdk") r.client = "sdk-ts";
    if (mode === "subagent") r.agent = "subagent";
    if (mode === "tool")
      s.events.push(
        event({
          id: "tool",
          kind: "tool-start",
          session: r.session,
          origins: [
            { session: r.session, location: { file: "synthetic", line: 3 } },
          ],
          time: r.time,
          toolId: "tool",
          request: r.id,
        }),
      );
    if (mode === "size")
      r.parts[0]!.usage = claudeUsage({ write: 100000, output: 0 });
    if (mode === "window") s.profiles[0]!.compaction.default.value = 200000;
    if (mode === "ttl") s.profiles[0]!.ttl.main.value = "5m";
    if (mode === "disabled") s.profiles[0]!.idle!.autoCompact = false;
    if (mode === "old") r.version = "2.1.289";
    const c = await check(s, 200);
    assert.notEqual(c.action, "change", mode);
    if (mode !== "old") near(c.afterRecommendations.deltaUsd, 0);
  }
});

test("TTL/window choices decide idle applicability while the current comparison is retained", async () => {
  const s = data([request("final", 100)]);
  for (const kind of ["ttl", "window"]) {
    const c = await check(
      s,
      200,
      kind === "ttl"
        ? {
            ttls: [
              {
                profile: "profile",
                bucket: "main",
                action: "change",
                recommended: "5m",
              } as TtlCheckResult,
            ],
          }
        : {
            windows: [
              {
                profile: "profile",
                owner: "settings.json:autoCompactWindow",
                action: "change",
                recommended: 100000,
              } as CompactionCheckResult,
            ],
          },
    );
    near(c.currentSettings.deltaUsd, 0.1);
    assert.equal(c.afterRecommendations.applicable, false);
    assert.equal(c.action, "advisory");
    assert.equal(c.recommended, null);
  }
});

test("known applied idle uses measured summary once and observed resumed geometry", async () => {
  const summary = request("idle-summary", 154, 0);
  summary.parts[0]!.kind = "compaction";
  summary.parts[0]!.usage = claudeUsage({
    read: 300000,
    write: 0,
    output: 2000,
  });
  const back = request("return", 200, 21000);
  const b = event({
    id: "applied",
    session: "target",
    origins: [{ session: "target", location: { file: "synthetic", line: 4 } }],
    time: 199 * MINUTE,
    idleAttribution: "notice",
    trigger: "auto",
    request: "idle-summary",
  });
  const s = data([request("before", 100), summary, back], [b]);
  const c = await check(s);
  const p = c.afterRecommendations.periods.find((p) => p.request === "before")!;
  assert.equal(p.outcome, "applied");
  near(p.summaryCost.totalUsd, 0.1);
  near(c.afterRecommendations.deltaUsd, 0.1 - ((300000 - 21000) * 8) / 1e6);
  near(
    c.afterRecommendations.cost.on.totalUsd! -
      c.afterRecommendations.cost.off.totalUsd!,
    c.afterRecommendations.deltaUsd!,
  );
});

test("same-cohort applied fraction charges finals exactly once and never transfers versions", async () => {
  const summary = request("idle-summary", 154, 0);
  summary.parts[0]!.kind = "compaction";
  summary.parts[0]!.usage = claudeUsage({
    read: 300000,
    write: 0,
    output: 2000,
  });
  const b = event({
    id: "applied",
    session: "yes",
    origins: [{ session: "yes", location: { file: "synthetic", line: 4 } }],
    time: 199 * MINUTE,
    idleAttribution: "notice",
    trigger: "auto",
    request: "idle-summary",
  });
  summary.session = "yes";
  const s = data(
    [
      request("yes", 100, 300000, { session: "yes" }),
      summary,
      request("yes-return", 200, 21000, { session: "yes" }),
      request("no", 100, 300000, { session: "no" }),
      request("no-return", 200, 301000, { session: "no" }),
      request("final", 100, 300000, { session: "final" }),
      request("different", 100, 300000, {
        session: "different",
        version: "2.1.315",
      }),
    ],
    [b],
  );
  for (const session of ["yes", "no"])
    s.events.unshift(
      event({
        id: `settings-${session}`,
        kind: "settings",
        session,
        origins: [{ session, location: { file: "synthetic", line: 5 } }],
        time: 99 * MINUTE,
        settings: {
          idleCompaction: true,
          promptCacheTtl: "1h",
          autoCompactEnabled: true,
        },
      }),
    );
  s.events.sort((a, b) => a.time - b.time);
  const duplicate = {
    ...s.requests.find((r) => r.id === "sample-resume")!,
    id: "different-version-sample",
    session: "different-sample",
    version: "2.1.315",
  };
  const summaryCopy = {
    ...s.requests.find((r) => r.id === "summary")!,
    id: "different-summary",
    session: "different-sample",
    version: "2.1.315",
  };
  s.requests.push(summaryCopy, duplicate);
  s.requests.sort((a, b) => a.time - b.time);
  s.events.push(
    event({
      id: "different-boundary",
      session: "different-sample",
      origins: [
        {
          session: "different-sample",
          location: { file: "synthetic", line: 7 },
        },
      ],
      request: "different-summary",
      version: "2.1.315",
    }),
  );
  s.events.sort((a, b) => a.time - b.time);
  const c = await check(s);
  const a = c.afterRecommendations;
  assert.equal(a.activationCohorts[0]!.fraction, 0.5);
  const final = a.periods.find((p) => p.request === "final")!;
  near(final.activation, 0.5);
  near(final.summaryCost.totalUsd, 0.05);
  const no = a.periods.find((p) => p.request === "no")!;
  assert.equal(no.outcome, "not-applied");
  near(no.summaryCost.totalUsd, 0);
  const different = a.periods.find((p) => p.request === "different")!;
  near(different.activation, 1);
  near(different.summaryCost.totalUsd, 0.1);
  assert.match(c.uncertainty.join(" "), /discarded summaries never ran/);
});

test("native informational notices are the only parsed idle attribution; project owners and threshold floor resolve", async () => {
  const w = await workspace();
  try {
    const project = join(w.home, "project");
    await mkdir(join(project, ".git"), { recursive: true });
    await mkdir(join(project, ".claude"));
    await writeFile(
      join(w.claude, "settings.json"),
      JSON.stringify({
        idleCompaction: true,
        autoCompactWindow: 400000,
        promptCacheTtl: "1h",
      }),
    );
    await writeFile(
      join(project, ".claude", "settings.local.json"),
      JSON.stringify({
        idleCompaction: false,
        env: { CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS: "90000" },
        promptCacheTtl: "5m",
      }),
    );
    const common = {
      sessionId: "native",
      version: "2.1.316",
      entrypoint: "cli",
      cwd: project,
    };
    const rows = [
      {
        ...common,
        type: "assistant",
        querySource: "repl_main_thread",
        timestamp: new Date(0).toISOString(),
        message: {
          id: "request",
          model: "claude-opus-5-5",
          usage: {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 300000,
            cache_creation: {
              ephemeral_1h_input_tokens: 300000,
              ephemeral_5m_input_tokens: 0,
            },
            service_tier: "standard",
          },
        },
      },
      {
        ...common,
        type: "system",
        subtype: "compact_boundary",
        uuid: "boundary",
        timestamp: new Date(1000).toISOString(),
        compactMetadata: {
          trigger: "auto",
          preTokens: 300000,
          postTokens: 2000,
          precomputed: true,
        },
      },
      {
        ...common,
        type: "system",
        subtype: "informational",
        level: "notice",
        uuid: "notice",
        parentUuid: "boundary",
        timestamp: new Date(2000).toISOString(),
        content:
          'Compacted while idle, before the prompt cache expired · turn off with "idleCompaction": false in settings.json',
      },
    ] as const;
    const file = join(w.claude, "projects", "project", "native.jsonl");
    await writeFile(file, rows.map((r) => JSON.stringify(r)).join("\n") + "\n");
    const parsed = await scan(w.options);
    assert.equal(
      parsed.events.find((e) => e.kind === "compaction")!.idleAttribution,
      "notice",
    );
    const owner = parsed.profiles.find((p) => p.harness === "claude")!
      .idleDirectories![project]!;
    assert.equal(owner.value, false);
    assert.equal(owner.threshold, 100000);
    assert.equal(
      owner.owner,
      join(project, ".claude", "settings.local.json") + ":idleCompaction",
    );
    assert.equal(owner.ttl!.main.value, "5m");
    const report = analyze({
      scan: parsed,
      catalog: await prices(),
      now: 100 * MINUTE,
    });
    assert.doesNotMatch(JSON.stringify(report), /Compacted while idle/);
    assert.match(formatReport(report), /Idle compaction/);
    assert.match(formatReport(report, { details: true }), /idle-compaction/);
    await writeFile(
      file,
      [
        rows[0],
        rows[1],
        {
          ...common,
          type: "user",
          uuid: "compact-summary",
          parentUuid: "boundary",
          timestamp: new Date(1500).toISOString(),
          isCompactSummary: true,
          message: { role: "user", content: "SYNTHETIC_PRIVATE_SUMMARY" },
        },
        { ...rows[2], parentUuid: "compact-summary" },
      ]
        .map((r) => JSON.stringify(r))
        .join("\n") + "\n",
    );
    const linked = await scan(w.options);
    assert.equal(
      linked.events.find((e) => e.kind === "compaction")!.idleAttribution,
      "notice",
    );
    await writeFile(
      file,
      rows
        .slice(0, 2)
        .map((r) => JSON.stringify(r))
        .join("\n") + "\n",
    );
    const absent = await scan(w.options);
    assert.equal(
      absent.events.find((e) => e.kind === "compaction")!.idleAttribution,
      "unknown",
    );
    // A notice on another branch must not relabel an old boundary, even in the same session.
    for (const trigger of ["auto", "manual"]) {
      await writeFile(
        file,
        [
          rows[0],
          {
            ...rows[1],
            compactMetadata: { ...rows[1]!.compactMetadata, trigger },
          },
          {
            ...rows[2],
            parentUuid: "missing-boundary",
            timestamp: new Date(2 * 3600000).toISOString(),
          },
        ]
          .map((r) => JSON.stringify(r))
          .join("\n") + "\n",
      );
      const orphan = await scan(w.options);
      assert.equal(
        orphan.events.find((e) => e.kind === "compaction")!.idleAttribution,
        "unknown",
      );
    }
    await writeFile(
      file,
      JSON.stringify({ ...rows[2], timestamp: undefined }) + "\n",
    );
    await assert.rejects(scan(w.options), /missing idle notice timestamp/);

    const nested = join(project, "nested");
    await mkdir(nested);
    await writeFile(
      file,
      [
        rows[0],
        {
          ...rows[0],
          cwd: nested,
          timestamp: new Date(60 * MINUTE).toISOString(),
          message: { ...rows[0]!.message, id: "selected" },
        },
      ]
        .map((r) => JSON.stringify(r))
        .join("\n") + "\n",
    );
    const selected = await scan({ ...w.options, since: MINUTE });
    const profile = selected.profiles.find((p) => p.harness === "claude")!;
    assert.equal(profile.idleDirectories![project]!.ttl!.main.value, "5m");
    const selectedReport = analyze({
      scan: selected,
      catalog: await prices(),
      now: 200 * MINUTE,
    });
    const idle = selectedReport.checks.find(
      (c) => c.check === "idle-compaction",
    )!;
    assert.equal(idle.check, "idle-compaction");
    assert.doesNotMatch(
      idle.currentSettings.unknown.join(" "),
      /policy changes within a conversation/,
    );
  } finally {
    await w.dispose();
  }
});

test("a net-zero tie retains either current toggle", async () => {
  for (const current of [false, true]) {
    const s = data([
      request("before", 100, 312500),
      request("return", 200, 312500),
    ]);
    s.requests[0]!.parts[0]!.usage = claudeUsage({
      read: 312500,
      write: 0,
      output: 3125,
    });
    s.requests[1]!.parts[0]!.usage = claudeUsage({
      read: 0,
      write: 296875,
      output: 0,
      ttl: "1h",
    });
    s.events[0]!.preTokens = 312500;
    s.since = 100 * MINUTE;
    s.profiles[0]!.idle!.value = current;
    const c = await check(s);
    near(c.afterRecommendations.deltaUsd, 0);
    assert.equal(c.recommended, current);
    assert.equal(c.action, "keep");
  }
});

test("later observed compaction reprices carried summary input once and converges the paths", async () => {
  const later = request("later-summary", 220, 0);
  later.parts[0]!.kind = "compaction";
  later.parts[0]!.usage = claudeUsage({ read: 302000, write: 0, output: 2000 });
  const b = event({
    id: "later",
    session: "target",
    origins: [{ session: "target", location: { file: "synthetic", line: 6 } }],
    time: 220.5 * MINUTE,
    request: later.id,
    preTokens: 302000,
  });
  const follow = request("follow", 210, 1000);
  follow.parts[0]!.usage = claudeUsage({
    read: 301000,
    write: 1000,
    output: 0,
    ttl: "1h",
  });
  const s = data(
    [
      request("before", 100),
      request("return", 200, 301000),
      follow,
      later,
      request("converged", 221, 20000),
    ],
    [b],
  );
  const c = await check(s, 222);
  near(c.afterRecommendations.deltaUsd, -2.252);
  near(
    c.afterRecommendations.cost.on.totalUsd! -
      c.afterRecommendations.cost.off.totalUsd!,
    -2.252,
  );
});

test("recorded final idle summaries are charged without needing a resumed-prefix estimate", async () => {
  const summary = request("idle-summary", 154, 0);
  summary.parts[0]!.kind = "compaction";
  summary.parts[0]!.usage = claudeUsage({
    read: 300000,
    write: 0,
    output: 2000,
  });
  const b = event({
    id: "applied-final",
    session: "target",
    origins: [{ session: "target", location: { file: "synthetic", line: 8 } }],
    time: 155 * MINUTE,
    request: summary.id,
    idleAttribution: "notice",
  });
  const c = await check(data([request("final", 100), summary], [b]), 200);
  near(c.afterRecommendations.deltaUsd, 0.1);
  assert.equal(c.recommended, false);
  assert.equal(
    c.afterRecommendations.periods.find((p) => p.request === "final")!.outcome,
    "applied",
  );
});

test("context/cohort changes after a hypothetical summary stay unpriced, and active requests are ineligible", async () => {
  for (const mode of ["decrease", "model", "active"] as const) {
    const before = request("before", 100);
    const back = request("return", 200, mode === "decrease" ? 150000 : 301000);
    if (mode === "model") back.parts[0]!.model = "claude-haiku-5-5";
    if (mode === "active") before.endTime = 170 * MINUTE;
    const c = await check(data([before, back]));
    if (mode === "active") {
      near(c.afterRecommendations.deltaUsd, 0);
      assert.equal(
        c.afterRecommendations.periods.find((p) => p.request === "before")!
          .status,
        "ineligible",
      );
    } else {
      assert.equal(c.afterRecommendations.deltaUsd, null);
      assert.equal(c.action, "advisory");
    }
  }
});

test("selected windows do not charge pre-window trigger estimates or generation", async () => {
  const s = data([request("before", 100), request("return", 200, 301000)]);
  s.since = 160 * MINUTE;
  const c = await check(s);
  near(c.afterRecommendations.deltaUsd, 0);
  assert.equal(
    c.afterRecommendations.periods.some((p) => p.request === "before"),
    false,
  );
});
