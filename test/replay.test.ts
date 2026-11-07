import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { writeFile } from "node:fs/promises";
import type { Profile, Request, Scan, StructuralEvent } from "../src/model.js";
import { formatReport } from "../src/cli.js";
import { analyze } from "../src/replay.js";
import { scan } from "../src/reader.js";
import {
  claudeUsage,
  fixture,
  makeRequest,
  prices,
  workspace,
} from "./helpers.js";

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
  };
  return {
    snapshotStartedAt: new Date(0).toISOString(),
    snapshotFinishedAt: new Date(0).toISOString(),
    requests,
    events,
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
