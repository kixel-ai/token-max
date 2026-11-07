import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { writeFile } from "node:fs/promises";
import type { Profile, Request, Scan, StructuralEvent } from "../src/model.js";
import { formatReport } from "../src/cli.js";
import { analyze as analyzeReport } from "../src/replay.js";
import { scan, usage } from "../src/reader.js";
import {
  claudeUsage,
  fixture,
  makeRequest,
  prices,
  workspace,
} from "./helpers.js";

function analyze(options: Parameters<typeof analyzeReport>[0]) {
  const report = analyzeReport(options);
  return {
    ...report,
    checks: report.checks.filter((c) => c.check === "claude-cache-ttl"),
  };
}

function data(requests: Request[], events: StructuralEvent[] = []): Scan {
  const profile: Profile = {
    id: "profile",
    harness: "claude",
    root: "synthetic",
    aliases: ["synthetic"],
    histories: [],
    settingsFile: "synthetic/settings.json",
    ttl: {
      main: {
        value: "5m",
        owner: "settings.json:promptCacheTtl",
        reason: "current user setting",
      },
      subagent: {
        value: "5m",
        owner: "settings.json:subagentPromptCacheTtl",
        reason: "current user setting",
      },
    },
    provider: null,
    settings: {},
    compaction: {
      default: {
        value: null,
        owner: "synthetic",
        reason: "unknown",
        applicable: false,
      },
      models: {},
    },
  };
  return {
    exposure: { components: [], requests: [], invocations: [], visibility: [] },
    snapshotStartedAt: new Date(0).toISOString(),
    snapshotFinishedAt: new Date(0).toISOString(),
    since: null,
    requests,
    events,
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
      issues: [],
    },
  };
}

test("TTL compares aggregate net cost in either direction and holds new prefix writes", async () => {
  const c = await prices();
  const first = makeRequest();
  const next = makeRequest({
    id: "next",
    time: 600000,
    endTime: 600000,
    parts: [{ ...first.parts[0]!, usage: claudeUsage({ write: 1100 }) }],
  });
  const s = data([first, next]);
  const r = analyze({ scan: s, catalog: c, now: 1200000 });
  const check = r.checks[0]!;
  assert.equal(check.recommended, "1h");
  assert.equal(check.action, "change");
  assert.ok(
    Math.abs(
      check.cost["1h"].totalUsd! - (1100 * 8 + 1000 * 0.2 + 20 * 20) / 1e6,
    ) < 1e-10,
  );
  assert.ok(check.savingsUsd! > 0);
  assert.ok(check.monthlySavingsUsd! > 0);
  assert.ok(
    check.sensitivity.warmReuseBreakEvenFraction! > 0 &&
      check.sensitivity.warmReuseBreakEvenFraction! < 1,
  );
  assert.ok(check.sensitivity.coldPrefix1hUsd! > check.cost["5m"].totalUsd!);
  const burst = data([first, { ...next, time: 60000, endTime: 60000 }]);
  burst.profiles[0]!.ttl.main.value = "1h";
  const other = analyze({ scan: burst, catalog: c, now: 1200000 }).checks[0]!;
  assert.equal(other.recommended, "5m");
  assert.equal(other.action, "change");
  const tie = data([
    makeRequest({
      parts: [
        { ...first.parts[0]!, usage: claudeUsage({ write: 0, read: 1000 }) },
      ],
    }),
  ]);
  assert.equal(
    analyze({ scan: tie, catalog: c, now: 1 }).checks[0]!.action,
    "keep",
  );
});

test("5m rewrites a 1h read after 5 minutes; sessions and models do not share inferred cache", async () => {
  const c = await prices();
  const first = makeRequest({
    parts: [{ ...makeRequest().parts[0]!, usage: claudeUsage({ ttl: "1h" }) }],
  });
  const read = makeRequest({
    id: "read",
    time: 600000,
    endTime: 600000,
    parts: [
      {
        ...first.parts[0]!,
        usage: claudeUsage({ write: 100, read: 1000, ttl: "1h" }),
      },
    ],
  });
  const result = analyze({
    scan: data([first, read]),
    catalog: c,
    now: 1200000,
  }).checks[0]!;
  assert.equal(result.recommended, "1h");
  assert.ok(result.cost["5m"].totalUsd! > result.cost["1h"].totalUsd!);
  const unshared = makeRequest({
    id: "separate",
    session: "different",
    time: 600000,
    endTime: 600000,
  });
  assert.equal(
    analyze({ scan: data([makeRequest(), unshared]), catalog: c, now: 1200000 })
      .checks[0]!.recommended,
    "5m",
  );
});

test("a large returned period decides the aggregate despite more losing burst periods", async () => {
  const c = await prices();
  const requests: Request[] = [];
  for (let i = 0; i < 4; i++)
    requests.push(
      makeRequest({
        id: `small-${i}`,
        session: `small-${i}`,
        parts: [
          { ...makeRequest().parts[0]!, usage: claudeUsage({ write: 10 }) },
        ],
      }),
    );
  requests.push(
    makeRequest({
      id: "big1",
      parts: [
        { ...makeRequest().parts[0]!, usage: claudeUsage({ write: 1000000 }) },
      ],
    }),
  );
  requests.push(
    makeRequest({
      id: "big2",
      time: 600000,
      endTime: 600000,
      parts: [
        { ...makeRequest().parts[0]!, usage: claudeUsage({ write: 1000000 }) },
      ],
    }),
  );
  assert.equal(
    analyze({ scan: data(requests), catalog: c, now: 1200000 }).checks[0]!
      .recommended,
    "1h",
  );
});

test("unknown current billing or version stays advisory, while missing prices prevent a priced winner", async () => {
  const c = await prices();
  const s = data([makeRequest()]);
  s.profiles[0]!.ttl.main.value = null;
  assert.equal(
    analyze({ scan: s, catalog: c, now: 100 }).checks[0]!.action,
    "advisory",
  );
  s.profiles[0]!.ttl.main.value = "1h";
  s.requests[0]!.version = "2.1.100";
  assert.equal(
    analyze({ scan: s, catalog: c, now: 100 }).checks[0]!.action,
    "advisory",
  );
  s.requests[0]!.parts[0]!.model = "private-internal";
  const result = analyze({ scan: s, catalog: c, now: 100 }).checks[0]!;
  assert.equal(result.recommended, null);
  assert.equal(result.evidence, "unknown");
});

test("summary boundaries remain structural; unmatched proxies are charged once in the outside-main bucket", async () => {
  const w = await workspace();
  try {
    await fixture(
      join(w.claude, "projects/project/main.jsonl"),
      "claude.jsonl",
    );
    const s = await scan(w.options);
    const r = analyze({
      scan: s,
      catalog: await prices(),
      now: Date.parse("2026-11-02"),
    });
    assert.equal(r.scan.requests.length, 2);
    assert.equal(r.compactions.length, 1);
    assert.equal(r.compactions[0]!.evidence, "estimated");
    assert.equal(r.compactions[0]!.cost.totalUsd, 0.0064);
    const main = r.checks.find((c) => c.bucket === "main")!;
    const sub = r.checks.find((c) => c.bucket === "subagent")!;
    assert.equal(main.summaryCost["5m"].totalUsd, 0);
    assert.equal(sub.summaryCost["5m"].totalUsd, 0.0064);
    assert.equal(sub.cost["5m"].totalUsd, 0.0064);
    assert.match(r.compactions[0]!.assumptions.join(" "), /generation time/);
  } finally {
    await w.dispose();
  }
});

test("Claude compaction/advisor iterations replace overlapping top-level usage and retain model attribution", async () => {
  const w = await workspace();
  try {
    const u = {
      input_tokens: 0,
      output_tokens: 10,
      cache_read_input_tokens: 0,
      cache_creation_input_tokens: 100,
      cache_creation: {
        ephemeral_5m_input_tokens: 100,
        ephemeral_1h_input_tokens: 0,
      },
      service_tier: "standard",
      speed: "standard",
    };
    const records = [
      {
        type: "assistant",
        sessionId: "s",
        timestamp: "2026-11-01T00:00:00Z",
        version: "2.1.314",
        requestRef: "summary-api",
        message: {
          id: "m",
          model: "claude-opus-5-5",
          usage: {
            ...u,
            service_tier: "standard",
            speed: "standard",
            inference_geo: "global",
            server_tool_use: {},
            iterations: [
              { ...u, type: "message" },
              { ...u, type: "compaction" },
              { ...u, type: "advisor_message", model: "claude-haiku-5-5" },
            ],
          },
        },
      },
      {
        type: "system",
        subtype: "compact_boundary",
        uuid: "b",
        sessionId: "s",
        timestamp: "2026-11-01T00:00:01Z",
        requestRef: "summary-api",
        compactMetadata: { preTokens: 1000, postTokens: 100 },
      },
    ];
    await writeFile(
      join(w.claude, "projects/project/main.jsonl"),
      records.map((r) => JSON.stringify(r)).join("\n") + "\n",
    );
    const s = await scan(w.options);
    assert.equal(s.requests[0]!.parts.length, 3);
    const r = analyze({
      scan: s,
      catalog: await prices(),
      now: Date.parse("2026-11-02"),
    });
    assert.equal(r.compactions[0]!.request, s.requests[0]!.id);
    assert.equal(r.compactions[0]!.evidence, "measured");
    assert.equal(r.compactions[0]!.ttlCost["5m"].totalUsd, 0);
    assert.equal(
      r.checks.find((c) => c.bucket === "subagent")!.summaryCost["5m"].totalUsd,
      0,
    );
    assert.ok(r.historical[0]!.cost.totalUsd! < 0.002);
  } finally {
    await w.dispose();
  }
});

test("default windows use observed write-TTL changes while explicit windows take precedence", async () => {
  const c = await prices();
  const first = makeRequest();
  const changed = makeRequest({
    id: "change",
    time: 600000,
    endTime: 600000,
    parts: [{ ...first.parts[0]!, usage: claudeUsage({ ttl: "1h" }) }],
  });
  const s = data([first, changed]);
  assert.equal(
    analyze({ scan: s, catalog: c, now: 1200000 }).checks[0]!.since,
    600000,
  );
  assert.equal(
    analyze({ scan: s, catalog: c, now: 1200000, since: 0 }).checks[0]!.since,
    0,
  );
});

test("scenario-invariant unknown server fees do not veto a priced TTL difference", async () => {
  const first = makeRequest({ serverTools: { code_execution_requests: 1 } });
  const next = makeRequest({
    ...first,
    id: "later",
    time: 600000,
    endTime: 600000,
  });
  const result = analyze({
    scan: data([first, next]),
    catalog: await prices(),
    now: 1200000,
  }).checks[0]!;
  assert.equal(result.action, "change");
  assert.equal(result.recommended, "1h");
  assert.equal(result.cost["5m"].totalUsd, null);
  assert.equal(result.cost["1h"].totalUsd, null);
  assert.ok(result.savingsUsd! > 0);
  const report = analyze({
    scan: data([first, next]),
    catalog: await prices(),
    now: 1200000,
  });
  assert.match(formatReport(report), /identical fixed costs remain unknown/);
  // Unknown request usage is not scenario-invariant and must still block a winner.
  next.parts[0]!.usage = { ...next.parts[0]!.usage, write: null, read: null };
  assert.equal(
    analyze({
      scan: data([first, next]),
      catalog: await prices(),
      now: 1200000,
    }).checks[0]!.recommended,
    null,
  );
});

test("mixed outside-main agent TTLs retain the available history window", async () => {
  const first = makeRequest({
    agent: "subagent",
    source: "agent:builtin:general-purpose",
  });
  const helper = makeRequest({
    id: "helper",
    session: "helper-session",
    agent: "subagent",
    source: "session_title",
    time: 600000,
    endTime: 600000,
    parts: [{ ...first.parts[0]!, usage: claudeUsage({ ttl: "1h" }) }],
  });
  const again = { ...first, id: "again", time: 1200000, endTime: 1200000 };
  const result = analyze({
    scan: data([first, helper, again]),
    catalog: await prices(),
    now: 1800000,
  }).checks[0]!;
  assert.equal(result.since, 0);
  assert.equal(result.requests, 3);
  assert.match(result.windowBasis, /mixed/i);
});

test("a selected boundary retains its own pre-window pricing context", async () => {
  const w = await workspace();
  try {
    const file = join(w.claude, "projects/project/main.jsonl");
    await fixture(file, "claude.jsonl");
    const fs = await import("node:fs/promises");
    await writeFile(
      file,
      (await fs.readFile(file, "utf8")).replace(
        "2026-11-01T00:20:00Z",
        "2026-11-02T00:20:00Z",
      ),
    );
    const s = await scan({ ...w.options, since: Date.parse("2026-11-02") });
    assert.equal(s.requests.length, 0);
    const result = analyze({
      scan: s,
      catalog: await prices(),
      since: Date.parse("2026-11-02"),
      now: Date.parse("2026-11-03"),
    });
    assert.equal(result.compactions[0]!.cost.totalUsd, 0.0064);
  } finally {
    await w.dispose();
  }
});

test("an unknown unmatched summary proxy cancels without hiding its unknown full cost", async () => {
  const first = makeRequest({
    agent: "subagent",
    source: "agent:builtin:general-purpose",
  });
  const next = makeRequest({
    ...first,
    id: "next",
    time: 600000,
    endTime: 600000,
  });
  const event: StructuralEvent = {
    id: "unpriced-summary",
    origins: [
      { session: "other-session", location: { file: "synthetic", line: 1 } },
    ],
    harness: "claude",
    session: "other-session",
    time: 300000,
    kind: "compaction",
    profiles: ["profile"],
    location: { file: "synthetic", line: 1 },
    request: null,
    model: null,
    version: "2.1.314",
    client: "sdk-ts",
    cwd: null,
    trigger: "auto",
    precomputed: true,
    preTokens: null,
    postTokens: null,
    durationMs: null,
    tool: null,
    toolId: null,
    settings: {},
    idleAttribution: "unknown",
    pricing: null,
  };
  const result = analyze({
    scan: data([first, next], [event]),
    catalog: await prices(),
    now: 1200000,
  }).checks[0]!;
  assert.equal(result.recommended, "1h");
  assert.equal(result.action, "change");
  assert.equal(result.summaryCost["5m"].totalUsd, null);
  assert.equal(result.fixedCost.totalUsd, null);
  assert.equal(result.cost["1h"].totalUsd, null);
  assert.ok(result.savingsUsd! > 0);
});

test("unknown TTL comparison reports missing linked usage before missing tier", async () => {
  const r = makeRequest({
    evidence: "unknown",
    issues: [
      "api request has no linked assistant usage; failed/in-flight/hidden request billing is unknown",
    ],
    parts: [
      { ...makeRequest().parts[0]!, usage: usage({}, "claude"), tier: null },
    ],
  });
  const check = analyze({
    scan: data([r]),
    catalog: await prices(),
    now: 1200000,
  }).checks[0]!;
  assert.equal(check.recommended, null);
  assert.match(check.uncertainty.join(" "), /no linked assistant usage/);
  assert.match(
    check.comparisonCost["5m"].unknown.join(" "),
    /usage is not recorded/,
  );
});

function bracketedAttempt() {
  const first = makeRequest({ id: "before" });
  const attempt = makeRequest({
    id: "attempt",
    evidence: "unknown",
    time: 600000,
    endTime: 600000,
    reference: "attempt-ref",
    attempt: { maxOutputTokens: 100 },
    serverTools: null,
    parts: [
      {
        ...first.parts[0]!,
        usage: usage({}, "claude"),
        tier: null,
        speed: null,
        geo: null,
      },
    ],
    issues: [
      "api request has no linked assistant usage; failed/in-flight/hidden request billing is unknown",
    ],
  });
  const after = makeRequest({ id: "after", time: 1200000, endTime: 1200000 });
  return { first, attempt, after };
}

test("unlinked attempts use matching bracket evidence without converting observed usage into invented counts", async () => {
  const { first, attempt, after } = bracketedAttempt();
  const report = analyze({
    scan: data([first, attempt, after]),
    catalog: await prices(),
    now: 1800000,
  });
  const check = report.checks[0]!;
  assert.equal(check.recommended, "1h");
  assert.equal(check.action, "change");
  assert.equal(check.requests, 3);
  assert.equal(check.usageEstimates.length, 1);
  assert.deepEqual(check.usageEstimates[0]!.basis, [first.id, after.id]);
  assert.equal(check.usageEstimates[0]!.part!.usage.input, 1000);
  assert.equal(check.usageEstimates[0]!.part!.usage.output, null);
  assert.equal(attempt.parts[0]!.usage.input, null);
  assert.equal(attempt.evidence, "unknown");
  assert.equal(report.historical[0]!.cost.totalUsd, null);
  assert.equal(check.cost["5m"].totalUsd, null);
  assert.ok(check.savingsUsd! > 0);
  // A losing cold-cache sensitivity must accompany, not veto, the point estimate.
  assert.ok(
    check.sensitivity.unlinked!.coldCache["1h"].totalUsd! >
      check.sensitivity.unlinked!.coldCache["5m"].totalUsd!,
  );
  assert.ok(check.sensitivity.unlinked!.noBilledAttempt["5m"].totalUsd! > 0);
  assert.match(formatReport(report), /unlinked.*estimated/i);
});

test("growing-context retry evidence supports an estimate within the recorded cache lifetime", async () => {
  const requests = Array.from({ length: 20 }, (_, index) =>
    makeRequest({
      id: `measured-${index}`,
      time: index * 1200000,
      endTime: index * 1200000,
      parts: [
        {
          ...makeRequest().parts[0]!,
          usage: claudeUsage({ write: 100000 + index * 1000 }),
        },
      ],
    }),
  );
  const { attempt } = bracketedAttempt();
  const before = requests.at(-1)!;
  attempt.time = attempt.endTime = before.time + 300;
  const after = makeRequest({
    id: "retry",
    time: before.time + 3300,
    endTime: before.time + 3300,
    parts: [
      {
        ...before.parts[0]!,
        usage: claudeUsage({ read: 119000, write: 1000 }),
      },
    ],
  });
  const report = analyze({
    scan: data([...requests, attempt, after]),
    catalog: await prices(),
    now: after.time + 1000,
  });
  const check = report.checks[0]!;
  const estimate = check.usageEstimates[0]!;
  assert.equal(estimate.evidence, "estimated");
  assert.deepEqual(estimate.basis, [before.id, after.id]);
  assert.equal(estimate.part!.usage.input, 119000);
  assert.equal(estimate.part!.usage.read, 119000);
  assert.equal(estimate.part!.usage.write, 0);
  assert.equal(estimate.part!.usage.output, null);
  assert.equal(check.recommended, "1h");
  assert.ok(check.sensitivity.unlinked);
  assert.equal(attempt.evidence, "unknown");
  assert.equal(attempt.parts[0]!.usage.input, null);
  assert.equal(report.historical[0]!.cost.totalUsd, null);
  after.time = after.endTime = before.time + 300000;
  const expired = analyze({
    scan: data([...requests, attempt, after]),
    catalog: await prices(),
    now: after.time + 1000,
  }).checks[0]!;
  assert.equal(expired.usageEstimates[0]!.evidence, "unknown");
  assert.equal(expired.recommended, null);
});

test("retry estimates attribute additional downstream reads, never the following request's own writes", async () => {
  const { first, attempt, after } = bracketedAttempt();
  attempt.time = attempt.endTime = 300;
  after.time = after.endTime = 3300;
  after.parts[0]!.usage = claudeUsage({ read: 1100, write: 100 });
  const report = analyze({
    scan: data([first, attempt, after]),
    catalog: await prices(),
    now: 60000,
  });
  const part = report.checks[0]!.usageEstimates[0]!.part!;
  assert.equal(part.usage.input, 1100);
  assert.equal(part.usage.read, 1000);
  assert.equal(part.usage.write, 100);
  assert.equal(after.parts[0]!.usage.write, 100);
  after.parts[0]!.usage = claudeUsage({ read: 900, write: 300 });
  const invalidated = analyze({
    scan: data([first, attempt, after]),
    catalog: await prices(),
    now: 60000,
  }).checks[0]!;
  assert.equal(invalidated.usageEstimates[0]!.evidence, "unknown");
  assert.equal(invalidated.recommended, null);
  first.parts[0]!.usage = claudeUsage({ input: 1000, write: 0 });
  after.parts[0]!.usage = claudeUsage({ input: 1200, write: 200 });
  const noReadEvidence = analyze({
    scan: data([first, attempt, after]),
    catalog: await prices(),
    now: 60000,
  }).checks[0]!;
  assert.equal(noReadEvidence.usageEstimates[0]!.evidence, "unknown");
});

test("pre-window estimated cache seeds retain provenance and sensitivity without adding their charges", async () => {
  const first = makeRequest({
    id: "first",
    parts: [
      {
        ...makeRequest().parts[0]!,
        usage: claudeUsage({ read: 1000, write: 0 }),
      },
    ],
  });
  const { attempt } = bracketedAttempt();
  attempt.time = attempt.endTime = 240000;
  const after = { ...first, id: "after", time: 480000, endTime: 480000 };
  const other = makeRequest({
    id: "other",
    session: "other",
    time: 60000,
    endTime: 60000,
    parts: [{ ...first.parts[0]!, usage: claudeUsage({ write: 10 }) }],
  });
  const transition = makeRequest({
    ...other,
    id: "transition",
    time: 420000,
    endTime: 420000,
    parts: [
      { ...first.parts[0]!, usage: claudeUsage({ write: 10, ttl: "1h" }) },
    ],
  });
  const check = analyze({
    scan: data([first, other, attempt, transition, after]),
    catalog: await prices(),
    now: 600000,
  }).checks[0]!;
  assert.equal(check.since, 420000);
  assert.equal(check.requests, 2);
  assert.equal(check.usageEstimates[0]!.request, attempt.id);
  assert.ok(Math.abs(check.comparisonCost["5m"].totalUsd! - 0.00025) < 1e-10);
  assert.equal(check.recommended, "5m");
  const none = check.sensitivity.unlinked!.noBilledAttempt;
  assert.ok(none["1h"].totalUsd! < none["5m"].totalUsd!);
  assert.match(check.assumptions.join(" "), /pre-window/);
});

function preWindowAttempt(speed: string | null) {
  const { first, attempt, after } = bracketedAttempt();
  first.parts[0]!.usage = claudeUsage({ write: 1000, ttl: "1h" });
  attempt.time = attempt.endTime = 17 * 60000;
  attempt.parts[0]!.speed = speed;
  after.time = after.endTime = 20 * 60000;
  after.parts[0]!.usage = claudeUsage({ read: 1000, write: 200, ttl: "1h" });
  const other = makeRequest({
    id: "other",
    session: "other",
    source: "other",
    time: 60000,
    endTime: 60000,
    parts: [{ ...first.parts[0]!, usage: claudeUsage({ write: 10 }) }],
  });
  const transition = makeRequest({
    ...other,
    id: "transition",
    time: 18 * 60000,
    endTime: 18 * 60000,
    parts: [
      { ...first.parts[0]!, usage: claudeUsage({ write: 10, ttl: "1h" }) },
    ],
  });
  return { first, attempt, after, other, transition };
}

test("unsupported pre-window attempts cannot choose opposite winners from missing speed metadata", async () => {
  for (const speed of [null, "standard"]) {
    const { first, attempt, after, other, transition } =
      preWindowAttempt(speed);
    const check = analyze({
      scan: data([first, other, attempt, transition, after]),
      catalog: await prices(),
      now: 21 * 60000,
    }).checks[0]!;
    assert.equal(check.since, 18 * 60000);
    assert.equal(check.requests, 2);
    assert.equal(check.recommended, null, String(speed));
    assert.equal(check.action, "advisory");
    assert.equal(check.comparisonCost["5m"].totalUsd, null);
    assert.equal(check.comparisonCost["1h"].totalUsd === null, false);
    assert.equal(check.usageEstimates[0]!.request, attempt.id);
    assert.equal(check.usageEstimates[0]!.evidence, "unknown");
    assert.ok(
      check.cacheEvidence.some(
        (e) =>
          e.request === after.id &&
          e.ttl === "5m" &&
          e.outcome === "unknown" &&
          e.predecessors.some(
            (p) => p.request === attempt.id && p.state === "unknown",
          ),
      ),
    );
  }
});

test("measured missing speed preserves ambiguity across every possible cache cohort", async () => {
  const { first, attempt, after, other, transition } = preWindowAttempt(null);
  attempt.evidence = "measured";
  attempt.attempt = null;
  attempt.parts[0]!.usage = claudeUsage({ read: 1000, write: 0, ttl: "1h" });
  const fastBefore = makeRequest({
    ...first,
    id: "fast-before",
    time: 30000,
    endTime: 30000,
    parts: [{ ...first.parts[0]!, speed: "fast" }],
  });
  const fastAfter = makeRequest({
    ...after,
    id: "fast-after",
    time: 21 * 60000,
    endTime: 21 * 60000,
    parts: [{ ...after.parts[0]!, speed: "fast" }],
  });
  const check = analyze({
    scan: data([
      first,
      fastBefore,
      other,
      attempt,
      transition,
      after,
      fastAfter,
    ]),
    catalog: await prices(),
    now: 22 * 60000,
  }).checks[0]!;
  assert.equal(check.recommended, null);
  for (const request of [after.id, fastAfter.id])
    assert.ok(
      check.cacheEvidence.some(
        (e) =>
          e.request === request &&
          e.outcome === "unknown" &&
          e.predecessors.some((p) => p.request === attempt.id),
      ),
    );
});

test("unknown pre-window state can cancel when cache charges do not depend on its prefix", async () => {
  const { first, attempt, after, other, transition } = preWindowAttempt(null);
  after.parts[0]!.usage = claudeUsage({ write: 1500, ttl: "1h" });
  const check = analyze({
    scan: data([first, other, attempt, transition, after]),
    catalog: await prices(),
    now: 21 * 60000,
  }).checks[0]!;
  assert.equal(check.recommended, "5m");
  assert.equal(check.usageEstimates[0]!.evidence, "unknown");
  assert.ok(
    check.cacheEvidence.some(
      (e) =>
        e.state === "unknown" &&
        e.outcome === "invariant" &&
        e.predecessors.some((p) => p.request === attempt.id),
    ),
  );
});

test("a later known cache touch resolves an earlier unsupported predecessor before the selected window", async () => {
  const { first, attempt, after, other, transition } = preWindowAttempt(null);
  const reset = makeRequest({
    ...after,
    id: "known-reset",
    time: 18 * 60000,
    endTime: 18 * 60000,
  });
  transition.time = transition.endTime = 19 * 60000;
  const report = analyze({
    scan: data([first, other, attempt, reset, transition, after]),
    catalog: await prices(),
    now: 21 * 60000,
  });
  const check = report.checks[0]!;
  assert.equal(check.since, 19 * 60000);
  assert.equal(check.recommended, "5m");
  assert.equal(check.usageEstimates.length, 0);
  assert.equal(report.historical[0]!.cost.totalUsd, null);
});

test("unsupported attempts block only their affected comparison and stay present with reasons", async () => {
  const { first, attempt, after } = bracketedAttempt();
  after.parts = [{ ...after.parts[0]!, usage: claudeUsage({ write: 1200 }) }];
  const sub = makeRequest({
    id: "sub",
    agent: "subagent",
    source: "agent:builtin:general-purpose",
    session: "child",
  });
  const report = analyze({
    scan: data([first, attempt, after, sub]),
    catalog: await prices(),
    now: 1800000,
  });
  const main = report.checks.find((c) => c.bucket === "main")!;
  const outside = report.checks.find((c) => c.bucket === "subagent")!;
  assert.equal(main.recommended, null);
  assert.equal(main.requests, 3);
  assert.equal(main.usageEstimates[0]!.evidence, "unknown");
  assert.match(main.usageEstimates[0]!.reason, /matching/);
  assert.equal(outside.recommended, "5m");
  assert.equal(report.scan.requests.length, 4);
});

test("bracketing cannot cross changes of source, model, client, profile provenance, or compaction", async () => {
  const catalog = await prices();
  for (const change of [
    "source",
    "model",
    "client",
    "profiles",
    "boundary",
  ] as const) {
    const { first, attempt, after } = bracketedAttempt();
    const events: StructuralEvent[] = [];
    if (change === "source") after.source = "different-source";
    if (change === "model") after.parts[0]!.model = "claude-haiku-5-5";
    if (change === "client") after.client = "changed-entrypoint";
    if (change === "profiles") after.profiles = ["profile", "alias"];
    if (change === "boundary")
      events.push({
        id: "boundary",
        origins: [
          { session: "session", location: { file: "synthetic", line: 1 } },
        ],
        harness: "claude",
        session: "session",
        time: 900000,
        kind: "compaction",
        profiles: ["profile"],
        location: { file: "synthetic", line: 1 },
        request: null,
        model: null,
        version: "2.1.314",
        client: null,
        cwd: null,
        trigger: "auto",
        precomputed: false,
        preTokens: null,
        postTokens: null,
        durationMs: null,
        tool: null,
        toolId: null,
        settings: {},
        idleAttribution: "unknown",
        pricing: null,
      });
    const check = analyze({
      scan: data([first, attempt, after], events),
      catalog,
      now: 1800000,
    }).checks.find((c) => c.bucket === "main")!;
    assert.equal(check.recommended, null, change);
    assert.equal(check.usageEstimates[0]!.evidence, "unknown", change);
  }
});

test("unknown output and ordinary input cancel when cache charges have recorded evidence", async () => {
  const first = makeRequest();
  first.parts[0]!.usage.output = null;
  first.parts[0]!.usage.input = null;
  const after = makeRequest({ id: "after", time: 600000, endTime: 600000 });
  const check = analyze({
    scan: data([first, after]),
    catalog: await prices(),
    now: 1200000,
  }).checks[0]!;
  assert.equal(check.recommended, "1h");
  assert.equal(check.fixedCost.totalUsd, null);
  assert.equal(check.comparisonCost["1h"].totalUsd === null, false);
});

test("estimation needs both adjacent measured brackets and preserves other unsupported attempts", async () => {
  const catalog = await prices();
  for (const layout of ["first", "last", "consecutive", "mixed"] as const) {
    const { first, attempt, after } = bracketedAttempt();
    const other = makeRequest({
      ...attempt,
      id: "other",
      session: layout === "mixed" ? "unbracketed-session" : attempt.session,
      time: 900000,
      endTime: 900000,
    });
    const requests =
      layout === "first"
        ? [attempt, after]
        : layout === "last"
          ? [first, attempt]
          : [first, attempt, other, after];
    const check = analyze({ scan: data(requests), catalog, now: 1800000 })
      .checks[0]!;
    assert.equal(check.recommended, null, layout);
    assert.equal(check.requests, requests.length, layout);
    assert.equal(
      check.usageEstimates.filter((e) => e.evidence === "estimated").length,
      layout === "mixed" ? 1 : 0,
      layout,
    );
  }
});

test("unknown rates for a recorded cache-free part cancel, while an unknown context tier does not", async () => {
  const first = makeRequest({ id: "before" });
  const after = makeRequest({ id: "after", time: 600000, endTime: 600000 });
  const fixed = makeRequest({
    id: "fixed",
    session: "separate",
    parts: [
      {
        ...first.parts[0]!,
        model: "private-internal",
        usage: { ...claudeUsage({ write: 0 }), input: null, output: null },
      },
    ],
  });
  const result = analyze({
    scan: data([first, after, fixed]),
    catalog: await prices(),
    now: 1200000,
  }).checks[0]!;
  assert.equal(result.recommended, "1h");
  assert.equal(result.fixedCost.totalUsd, null);
  first.parts[0]!.model = "claude-haiku-5-5";
  first.parts[0]!.usage.input = null;
  assert.equal(
    analyze({
      scan: data([first, after]),
      catalog: await prices(),
      now: 1200000,
    }).checks[0]!.recommended,
    null,
  );
});

test("unknown kinds span both cache buckets while known message agents remain separate", async () => {
  const first = makeRequest({
    id: "first",
    parts: [
      {
        ...makeRequest().parts[0]!,
        kind: "compaction",
        usage: claudeUsage({ write: 100000, ttl: "1h" }),
      },
    ],
  });
  const middle = makeRequest({
    ...first,
    id: "middle",
    time: 17 * 60000,
    endTime: 17 * 60000,
    parts: [
      {
        ...first.parts[0]!,
        kind: "unknown",
        usage: claudeUsage({ read: 100000, write: 0, ttl: "1h" }),
      },
    ],
  });
  const after = makeRequest({
    ...first,
    id: "after",
    time: 20 * 60000,
    endTime: 20 * 60000,
    parts: [
      {
        ...first.parts[0]!,
        usage: claudeUsage({ read: 100000, write: 2000, ttl: "1h" }),
      },
    ],
  });
  const check = analyze({
    scan: data([first, middle, after]),
    catalog: await prices(),
    since: 19 * 60000,
  }).checks[0]!;
  assert.equal(check.bucket, "subagent");
  assert.equal(check.recommended, null);
  assert.ok(
    check.cacheEvidence.some((e) =>
      e.predecessors.some(
        (p) =>
          p.request === middle.id &&
          !p.cohortKnown &&
          p.prefixEvidence === "measured",
      ),
    ),
  );
  middle.parts[0]!.kind = "compaction";
  assert.equal(
    analyze({
      scan: data([first, middle, after]),
      catalog: await prices(),
      since: 19 * 60000,
    }).checks[0]!.recommended,
    "5m",
  );
  // Message cohorts in the same conversation cannot renew another agent's cache.
  for (const r of [first, middle, after]) r.parts[0]!.kind = "message";
  middle.agent = "subagent";
  assert.equal(
    analyze({
      scan: data([first, middle, after]),
      catalog: await prices(),
      since: 19 * 60000,
    }).checks[0]!.recommended,
    "1h",
  );
  middle.agent = "unknown";
  assert.equal(
    analyze({
      scan: data([first, middle, after]),
      catalog: await prices(),
      since: 19 * 60000,
    }).checks[0]!.recommended,
    null,
  );
});

test("unresolved selected part attribution stays unpriced in each possible bucket", async () => {
  const request = makeRequest();
  request.parts[0]!.kind = "unknown";
  const report = analyze({ scan: data([request]), catalog: await prices() });
  assert.equal(report.checks.length, 2);
  for (const check of report.checks) {
    assert.equal(check.comparisonCost["5m"].totalUsd, null);
    assert.match(
      check.comparisonCost["5m"].unknown.join(" "),
      /bucket is not established/,
    );
  }
  assert.equal(report.historical[0]!.cost.totalUsd, null);
  assert.match(
    report.historical[0]!.cost.unknown.join(" "),
    /iteration semantics/,
  );
});

test("estimated prefix provenance remains explicit when its cohort is incomplete", async () => {
  const { first, attempt, after } = bracketedAttempt();
  first.parts[0]!.speed = after.parts[0]!.speed = null;
  attempt.time = attempt.endTime = 60 * 60000;
  after.time = after.endTime = 65 * 60000;
  const check = analyze({
    scan: data([first, attempt, after]),
    catalog: await prices(),
    since: 64 * 60000,
  }).checks[0]!;
  assert.equal(check.usageEstimates[0]!.evidence, "estimated");
  assert.ok(
    check.cacheEvidence.some((e) =>
      e.predecessors.some(
        (p) =>
          p.request === attempt.id &&
          p.state === "unknown" &&
          p.prefixEvidence === "estimated" &&
          !p.cohortKnown &&
          /estimated/.test(p.reason),
      ),
    ),
  );
});

test("borrowed TTL splits never establish observed warmth for estimated attempts", async () => {
  for (const ttl of ["5m", "1h"] as const) {
    const results = [];
    for (const split of [true, false]) {
      const { first, attempt, after } = bracketedAttempt();
      for (const r of [first, after]) {
        r.parts[0]!.usage = claudeUsage({ read: 100000, write: 1000, ttl });
        if (!split) {
          r.parts[0]!.usage.write5m = null;
          r.parts[0]!.usage.write1h = null;
        }
      }
      attempt.time = attempt.endTime = 30 * 60000;
      after.time = after.endTime = 31 * 60000;
      const check = analyze({
        scan: data([first, attempt, after]),
        catalog: await prices(),
        now: 32 * 60000,
      }).checks[0]!;
      assert.equal(check.usageEstimates[0]!.evidence, "estimated");
      results.push(check.comparisonCost);
      assert.equal(check.recommended, "1h");
    }
    assert.deepEqual(results[0], results[1], ttl);
  }
});

test("unknown subagent iteration kinds cannot affect the known main bucket", async () => {
  const first = makeRequest({
    id: "main-first",
    parts: [
      { ...makeRequest().parts[0]!, usage: claudeUsage({ write: 50000 }) },
    ],
  });
  const after = makeRequest({
    id: "main-after",
    time: 20 * 60000,
    endTime: 20 * 60000,
    parts: [
      { ...first.parts[0]!, usage: claudeUsage({ read: 50000, write: 1000 }) },
    ],
  });
  const child = makeRequest({
    id: "child",
    agent: "subagent",
    time: 60000,
    endTime: 60000,
    parts: [
      {
        ...first.parts[0]!,
        kind: "unknown",
        usage: claudeUsage({ write: 20000 }),
      },
    ],
  });
  const baseline = analyze({
    scan: data([first, after]),
    catalog: await prices(),
  }).checks[0]!;
  const report = analyze({
    scan: data([first, child, after]),
    catalog: await prices(),
  });
  const main = report.checks.find((c) => c.bucket === "main")!;
  assert.equal(main.requests, 2);
  assert.deepEqual(main.comparisonCost, baseline.comparisonCost);
  assert.equal(main.recommended, "5m");
  assert.equal(
    report.checks.find((c) => c.bucket === "subagent")!.recommended,
    null,
  );
});
