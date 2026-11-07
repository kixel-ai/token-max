import { test } from "node:test";
import assert from "node:assert/strict";
import { appendFile, mkdir, stat, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { discover } from "../src/discovery.js";
import { readSnapshot, scan, usage } from "../src/reader.js";
import { ScanError } from "../src/safe.js";
import { fixture, workspace } from "./helpers.js";

test("Claude stream snapshots deduplicate, link source and retain structural metadata only", async () => {
  const w = await workspace();
  try {
    await fixture(
      join(w.claude, "projects/project/main.jsonl"),
      "claude.jsonl",
    );
    const data = await scan(w.options);
    assert.equal(data.requests.length, 2);
    assert.equal(data.requests[0]!.parts[0]!.usage.output, 20);
    assert.equal(data.requests[0]!.source, "sdk");
    assert.equal(data.requests[0]!.locations.length, 2);
    assert.equal(data.events[0]!.precomputed, true);
    assert.equal(data.events[0]!.idleAttribution, "unknown");
    assert.equal(data.events[0]!.preTokens, 1100);
    assert.equal(data.events[0]!.postTokens, 100);
    assert.doesNotMatch(JSON.stringify(data), /SYNTHETIC_PRIVATE/);
  } finally {
    await w.dispose();
  }
});

test("Codex individual response usage replaces overlapping totals and duplicated response IDs", async () => {
  const w = await workspace();
  try {
    await fixture(join(w.codex, "sessions/main.jsonl"), "codex.jsonl");
    const data = await scan(w.options);
    assert.equal(data.requests.length, 1);
    assert.equal(data.requests[0]!.parts[0]!.usage.input, 1000);
    assert.equal(data.requests[0]!.parts[0]!.usage.reasoning, 20);
    assert.equal(data.requests[0]!.accounting, "individual");
    assert.equal(data.events.at(-1)!.kind, "turn-end");
    assert.doesNotMatch(JSON.stringify(data), /SYNTHETIC_PRIVATE/);
  } finally {
    await w.dispose();
  }
});

test("shared directories are read once, preserve profile aliases, and include nested subagents", async () => {
  const w = await workspace();
  try {
    const alternate = join(w.home, ".claude-work");
    await mkdir(alternate);
    await symlink(join(w.claude, "projects"), join(alternate, "projects"));
    await writeFile(
      join(alternate, "settings.json"),
      JSON.stringify({ promptCacheTtl: "1h" }),
    );
    await fixture(
      join(w.claude, "projects/project/main.jsonl"),
      "claude.jsonl",
    );
    await mkdir(join(w.claude, "projects/project/main/subagents"), {
      recursive: true,
    });
    await writeFile(
      join(w.claude, "projects/project/main/subagents/agent.jsonl"),
      JSON.stringify({
        type: "assistant",
        sessionId: "child",
        timestamp: "2026-11-01T00:00:00Z",
        message: {
          id: "child-message",
          model: "claude-opus-5-5",
          usage: {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
          },
        },
      }) + "\n",
    );
    const data = await scan(w.options);
    assert.equal(data.snapshots.length, 2);
    assert.equal(data.requests.length, 3);
    assert.ok(data.requests.every((r) => r.profiles.length === 2));
    assert.equal(data.requests.filter((r) => r.agent === "subagent").length, 1);
    const selected = await scan({ ...w.options, claudeDirs: [alternate] });
    assert.equal(selected.profiles.length, 1);
    assert.ok(
      selected.roots.some(
        (r) => r.path === w.claude && r.status === "excluded",
      ),
    );
    assert.equal(selected.profiles[0]!.ttl.main.value, "1h");
  } finally {
    await w.dispose();
  }
});

test("all lengths freeze before reading; unterminated final records and later appends are deferred", async () => {
  const w = await workspace();
  try {
    const file = join(w.claude, "projects/project/active.jsonl");
    await writeFile(
      file,
      '{"type":"custom-title"}\n{"private":"SYNTHETIC_PRIVATE',
    );
    const data = await discover(w.options);
    const snapshot = data.snapshots[0]!;
    await appendFile(file, '_REPLY"}\n{"type":"new"}\n');
    let records = 0;
    await readSnapshot(snapshot, () => {
      records++;
    });
    assert.equal(records, 1);
    assert.ok(snapshot.deferredBytes > 0);
    assert.equal(snapshot.lines, 1);
    const info = await stat(file);
    assert.ok(info.size > snapshot.bytes);
  } finally {
    await w.dispose();
  }
});

test("complete malformed records fail with safe file and line, while valid out-of-window usage is excluded before validation", async () => {
  const w = await workspace();
  try {
    const file = join(w.claude, "projects/project/bad.jsonl");
    await writeFile(
      file,
      '{"timestamp":"2020-01-01T00:00:00Z","type":"assistant","message":{"usage":{"input_tokens":"bad"}}}\n',
    );
    assert.equal(
      (await scan({ ...w.options, since: Date.parse("2026-01-01") })).requests
        .length,
      0,
    );
    await appendFile(
      file,
      '{"type":"assistant","private":"SYNTHETIC_PRIVATE_REPLY", broken}\n',
    );
    await assert.rejects(scan(w.options), (error: unknown) => {
      assert.ok(error instanceof ScanError);
      assert.equal(error.location?.line, 1);
      assert.doesNotMatch(error.message, /SYNTHETIC_PRIVATE/);
      return true;
    });
    await assert.rejects(
      scan({ ...w.options, since: Date.parse("2026-01-01") }),
      (error: unknown) => {
        assert.ok(error instanceof ScanError);
        assert.equal(error.location?.line, 2);
        return true;
      },
    );
  } finally {
    await w.dispose();
  }
});

test("legacy counters use per-request last usage, ignore repeated totals, and mark unverifiable resets unknown", async () => {
  const w = await workspace();
  try {
    const records: unknown[] = [
      {
        type: "session_meta",
        timestamp: "2026-11-01T00:00:00Z",
        payload: { id: "legacy", source: "cli", model_provider: "openai" },
      },
      {
        type: "turn_context",
        timestamp: "2026-11-01T00:00:00Z",
        payload: { model: "gpt-5.4", service_tier: "standard" },
      },
    ];
    const u = (n: number) => ({
      input_tokens: n,
      cached_input_tokens: 0,
      output_tokens: n,
      reasoning_output_tokens: 0,
    });
    for (const [minute, total, last] of [
      [1, 100, 100],
      [2, 100, 100],
      [3, 300, 200],
      [4, 10, null],
      [5, 30, null],
    ] as const)
      records.push({
        type: "event_msg",
        timestamp: `2026-11-01T00:0${minute}:00Z`,
        payload: {
          type: "token_count",
          info: {
            total_token_usage: u(total),
            ...(last === null ? {} : { last_token_usage: u(last) }),
          },
        },
      });
    await writeFile(
      join(w.codex, "sessions/legacy.jsonl"),
      records.map((r) => JSON.stringify(r)).join("\n") + "\n",
    );
    const data = await scan(w.options);
    assert.equal(data.requests.length, 4);
    assert.deepEqual(
      data.requests.map((r) => r.parts[0]!.usage.input),
      [100, 200, null, 20],
    );
    assert.ok(data.requests[2]!.issues.length);
  } finally {
    await w.dispose();
  }
});

test("usage validates subset invariants and preserves missing write counters", () => {
  const u = usage(
    {
      input_tokens: 100,
      output_tokens: 20,
      cached_input_tokens: 30,
      reasoning_output_tokens: 10,
    },
    "codex",
  );
  assert.equal(u.write, null);
  assert.equal(u.output, 20);
  assert.throws(
    () =>
      usage(
        { input_tokens: 10, cached_input_tokens: 11, output_tokens: 1 },
        "codex",
      ),
    ScanError,
  );
  assert.throws(
    () => usage({ input_tokens: 10, output_tokens: -1 }, "claude"),
    ScanError,
  );
  assert.throws(
    () =>
      usage(
        { input_tokens: 10, output_tokens: 5, reasoning_output_tokens: 6 },
        "codex",
      ),
    ScanError,
  );
});

test("current environment overrides are scoped to the active config root", async () => {
  const w = await workspace();
  try {
    const other = join(w.home, ".claude-other");
    await mkdir(join(other, "projects"), { recursive: true });
    await writeFile(
      join(other, "settings.json"),
      JSON.stringify({ promptCacheTtl: "1h", subagentPromptCacheTtl: "1h" }),
    );
    const data = await discover({
      home: w.home,
      env: {
        CLAUDE_CONFIG_DIR: w.claude,
        FORCE_PROMPT_CACHING_5M: "1",
        CLAUDE_CODE_PROMPT_CACHE_TTL: "1h",
      },
    });
    assert.equal(
      data.profiles.find((p) => p.root === w.claude)!.ttl.main.value,
      "5m",
    );
    assert.equal(
      data.profiles.find((p) => p.root === other)!.ttl.main.value,
      "1h",
    );
  } finally {
    await w.dispose();
  }
});

test("separate subagent files do not share inferred cache when parent session IDs match", async () => {
  const w = await workspace();
  try {
    const folder = join(w.claude, "projects/project/main/subagents");
    await mkdir(folder, { recursive: true });
    for (const name of ["agent-a", "agent-b"])
      await writeFile(
        join(folder, name + ".jsonl"),
        JSON.stringify({
          type: "assistant",
          sessionId: "same-parent",
          timestamp: "2026-11-01T00:00:00Z",
          cwd: "/synthetic/project",
          message: {
            id: name,
            model: "claude-opus-5-5",
            usage: {
              input_tokens: 1,
              output_tokens: 1,
              cache_read_input_tokens: 0,
              cache_creation_input_tokens: 0,
            },
          },
        }) + "\n",
      );
    const data = await scan(w.options);
    assert.equal(data.requests.length, 2);
    assert.notEqual(data.requests[0]!.session, data.requests[1]!.session);
    assert.equal(data.requests[0]!.cwd, "/synthetic/project");
  } finally {
    await w.dispose();
  }
});

test("unlinked API attempts expose missing usage instead of disappearing from coverage", async () => {
  const w = await workspace();
  try {
    await writeFile(
      join(w.claude, "projects/project/main.jsonl"),
      JSON.stringify({
        type: "api-request",
        id: "orphan",
        sessionId: "session",
        timestamp: "2026-11-01T00:00:00Z",
        querySource: "sdk",
        params: { model: "claude-opus-5-5" },
      }) + "\n",
    );
    const data = await scan(w.options);
    assert.equal(data.requests.length, 1);
    assert.equal(data.coverage.missingUsage, 1);
    assert.equal(data.requests[0]!.evidence, "unknown");
    assert.match(
      data.requests[0]!.issues.join(" "),
      /no linked assistant usage/,
    );
  } finally {
    await w.dispose();
  }
});

test("nested Codex settings and trigger attribution retain only whitelisted metadata", async () => {
  const w = await workspace();
  try {
    const file = join(w.codex, "sessions/main.jsonl");
    await fixture(file, "codex.jsonl");
    const raw = await import("node:fs/promises").then((fs) =>
      fs.readFile(file, "utf8"),
    );
    const lines = raw.trimEnd().split("\n");
    lines.splice(
      2,
      0,
      JSON.stringify({
        type: "event_msg",
        timestamp: "2026-11-01T00:00:00Z",
        payload: {
          type: "thread_settings_applied",
          thread_settings: {
            service_tier: "fast",
            model_provider_id: "openai",
            model: "gpt-6.1-sol",
            developer_instructions: "SYNTHETIC_PRIVATE",
          },
        },
      }),
    );
    await writeFile(file, lines.join("\n") + "\n");
    const data = await scan(w.options);
    assert.equal(data.requests[0]!.parts[0]!.tier, "fast");
    assert.equal(data.requests[0]!.source, "cli");
    assert.doesNotMatch(JSON.stringify(data), /SYNTHETIC_PRIVATE/);
  } finally {
    await w.dispose();
  }
});

test("large UTF-8 records span chunks without retaining content; a shrunk snapshot fails", async () => {
  const w = await workspace();
  try {
    const file = join(w.claude, "projects/project/large.jsonl");
    await writeFile(
      file,
      JSON.stringify({ type: "custom-title", title: "😀".repeat(40000) }) +
        "\n",
    );
    const discovery = await discover(w.options);
    let parsed = 0;
    await readSnapshot(discovery.snapshots[0]!, () => {
      parsed++;
    });
    assert.equal(parsed, 1);
    const original = await discover(w.options);
    await writeFile(file, "{}\n");
    await assert.rejects(
      readSnapshot(original.snapshots[0]!, () => {}),
      ScanError,
    );
    await assert.rejects(
      discover({ ...w.options, claudeDirs: [join(w.home, "absent")] }),
      ScanError,
    );
  } finally {
    await w.dispose();
  }
});
