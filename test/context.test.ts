import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdir, writeFile, readFile } from "node:fs/promises";
import { join } from "node:path";
import { scan } from "../src/reader.js";
import { analyze } from "../src/replay.js";
import { formatReport } from "../src/report.js";
import { priceExposureChange } from "../src/exposure.js";
import { makeRequest, prices, workspace } from "./helpers.js";

const at = (s: number) =>
  new Date(Date.UTC(2026, 10, 1, 0, 0, s)).toISOString();
function assistant(s: number, content: unknown[] = [], cwd?: string) {
  return {
    type: "assistant",
    timestamp: at(s),
    sessionId: "synthetic",
    version: "2.1.314",
    entrypoint: "cli",
    cwd,
    requestRef: `api-${s}`,
    message: {
      id: `message-${s}`,
      model: "claude-opus-5-5",
      content,
      usage: {
        input_tokens: 1000,
        output_tokens: 10,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        service_tier: "standard",
        speed: "standard",
        inference_geo: "global",
      },
    },
  };
}
function attachment(s: number, attachment: unknown) {
  return {
    type: "attachment",
    timestamp: at(s),
    sessionId: "synthetic",
    attachment,
  };
}
function wire(
  s: number,
  options: { system?: unknown; messages?: unknown[]; tools?: unknown[] } = {},
) {
  return {
    type: "api-request",
    timestamp: at(s),
    id: `api-${s}`,
    sessionId: "synthetic",
    params: {
      model: "claude-opus-5-5",
      system: options.system ?? "SYNTHETIC_PRIVATE_SYSTEM",
      messages: options.messages ?? [],
      tools: options.tools ?? [],
    },
  };
}
async function claudeReport(records: unknown[]) {
  const w = await workspace();
  try {
    await writeFile(
      join(w.claude, "projects/project/main.jsonl"),
      records.map((r) => JSON.stringify(r)).join("\n") + "\n",
    );
    const data = await scan({ ...w.options, claudeDirs: [w.claude] });
    return analyze({
      scan: data,
      catalog: await prices(),
      now: Date.parse(at(30)),
    });
  } finally {
    await w.dispose();
  }
}
const context = (report: ReturnType<typeof analyze>) =>
  report.checks.find((c) => c.check === "loaded-context")!;

test("wire block identities attribute instruction, rule, memory, server and result context without content or overlap", async () => {
  const report = await claudeReport([
    attachment(0, {
      type: "instructions",
      files: [
        {
          path: "/synthetic/CLAUDE.md",
          content: "SYNTHETIC_PRIVATE_INSTRUCTIONS",
        },
        {
          path: "/synthetic/.claude/rules/rust.md",
          content: "SYNTHETIC_PRIVATE_RULES",
        },
        { path: "/synthetic/MEMORY.md", content: "SYNTHETIC_PRIVATE_MEMORY" },
      ],
    }),
    attachment(0, {
      type: "hook_additional_context",
      content: ["SYNTHETIC_PRIVATE_HOOK"],
    }),
    attachment(0, {
      type: "mcp_instructions_delta",
      addedNames: ["mcp__sample"],
      addedBlocks: ["SYNTHETIC_PRIVATE_SERVER"],
      isInitial: true,
    }),
    assistant(1, [
      { type: "tool_use", id: "call", name: "mcp__sample__read", input: {} },
    ]),
    {
      type: "user",
      timestamp: at(2),
      sessionId: "synthetic",
      message: {
        content: [
          {
            type: "tool_result",
            tool_use_id: "call",
            content: "SYNTHETIC_PRIVATE_RESULT",
          },
        ],
      },
    },
    wire(3, {
      system: ["INSTRUCTIONS", "RULES", "MEMORY", "SERVER", "HOOK"].map(
        (s) => ({
          type: "text",
          text: `SYNTHETIC_PRIVATE_${s}`,
        }),
      ),
      tools: [
        {
          name: "mcp__sample__read",
          description: "SYNTHETIC_PRIVATE_SCHEMA",
          input_schema: { type: "object" },
        },
        { name: "Read", input_schema: { type: "object" } },
      ],
      messages: [
        {
          role: "user",
          content: [
            {
              type: "tool_result",
              tool_use_id: "call",
              content: "SYNTHETIC_PRIVATE_RESULT",
            },
          ],
        },
      ],
    }),
    assistant(3),
    wire(4, { system: "another synthetic input" }),
    assistant(4),
  ]);
  const c = context(report);
  for (const category of [
    "instructions",
    "rules",
    "memory",
    "mcp-instructions",
    "hooks",
    "mcp-schema",
    "mcp-result",
  ])
    assert.ok(
      c.attribution.some(
        (a) =>
          a.category === category &&
          a.wireRequests === 1 &&
          a.gross.knownUsd > 0,
      ),
      category,
    );
  assert.equal(
    c.attribution.filter((a) => a.category === "mcp-schema").length,
    1,
  );
  assert.ok(c.attribution.every((a) => a.net.totalUsd === null));
  assert.ok(c.servers.some((s) => s.observedCalls === 1 && s.completed === 1));
  assert.match(
    c.attribution.find((a) => a.category === "rules")!.source!,
    /rules\/rust.md$/,
  );
  assert.doesNotMatch(JSON.stringify(report), /SYNTHETIC_PRIVATE/);
  const text = formatReport(report, { details: true });
  assert.match(text, /Gross attributed overhead/);
  assert.match(text, /net unknown/);
  assert.doesNotMatch(text, /SYNTHETIC_PRIVATE/);
});

test("opaque schema hashes and deferred definitions never invent tokens, use or automatic disable", async () => {
  const report = await claudeReport([
    attachment(0, {
      type: "deferred_tools_record",
      entries: [
        {
          name: "mcp__sample__read",
          input_schema: { type: "object" },
          defer_loading: true,
        },
      ],
    }),
    {
      type: "world_state",
      timestamp: at(0),
      payload: {
        full: true,
        state: {
          top_level_tools: { mcp__other__read: "SYNTHETIC_PRIVATE_HASH" },
          plugins_instructions: true,
          apps_instructions: false,
        },
      },
    },
    assistant(1),
  ]);
  const c = context(report);
  assert.equal(c.action, "advisory");
  assert.equal(c.savingsUsd, null);
  assert.ok(
    c.attribution
      .filter((a) => a.category === "mcp-schema")
      .every((a) => a.gross.totalUsd === null && a.tokens === null),
  );
  assert.ok(
    c.tools.every(
      (t) =>
        t.automaticUnused === false &&
        t.usage === "no observed use" &&
        t.visibility === "incomplete",
    ),
  );
  assert.ok(
    c.attribution.some(
      (a) => a.category === "plugin-instructions" && a.characters === null,
    ),
  );
  assert.doesNotMatch(JSON.stringify(report), /SYNTHETIC_PRIVATE_HASH/);
});

test("only executed nested MCP completions count use; matching code references remain references", async () => {
  const w = await workspace();
  try {
    const records = [
      {
        type: "session_meta",
        timestamp: at(0),
        payload: {
          id: "session",
          cli_version: "0.167.0",
          originator: "codex_cli_rs",
          model_provider: "openai",
        },
      },
      {
        type: "turn_context",
        timestamp: at(0),
        payload: {
          model: "gpt-6.1-sol",
          turn_id: "turn",
          service_tier: "standard",
        },
      },
      {
        type: "world_state",
        timestamp: at(0),
        payload: {
          full: true,
          state: { tools: { mcp__sample: "SYNTHETIC_PRIVATE_DISCOVERY" } },
        },
      },
      {
        type: "response_item",
        timestamp: at(1),
        payload: {
          type: "function_call",
          name: "exec",
          namespace: "functions",
          call_id: "wrapper",
          arguments:
            "if (false) await tools.mcp__sample__unused({}); await tools.mcp__sample__read({});",
        },
      },
      {
        type: "event_msg",
        timestamp: at(2),
        payload: {
          type: "item_completed",
          item: {
            type: "McpToolCall",
            id: "actual",
            server: "sample",
            tool: "read",
            status: "failed",
            result: { isError: true, content: [] },
          },
        },
      },
      {
        type: "token_usage_record",
        timestamp: at(3),
        payload: {
          response_id: "response",
          turn_id: "turn",
          usage: {
            input_tokens: 1000,
            output_tokens: 10,
            cached_input_tokens: 500,
            cache_write_input_tokens: 0,
          },
        },
      },
    ];
    await writeFile(
      join(w.codex, "sessions/main.jsonl"),
      records.map((r) => JSON.stringify(r)).join("\n") + "\n",
    );
    const report = analyze({
      scan: await scan({ ...w.options, codexDirs: [w.codex] }),
      catalog: await prices(),
      now: Date.parse(at(30)),
    });
    const c = context(report);
    assert.ok(c.tools.some((t) => t.observedCalls === 0 && t.references === 1));
    assert.ok(
      c.tools.some(
        (t) => t.observedCalls === 1 && t.failed === 1 && t.completed === 0,
      ),
    );
    assert.equal(c.servers[0]!.observedCalls, 1);
    assert.ok(c.since! < Date.parse(at(3)));
    assert.equal(c.action, "advisory");
    assert.doesNotMatch(JSON.stringify(report), /SYNTHETIC_PRIVATE_DISCOVERY/);
  } finally {
    await w.dispose();
  }
});

test("skill context is linked to its owner and excluded from context charges", async () => {
  const report = await claudeReport([
    attachment(0, {
      type: "skill_listing",
      content: "- sample:skill: SYNTHETIC_PRIVATE_DESCRIPTION",
      names: ["sample:skill"],
    }),
    attachment(0, {
      type: "instructions",
      files: [
        {
          path: "/synthetic/CLAUDE.md",
          content: "SYNTHETIC_PRIVATE_INSTRUCTION",
        },
      ],
    }),
    assistant(1),
  ]);
  const c = context(report);
  assert.ok(c.sharedSkills.length > 0);
  assert.ok(c.attribution.every((a) => !a.category.startsWith("skill-")));
  assert.ok(c.sharedSkills.every((s) => s.owner === "skills"));
});

test("current Claude budgets require effective native scope and never fabricate historical dollars", async () => {
  const w = await workspace();
  try {
    const cwd = join(w.home, "project");
    await mkdir(join(cwd, ".git"), { recursive: true });
    await mkdir(join(cwd, ".claude"));
    const file = join(cwd, "CLAUDE.md");
    const body = "SYNTHETIC_PRIVATE_REQUIRED_INSTRUCTION\n".repeat(201);
    await writeFile(file, body);
    await writeFile(
      join(w.claude, "settings.json"),
      JSON.stringify({
        bashOutputMaxChars: 9000,
        env: { ENABLE_TOOL_SEARCH: "false" },
      }),
    );
    await writeFile(
      join(cwd, ".claude/settings.local.json"),
      JSON.stringify({ bashOutputMaxChars: 4500 }),
    );
    await writeFile(
      join(cwd, ".mcp.json"),
      JSON.stringify({
        mcpServers: {
          sample: {
            url: "https://example.invalid/SYNTHETIC_PRIVATE_SECRET",
            alwaysLoad: true,
          },
        },
      }),
    );
    await writeFile(
      join(w.claude, "projects/project/main.jsonl"),
      JSON.stringify(assistant(1, [], cwd)) + "\n",
    );
    const report = analyze({
      scan: await scan({ ...w.options, claudeDirs: [w.claude] }),
      catalog: await prices(),
      now: Date.parse(at(30)),
    });
    const c = context(report);
    assert.equal(c.action, "change");
    assert.equal(c.savingsUsd, null);
    assert.ok(
      c.guidance.some(
        (g) =>
          g.kind === "instruction-budget" &&
          g.owner === file &&
          g.action === "change",
      ),
    );
    assert.ok(
      c.controls.some((s) =>
        s.controls.some(
          (v) =>
            v.key === "bashOutputMaxChars" &&
            v.value === 4500 &&
            v.owner.includes("settings.local.json"),
        ),
      ),
    );
    assert.ok(c.guidance.some((g) => /Permission deny/.test(g.message)));
    assert.ok(c.guidance.some((g) => g.kind === "deferred-loading"));
    assert.equal(await readFile(file, "utf8"), body);
    assert.doesNotMatch(JSON.stringify(report), /SYNTHETIC_PRIVATE/);
    // A missing working directory cannot make a current effective finding.
    await writeFile(
      join(w.claude, "projects/project/main.jsonl"),
      JSON.stringify(assistant(1, [], join(cwd, "deleted"))) + "\n",
    );
    const unresolved = context(
      analyze({
        scan: await scan({ ...w.options, claudeDirs: [w.claude] }),
        catalog: await prices(),
      }),
    );
    assert.equal(unresolved.action, "advisory");
  } finally {
    await w.dispose();
  }
});

test("Codex omission and call blocking remain distinct; app/server surfaces combine without sending schemas", async () => {
  const w = await workspace();
  try {
    await writeFile(
      join(w.codex, "config.toml"),
      `tool_output_token_limit = 4000\n[mcp_servers.sample]\nenabled = true\nomit_tools_from = ["direct"]\ndisabled_tools = ["read"]\n[apps.example]\nenabled = true\nomit_tools_from = ["code_mode"]\n`,
    );
    const report = analyze({
      scan: await scan({ ...w.options, codexDirs: [w.codex] }),
      catalog: await prices(),
    });
    const c = context(report);
    assert.deepEqual(c.controls[0]!.servers[0]!.omitted, ["direct"]);
    assert.equal(c.controls[0]!.servers[0]!.blockedTools, 1);
    assert.deepEqual(c.controls[0]!.apps[0]!.omitted, ["code_mode"]);
    assert.equal(c.attribution.length, 0);
    assert.equal(c.action, "advisory");
    assert.ok(
      c.guidance.some((g) => /disabled_tools blocks calls/.test(g.message)),
    );
    assert.ok(
      c.guidance.some((g) => /tool_output_token_limit=4000/.test(g.message)),
    );
  } finally {
    await w.dispose();
  }
});

test("malformed selected context controls fail atomically with safe diagnostics", async () => {
  const w = await workspace();
  try {
    await writeFile(
      join(w.codex, "config.toml"),
      '[mcp_servers.sample]\nomit_tools_from = ["SYNTHETIC_PRIVATE_INVALID"]\n',
    );
    await assert.rejects(
      scan({ ...w.options, codexDirs: [w.codex] }),
      /invalid MCP omission setting/,
    );
    await writeFile(
      join(w.codex, "config.toml"),
      'tool_output_token_limit = "SYNTHETIC_PRIVATE_INVALID"\n',
    );
    await assert.rejects(
      scan({ ...w.options, codexDirs: [w.codex] }),
      /invalid context budget setting/,
    );
  } finally {
    await w.dispose();
  }
});

test("observed output overflow is advisory and never treated as net saved tokens or zero re-reads", async () => {
  const report = await claudeReport([
    assistant(1, [
      { type: "tool_use", id: "call", name: "mcp__sample__read", input: {} },
    ]),
    {
      type: "user",
      timestamp: at(2),
      sessionId: "synthetic",
      message: {
        content: [
          {
            type: "tool_result",
            tool_use_id: "call",
            content: "x".repeat(50001),
          },
        ],
      },
    },
    assistant(3),
  ]);
  const c = context(report);
  assert.ok(
    c.guidance.some(
      (g) => g.kind === "output-overflow" && /50001/.test(g.message),
    ),
  );
  assert.equal(c.action, "advisory");
  assert.equal(c.savingsUsd, null);
  assert.ok(c.attribution.every((a) => a.net.totalUsd === null));
});

test("marginal Codex write removal preserves 30m and 12h prices and leaves missing splits unknown", async () => {
  const r = makeRequest({
    harness: "codex",
    model: "gpt-6.1-sol",
    provider: "openai",
    api: "responses",
  });
  r.parts[0] = {
    ...r.parts[0]!,
    model: "gpt-6.1-sol",
    tier: "standard",
    speed: null,
    usage: {
      ...r.parts[0]!.usage,
      input: 1000,
      read: 200,
      write: 400,
      write5m: null,
      write1h: null,
      write30m: 200,
      write12h: 200,
    },
  };
  const catalog = await prices();
  const short = priceExposureChange({
    catalog,
    request: r,
    removedTokens: 100,
    addedTokens: 0,
    position: {
      read: 0,
      write5m: 0,
      write1h: 0,
      write30m: 100,
      write12h: 0,
      ordinary: 0,
    },
  });
  const long = priceExposureChange({
    catalog,
    request: r,
    removedTokens: 100,
    addedTokens: 0,
    position: {
      read: 0,
      write5m: 0,
      write1h: 0,
      write30m: 0,
      write12h: 100,
      ordinary: 0,
    },
  });
  assert.ok(short.gross.totalUsd! > 0);
  assert.ok(Math.abs(long.gross.totalUsd! - short.gross.totalUsd! * 2) < 1e-10);
  r.parts[0]!.usage.write30m = null;
  r.parts[0]!.usage.write12h = null;
  assert.equal(
    priceExposureChange({
      catalog,
      request: r,
      removedTokens: 100,
      addedTokens: 0,
    }).gross.totalUsd,
    null,
  );
});
