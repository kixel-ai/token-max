import { test } from "node:test";
import assert from "node:assert/strict";
import { join, resolve } from "node:path";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { timeoutControls } from "../src/controls.js";
import { terminalArguments, timeoutChecks } from "../src/timeouts.js";
import { scan } from "../src/reader.js";
import { analyze } from "../src/replay.js";
import { formatReport } from "../src/report.js";
import type { Scan, TtlCheckResult } from "../src/model.js";
import { makeRequest, prices, workspace } from "./helpers.js";

const start = Date.parse("2026-11-01T00:00:00Z");
const iso = (ms: number) => new Date(start + ms).toISOString();
function control(
  config: Record<string, unknown> = {},
  env: Record<string, unknown> = {},
) {
  return timeoutControls({
    harness: "claude",
    scopes: [{ file: "user", config }],
    env,
  });
}
async function terminalScan() {
  const w = await workspace();
  const s = await scan({ ...w.options, claudeDirs: [w.claude] });
  const p = s.profiles[0]!;
  p.ttl.main = {
    value: "5m",
    owner: "user:promptCacheTtl",
    reason: "synthetic",
  };
  p.timeout = control();
  s.requests = [
    makeRequest({
      profiles: [p.id],
      cwd: w.home,
      client: "cli",
      version: "2.1.319",
    }),
  ];
  return { w, s, p };
}
function checks(
  s: Scan,
  catalog: Awaited<ReturnType<typeof prices>>,
  ttls: TtlCheckResult[] = [],
) {
  return timeoutChecks({
    scan: s,
    catalog,
    ttls,
    since: undefined,
    now: start + 86400000,
  });
}
async function rows(file: string, values: unknown[]) {
  await writeFile(file, values.map((v) => JSON.stringify(v)).join("\n") + "\n");
}

test("foreground ceiling uses both documented limits and native settings env precedence", () => {
  assert.equal(control().ceilingMs, 600000);
  const c = control(
    {
      env: {
        BASH_MAX_TIMEOUT_MS: "200000",
        BASH_DEFAULT_TIMEOUT_MS: "3600000",
      },
    },
    { BASH_MAX_TIMEOUT_MS: "3300000", BASH_DEFAULT_TIMEOUT_MS: "120000" },
  );
  assert.equal(c.ceilingMs, 3600000);
  assert.equal(c.maximumMs, 200000);
  assert.equal(c.defaultOwner, "user:env.BASH_DEFAULT_TIMEOUT_MS");
  assert.equal(c.maximumOwner, "user:env.BASH_MAX_TIMEOUT_MS");
  assert.equal(control({}, { BASH_MAX_TIMEOUT_MS: "broken" }).resolved, false);
  for (const value of [0, "0", "-1", "bad", "1.5", "9007199254740992"]) {
    assert.throws(
      () => control({ env: { BASH_MAX_TIMEOUT_MS: value } }),
      /invalid terminal timeout/,
    );
  }
});

test("cap equality changes, below-TTL keeps, and lowering MAX alone cannot bypass DEFAULT", async () => {
  const { w, s, p } = await terminalScan();
  try {
    const c = await prices();
    for (const [maximum, defaultMs, action] of [
      [299999, 120000, "keep"],
      [300000, 120000, "change"],
      [100000, 300000, "change"],
      [3300000, 120000, "change"],
    ] as const) {
      p.timeout = control({
        env: {
          BASH_MAX_TIMEOUT_MS: String(maximum),
          BASH_DEFAULT_TIMEOUT_MS: String(defaultMs),
        },
      });
      const check = checks(s, c)[0]!;
      assert.equal(check.action, action);
      assert.equal(check.savingsUsd, null);
      assert.equal(check.observed.capHits, null);
      assert.equal(check.evaluationTtl, "5m");
    }
    p.ttl.main.value = "1h";
    p.timeout = control({ env: { BASH_MAX_TIMEOUT_MS: "3600000" } });
    assert.equal(checks(s, c)[0]!.action, "change");
  } finally {
    await w.dispose();
  }
});

test("foreground guidance follows applicable recommended TTL while preserving recorded baseline", async () => {
  const { w, s, p } = await terminalScan();
  try {
    const c = await prices();
    const ttl = {
      profile: p.id,
      bucket: "main",
      owner: p.ttl.main.owner,
      current: "5m",
      recommended: "1h",
      action: "change",
    } as TtlCheckResult;
    const result = checks(s, c, [ttl])[0]!;
    assert.equal(result.action, "keep");
    assert.equal(result.currentTtl, "5m");
    assert.equal(result.evaluationTtl, "1h");
    assert.equal(result.atOrAboveCurrentTtl, true);
    assert.equal(result.atOrAboveEvaluationTtl, false);
    assert.equal(
      checks(s, c, [{ ...ttl, action: "advisory" }])[0]!.action,
      "change",
    );
    assert.equal(
      checks(s, c, [{ ...ttl, owner: "another-project" }])[0]!.evaluationTtl,
      "5m",
    );
    p.ttl.main.value = "1h";
    const shorter = {
      ...ttl,
      current: "1h",
      recommended: "5m",
    } as TtlCheckResult;
    assert.equal(checks(s, c, [shorter])[0]!.action, "change");
    assert.match(result.baseline, /recorded.*tier/);
  } finally {
    await w.dispose();
  }
});

test("unknown owners, TTLs, clients and versions cannot cause automatic timeout changes", async () => {
  const { w, s, p } = await terminalScan();
  try {
    const c = await prices();
    for (const [client, version] of [
      [null, "2.1.319"],
      ["custom", "2.1.319"],
      ["cli", null],
      ["cli", "2.1.314"],
      ["claude-desktop", "2.1.319"],
    ] as const) {
      Object.assign(s.requests[0]!, { client, version });
      assert.equal(checks(s, c)[0]!.action, "advisory");
    }
    Object.assign(s.requests[0]!, { client: "sdk-ts", version: "2.1.319" });
    assert.equal(checks(s, c)[0]!.action, "change");
    p.timeout!.resolved = false;
    assert.equal(checks(s, c)[0]!.action, "advisory");
    p.timeout!.resolved = true;
    p.ttl.main.value = null;
    assert.equal(checks(s, c)[0]!.action, "advisory");
  } finally {
    await w.dispose();
  }
});

test("unresolved subagent frontmatter is advisory until an explicit bucket TTL change supersedes it", async () => {
  const { w, s, p } = await terminalScan();
  try {
    s.requests[0]!.agent = "subagent";
    p.ttl.subagent = {
      value: "5m",
      owner: "subagent-default",
      reason: "unobserved frontmatter",
      resolved: false,
    };
    p.timeout = control({ env: { BASH_MAX_TIMEOUT_MS: "3600000" } });
    const c = await prices();
    const before = checks(s, c)[0]!;
    assert.equal(before.action, "advisory");
    assert.equal(before.atOrAboveCurrentTtl, null);
    const ttl = {
      profile: p.id,
      bucket: "subagent",
      current: "5m",
      recommended: "1h",
      owner: "subagent-default",
      action: "change",
    } as TtlCheckResult;
    const after = checks(s, c, [ttl])[0]!;
    assert.equal(after.action, "change");
    assert.equal(after.evaluationTtl, "1h");
    assert.equal(after.atOrAboveCurrentTtl, null);
  } finally {
    await w.dispose();
  }
});

test("only native structural poll targets count; keystrokes, custom tools, removed TaskOutput and Bash text do not", () => {
  const codex = {
    harness: "codex",
    client: "codex_cli_rs",
    version: "0.167.0",
    tool: "write_stdin",
  } as const;
  const poll = terminalArguments({
    ...codex,
    input: JSON.stringify({ session_id: 42, chars: "", yield_time_ms: 300000 }),
  })!;
  assert.equal(poll.timeoutMs, 300000);
  assert.match(poll.poll!, /^[a-f0-9]{64}$/);
  assert.equal(
    terminalArguments({
      ...codex,
      input: { session_id: 42, chars: "SYNTHETIC_KEYSTROKES" },
    }),
    undefined,
  );
  assert.equal(
    terminalArguments({
      ...codex,
      tool: "mcp__write_stdin",
      input: { session_id: 42 },
    }),
    undefined,
  );
  assert.equal(
    terminalArguments({
      ...codex,
      client: "custom",
      input: { session_id: 42 },
    }),
    undefined,
  );
  assert.throws(
    () => terminalArguments({ ...codex, input: "{" }),
    /invalid terminal tool arguments/,
  );
  const claude = {
    harness: "claude",
    client: "cli",
    version: "2.1.276",
    tool: "TaskOutput",
    input: { task_id: "SYNTHETIC_TASK", timeout: 300000 },
  } as const;
  assert.ok(terminalArguments(claude)?.poll);
  assert.equal(terminalArguments({ ...claude, version: "2.1.277" }), undefined);
  const bash = terminalArguments({
    ...claude,
    version: "2.1.319",
    tool: "Bash",
    input: { command: "SYNTHETIC_POLL_LOOP", timeout: 300000 },
  })!;
  assert.equal(bash.poll, null);
  assert.equal(bash.background, false);
});

test("direct native parent/result linkage prices observed misses once without calling them savings or cap hits", async () => {
  const w = await workspace();
  try {
    const file = join(w.claude, "projects/project/terminal.jsonl");
    const meta = {
      sessionId: "s",
      cwd: w.home,
      entrypoint: "cli",
      version: "2.1.319",
    };
    const u = {
      input_tokens: 100,
      output_tokens: 10,
      cache_read_input_tokens: 1000,
      cache_creation_input_tokens: 2000,
      cache_creation: {
        ephemeral_5m_input_tokens: 0,
        ephemeral_1h_input_tokens: 2000,
      },
      service_tier: "standard",
      speed: "fast",
      inference_geo: "us",
    };
    const assistant = (
      id: string,
      uuid: string,
      time: number,
      extra: Record<string, unknown> = {},
    ) => ({
      ...meta,
      type: "assistant",
      uuid,
      timestamp: iso(time),
      message: { id, model: "claude-opus-5-5", usage: u, ...extra },
    });
    const initial = assistant("a", "a", 0, {
      content: [
        {
          type: "tool_use",
          id: "one",
          name: "Bash",
          input: { command: "SYNTHETIC_PRIVATE_COMMAND", timeout: 300000 },
        },
        {
          type: "tool_use",
          id: "two",
          name: "Bash",
          input: {
            command: "SYNTHETIC_PRIVATE_COMMAND_2",
            run_in_background: true,
          },
        },
      ],
    });
    const result = {
      ...meta,
      type: "user",
      uuid: "result",
      parentUuid: "a",
      timestamp: iso(300000),
      toolUseResult: {
        interrupted: true,
        stderr: "timeout SYNTHETIC_PRIVATE_OUTPUT",
      },
      message: {
        content: [
          {
            type: "tool_result",
            tool_use_id: "one",
            content: "SYNTHETIC_PRIVATE_OUTPUT",
          },
          {
            type: "tool_result",
            tool_use_id: "two",
            content: "SYNTHETIC_PRIVATE_OUTPUT",
          },
        ],
      },
    };
    const resume = { ...assistant("b", "b", 300001), parentUuid: "result" };
    await rows(file, [initial, result, resume, resume]);
    const original = await readFile(file, "utf8");
    const s = await scan({ ...w.options, claudeDirs: [w.claude] });
    const check = checks(s, await prices())[0]!;
    assert.equal(check.observed.calls, 2);
    assert.equal(check.observed.capHits, null);
    assert.equal(check.observed.foregroundRequests, 1);
    assert.equal(check.observed.backgroundRequests, 1);
    assert.equal(check.observed.linkedRequests.length, 1);
    assert.equal(check.calls[0]!.elapsedMs, 300000);
    assert.ok(
      Math.abs(
        check.observed.missCost.totalUsd! - ((100 * 8 + 2000 * 16) * 1.1) / 1e6,
      ) < 1e-12,
    );
    assert.equal(check.savingsUsd, null);
    const report = analyze({
      scan: s,
      catalog: await prices(),
      now: start + 86400000,
    });
    for (const text of [
      JSON.stringify(report),
      formatReport(report),
      formatReport(report, { details: true }),
    ]) {
      assert.doesNotMatch(text, /SYNTHETIC_PRIVATE/);
    }
    assert.match(
      formatReport(report),
      /not proven expiry or removable savings/,
    );
    assert.equal(await readFile(file, "utf8"), original);
    const catalog = await prices();
    catalog.models.clear();
    assert.equal(checks(s, catalog)[0]!.observed.missCost.totalUsd, null);
    s.requests[1]!.parts[0]!.usage.input = null;
    const partial = checks(s, await prices())[0]!.observed.missCost;
    assert.equal(partial.totalUsd, null);
    assert.ok(
      partial.knownUsd > 0,
      "known write charges survive unknown ordinary input",
    );
    s.requests[1]!.toolResults = [];
    assert.equal(
      checks(s, await prices())[0]!.observed.missCost.totalUsd,
      null,
      "adjacency and elapsed time cannot replace native parent linkage",
    );
    const selected = await scan({
      ...w.options,
      claudeDirs: [w.claude],
      since: start + 300001,
    });
    assert.equal(
      checks(selected, await prices())[0]!.observed.missCost.totalUsd,
      null,
    );
    await rows(file, [
      initial,
      result,
      {
        ...result,
        message: {
          content: [
            { type: "tool_result", tool_use_id: "conflicting-native-identity" },
          ],
        },
      },
      resume,
    ]);
    const conflicted = await scan({ ...w.options, claudeDirs: [w.claude] });
    assert.equal(conflicted.requests[1]!.toolResults, undefined);
    assert.equal(
      checks(conflicted, await prices())[0]!.observed.missCost.totalUsd,
      null,
    );
  } finally {
    await w.dispose();
  }
});

test("Codex repeated empty polls are observed across the same opaque session without promoting requested tiers", async () => {
  const w = await workspace();
  try {
    const file = join(w.codex, "sessions/terminal.jsonl");
    await rows(file, [
      {
        type: "session_meta",
        timestamp: iso(0),
        payload: {
          id: "s",
          cwd: w.home,
          originator: "codex_cli_rs",
          source: "cli",
          cli_version: "0.167.0",
          model_provider: "openai",
        },
      },
      {
        type: "turn_context",
        timestamp: iso(0),
        payload: { model: "gpt-6.1-sol", service_tier: "fast" },
      },
      ...[1, 2, 3].map((n) => ({
        type: "response_item",
        timestamp: iso(n * 1000),
        payload: {
          type: "function_call",
          name: "write_stdin",
          call_id: `call-${n}`,
          arguments: JSON.stringify({
            session_id: n === 3 ? 8 : 7,
            chars: n === 3 ? "SYNTHETIC_KEYS" : "",
            yield_time_ms: 300000,
          }),
        },
      })),
      {
        type: "token_usage_record",
        timestamp: iso(5000),
        payload: {
          response_id: "r",
          usage: {
            input_tokens: 1000,
            output_tokens: 10,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
          },
        },
      },
    ]);
    const s = await scan({ ...w.options, codexDirs: [w.codex] });
    const check = checks(s, await prices())[0]!;
    assert.equal(check.action, "advisory");
    assert.equal(check.recommended, null);
    assert.equal(check.control.pollMs, 300000);
    assert.equal(check.observed.pollCalls, 2);
    assert.equal(check.observed.repeatedPollCalls, 1);
    assert.equal(check.observed.missCost.totalUsd, null);
    assert.equal(check.savingsUsd, null);
    assert.equal(s.requests[0]!.parts[0]!.tier, null);
    assert.equal(s.requests[0]!.requestedSpeed!.tier, "fast");
    assert.doesNotMatch(JSON.stringify(check), /SYNTHETIC_KEYS/);
    s.requests = [];
    const absent = checks(s, await prices())[0]!;
    assert.equal(absent.observed.pollCalls, 2);
    assert.equal(absent.observed.missCost.totalUsd, null);
  } finally {
    await w.dispose();
  }
});

test("current scoped owners respect active profile, managed and project overrides; missing directories stay unresolved", async () => {
  const w = await workspace();
  try {
    const repo = join(w.home, "repo");
    await mkdir(join(repo, ".git"), { recursive: true });
    await mkdir(join(repo, ".claude"));
    await writeFile(
      join(w.claude, "settings.json"),
      JSON.stringify({
        promptCacheTtl: "5m",
        env: { BASH_MAX_TIMEOUT_MS: "3600000" },
      }),
    );
    await writeFile(
      join(repo, ".claude/settings.local.json"),
      JSON.stringify({ env: { BASH_MAX_TIMEOUT_MS: "200000" } }),
    );
    await writeFile(
      w.options.managedSettingsFile,
      JSON.stringify({ env: { BASH_DEFAULT_TIMEOUT_MS: "300000" } }),
    );
    await rows(join(w.claude, "projects/project/main.jsonl"), [
      {
        type: "assistant",
        uuid: "a",
        sessionId: "s",
        cwd: repo,
        entrypoint: "cli",
        version: "2.1.319",
        timestamp: iso(0),
        message: {
          id: "a",
          model: "claude-opus-5-5",
          usage: {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
          },
        },
      },
    ]);
    const options = {
      ...w.options,
      env: { CLAUDE_CONFIG_DIR: w.claude, BASH_MAX_TIMEOUT_MS: "3300000" },
      claudeDirs: [w.claude],
    };
    const s = await scan(options);
    const c = checks(s, await prices())[0]!;
    assert.equal(c.control.ceilingMs, 300000);
    assert.equal(
      c.control.maximumOwner,
      join(repo, ".claude/settings.local.json") + ":env.BASH_MAX_TIMEOUT_MS",
    );
    assert.equal(
      c.control.defaultOwner,
      w.options.managedSettingsFile + ":env.BASH_DEFAULT_TIMEOUT_MS",
    );
    assert.equal(c.action, "change");
    await writeFile(
      join(repo, ".claude/settings.local.json"),
      JSON.stringify({
        env: {
          BASH_MAX_TIMEOUT_MS: "200000",
          CLAUDE_CODE_PROMPT_CACHE_TTL: "1h",
        },
      }),
    );
    const nativeTtl = await scan({
      ...options,
      env: { ...options.env, CLAUDE_CODE_PROMPT_CACHE_TTL: "5m" },
    });
    assert.equal(
      checks(nativeTtl, await prices())[0]!.currentTtl,
      "1h",
      "settings TTL overrides inherited shell for the native terminal policy",
    );
    assert.equal(checks(nativeTtl, await prices())[0]!.action, "keep");
    const other = join(w.home, ".claude-other");
    await mkdir(join(other, "projects"), { recursive: true });
    const profiles = await scan({ ...options, claudeDirs: [w.claude, other] });
    assert.equal(
      profiles.profiles.find((p) => p.root === other)!.timeout!.maximumMs,
      600000,
    );
    s.requests[0]!.cwd = join(w.home, "deleted");
    const { discoverIdleOwners } = await import("../src/discovery.js");
    await discoverIdleOwners(options, s.profiles, s.requests);
    assert.equal(checks(s, await prices())[0]!.action, "advisory");
  } finally {
    await w.dispose();
  }
});

test("Codex named/legacy and project timeout ownership remains explicit and malformed controls fail", async () => {
  const w = await workspace();
  try {
    const config = join(w.codex, "config.toml");
    const options = {
      ...w.options,
      codexDirs: [w.codex],
      codexSystemConfigFile: join(w.home, "system.toml"),
      codexRequirementsFile: join(w.home, "requirements.toml"),
    };
    await writeFile(
      options.codexSystemConfigFile,
      "background_terminal_max_timeout=123000",
    );
    assert.equal((await scan(options)).profiles[0]!.timeout!.pollMs, 123000);
    const inherited = timeoutControls({
      harness: "codex",
      scopes: [{ file: "user", config: {} }],
      env: { background_terminal_max_timeout: "987000" },
    });
    assert.equal(
      inherited.pollMs,
      300000,
      "Codex has no documented environment binding for this TOML key",
    );
    await writeFile(config, "background_terminal_max_timeout=234000");
    assert.equal((await scan(options)).profiles[0]!.timeout!.pollMs, 234000);
    await writeFile(
      join(w.codex, "named.config.toml"),
      "background_terminal_max_timeout=456000",
    );
    assert.equal((await scan(options)).profiles[0]!.timeout!.resolved, false);
    await writeFile(
      join(w.codex, "named.config.toml"),
      'background_terminal_max_timeout="private-invalid"',
    );
    await assert.rejects(scan(options), /invalid terminal timeout/);
    await writeFile(join(w.codex, "named.config.toml"), "");
    await writeFile(
      config,
      "[profiles.legacy]\nbackground_terminal_max_timeout=-1",
    );
    await assert.rejects(scan(options), /invalid terminal timeout/);
    await writeFile(config, "");
    const repo = join(w.home, "repo");
    const cwd = join(repo, "nested");
    await mkdir(join(repo, ".git"), { recursive: true });
    await mkdir(join(repo, ".codex"), { recursive: true });
    await mkdir(join(cwd, ".codex"), { recursive: true });
    await writeFile(
      join(repo, ".codex/config.toml"),
      "background_terminal_max_timeout=123000",
    );
    const nearest = join(cwd, ".codex/config.toml");
    await writeFile(nearest, "background_terminal_max_timeout=456000");
    await rows(join(w.codex, "sessions/main.jsonl"), [
      {
        type: "session_meta",
        timestamp: iso(0),
        payload: { id: "s", cwd, source: "cli", cli_version: "0.167.0" },
      },
      {
        type: "token_usage_record",
        timestamp: iso(1000),
        payload: {
          response_id: "r",
          usage: { input_tokens: 1, output_tokens: 1 },
        },
      },
    ]);
    const scoped = await scan(options);
    assert.equal(scoped.profiles[0]!.timeoutDirectories![cwd]!.owner, nearest);
    assert.equal(scoped.profiles[0]!.timeoutDirectories![cwd]!.resolved, false);
  } finally {
    await w.dispose();
  }
});

test("subagent and main caps use distinct TTLs and scopes; unlinked and partial-priced charges remain unknown", async () => {
  const { w, s, p } = await terminalScan();
  try {
    const c = await prices();
    p.ttl.subagent = {
      value: "1h",
      owner: "subagent-owner",
      reason: "synthetic",
    };
    s.requests.push(
      makeRequest({ ...s.requests[0]!, id: "subagent", agent: "subagent" }),
    );
    assert.deepEqual(
      checks(s, c).map((r) => [r.bucket, r.action]),
      [
        ["main", "change"],
        ["subagent", "keep"],
      ],
    );
    p.timeoutDirectories = {
      [w.home]: {
        ...control({ env: { BASH_MAX_TIMEOUT_MS: "200000" } }),
        ttl: {
          ...p.ttl,
          main: { value: "1h", owner: "project-ttl", reason: "synthetic" },
        },
      },
    };
    assert.equal(checks(s, c)[0]!.currentTtl, "1h");
    assert.equal(checks(s, c)[0]!.action, "keep");
    assert.equal(checks(s, c)[0]!.observed.missCost.totalUsd, null);
  } finally {
    await w.dispose();
  }
});

test("built CLI timeout recommendations are read-only; malformed complete records fail atomically", async () => {
  const w = await workspace();
  try {
    const settingsFile = join(w.claude, "settings.json");
    const settings = JSON.stringify({
      promptCacheTtl: "5m",
      env: { BASH_MAX_TIMEOUT_MS: "3600000" },
    });
    await writeFile(settingsFile, settings);
    const file = join(w.claude, "projects/project/a.jsonl");
    await rows(file, [
      {
        type: "assistant",
        uuid: "a",
        sessionId: "s",
        cwd: w.home,
        entrypoint: "cli",
        version: "2.1.319",
        timestamp: iso(0),
        message: {
          id: "a",
          model: "claude-opus-5-5",
          usage: {
            input_tokens: 1,
            output_tokens: 1,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            service_tier: "standard",
          },
        },
      },
    ]);
    const args = [
      resolve("dist/src/cli.js"),
      "--claude-dir",
      w.claude,
      "--prices-file",
      resolve("test/fixtures/prices.json"),
      "--json",
    ];
    const run = () =>
      spawnSync(process.execPath, args, {
        encoding: "utf8",
        env: { ...process.env, HOME: w.home, CLAUDE_CONFIG_DIR: w.claude },
      });
    const result = run();
    assert.equal(result.status, 1, result.stderr);
    assert.equal(
      JSON.parse(result.stdout).checks.find(
        (c: { check: string }) => c.check === "terminal-timeout",
      ).action,
      "change",
    );
    assert.equal(await readFile(settingsFile, "utf8"), settings);
    await writeFile(file, '{"private":"SYNTHETIC_SECRET",bad}\n');
    const failed = run();
    assert.equal(failed.status, 2);
    assert.equal(failed.stdout, "");
    assert.doesNotMatch(failed.stderr, /SYNTHETIC_SECRET/);
  } finally {
    await w.dispose();
  }
});
