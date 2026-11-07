import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdir, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { scan } from "../src/reader.js";
import { analyze } from "../src/replay.js";
import { formatReport } from "../src/report.js";
import { skillState, type SkillsCheckResult } from "../src/skills.js";
import { identity } from "../src/safe.js";
import { prices, workspace } from "./helpers.js";

const at = (n: number) =>
  new Date(Date.UTC(2026, 10, 1, 0, 0, n)).toISOString();
const time = (n: number) => Date.parse(at(n));
const attachment = (n: number, body: string) => ({
  type: "attachment",
  timestamp: at(n),
  sessionId: "s",
  version: "2.1.314",
  attachment: { type: "skill_listing", content: body },
});
const wire = (n: number, body: string, extra = "") => ({
  type: "api-request",
  id: `a${n}`,
  timestamp: at(n),
  sessionId: "s",
  params: {
    model: "claude-opus-5-5",
    system: [{ type: "text", text: `prefix\n${body}\nsuffix` }],
    messages: [{ role: "user", content: extra || "synthetic user input" }],
    tools: [],
  },
});
const assistant = (n: number, content: unknown[] = []) => ({
  type: "assistant",
  timestamp: at(n + 0.1),
  sessionId: "s",
  version: "2.1.314",
  entrypoint: "sdk-ts",
  requestRef: `a${n}`,
  message: {
    id: `m${n}`,
    model: "claude-opus-5-5",
    content,
    usage: {
      input_tokens: 100,
      cache_read_input_tokens: 200,
      cache_creation_input_tokens: 100,
      cache_creation: {
        ephemeral_5m_input_tokens: 100,
        ephemeral_1h_input_tokens: 0,
      },
      output_tokens: 10,
      service_tier: "standard",
      speed: "standard",
      inference_geo: "global",
    },
  },
});
const meta = (cwd: string) => ({
  type: "session_meta",
  timestamp: at(0),
  payload: {
    id: "s",
    cwd,
    model_provider: "openai",
    cli_version: "0.167.0",
    originator: "codex_cli_rs",
    source: "cli",
  },
});
const turn = (cwd: string) => ({
  type: "turn_context",
  timestamp: at(0),
  payload: {
    cwd,
    model: "gpt-6.1-sol",
    turn_id: "t",
    service_tier: "standard",
  },
});
const world = (n: number, body: string) => ({
  type: "world_state",
  timestamp: at(n),
  payload: { full: true, state: { host_skills: { body } } },
});
const billed = (n: number) => ({
  type: "token_usage_record",
  timestamp: at(n),
  payload: {
    response_id: `r${n}`,
    turn_id: "t",
    usage: {
      input_tokens: 1000,
      output_tokens: 10,
      cached_input_tokens: 500,
      cache_write_input_tokens: 0,
    },
  },
});
async function records(file: string, rows: unknown[]) {
  await writeFile(file, rows.map((r) => JSON.stringify(r)).join("\n") + "\n");
}
async function install(
  root: string,
  folder: string,
  name: string,
  description = "synthetic description",
) {
  const dir = join(root, folder);
  await mkdir(dir, { recursive: true });
  const file = join(dir, "SKILL.md");
  await writeFile(
    file,
    `---\nname: ${name}\ndescription: ${description}\n---\nSYNTHETIC_PRIVATE_BODY\n`,
  );
  return file;
}
function checks(report: ReturnType<typeof analyze>): SkillsCheckResult[] {
  return report.checks.filter((c) => c.check === "skills-catalog");
}

test("wire listings price rendered spans, body loads remain separate and no unused claim escapes visibility", async () => {
  const w = await workspace();
  try {
    const body =
      "Available skills\n- example:one: SYNTHETIC_PRIVATE_DESCRIPTION\n- second: synthetic";
    const loaded = "SYNTHETIC_PRIVATE_BODY";
    await records(join(w.claude, "projects/project/a.jsonl"), [
      attachment(0, body),
      wire(1, body),
      assistant(1, [
        {
          type: "tool_use",
          name: "Skill",
          id: "call",
          input: { skill: "example:one" },
        },
      ]),
      {
        type: "user",
        timestamp: at(2),
        sessionId: "s",
        message: {
          content: [
            { type: "tool_result", tool_use_id: "call", content: loaded },
          ],
        },
      },
      wire(3, body, loaded),
      assistant(3),
    ]);
    const data = await scan(w.options);
    const report = analyze({
      scan: data,
      catalog: await prices(),
      now: time(10),
    });
    const row = checks(report).find(
      (r) => r.target === identity("skill:example:one"),
    )!;
    assert.equal(row.exposure.wireRequests, 2);
    assert.equal(row.exposure.candidateRequests, 0);
    assert.equal(row.use.calls, 1);
    assert.equal(row.use.successfulCalls, 1);
    assert.ok(row.listing.gross.totalUsd! > 0);
    assert.equal(row.bodies.wireRequests, 1);
    assert.ok(row.bodies.gross.totalUsd! > 0);
    assert.equal(row.action, "advisory");
    assert.equal(row.use.completeWindow, false);
    assert.equal(row.savingsUsd, null);
    assert.equal(row.marginal[0]!.net.totalUsd, null);
    assert.equal(
      checks(report).find((r) => r.target === identity("skill:second"))!.use
        .calls,
      0,
    );
    assert.ok(!JSON.stringify(report).includes("SYNTHETIC_PRIVATE"));
    const text = formatReport(report, { details: true, width: 65 });
    assert.match(text, /no observed use/);
    assert.match(text, /Gross is not net/);
    assert.ok(!text.includes("SYNTHETIC_PRIVATE"));
  } finally {
    await w.dispose();
  }
});

test("wire text must match the complete recorded span, not just a skill name or attachment chronology", async () => {
  const w = await workspace();
  try {
    await records(join(w.claude, "projects/project/a.jsonl"), [
      attachment(0, "- one: complete catalog"),
      wire(1, "- one: different description"),
      assistant(1),
    ]);
    const report = analyze({
      scan: await scan(w.options),
      catalog: await prices(),
      now: time(10),
    });
    // A complete unrelated wire request supplies no attribution to the preceding attachment.
    assert.equal(checks(report).filter((c) => c.bucket === "skill").length, 0);
  } finally {
    await w.dispose();
  }
});

test("observed replacement catalogs price expansion and backfill rather than summing removed entries", async () => {
  const w = await workspace();
  try {
    const before = "- one: small\n- retained: short";
    const after =
      "- retained: expanded description ".repeat(1) +
      "x".repeat(300) +
      "\n- backfill: added entry";
    await records(join(w.claude, "projects/project/a.jsonl"), [
      attachment(0, before),
      wire(1, before),
      assistant(1),
      attachment(2, after),
      wire(3, after),
      assistant(3),
    ]);
    const report = analyze({
      scan: await scan(w.options),
      catalog: await prices(),
      now: time(10),
    });
    const row = checks(report).find((r) => r.target === identity("skill:one"))!;
    assert.ok(row.listing.gross.totalUsd! > 0);
    assert.ok(row.marginal[0]!.afterTokens! > row.marginal[0]!.beforeTokens!);
    assert.ok(row.marginal[0]!.net.totalUsd! < 0);
    assert.ok(row.marginal[0]!.rangeUsd);
    assert.equal(row.savingsUsd, null);
    assert.equal(row.action, "advisory");
  } finally {
    await w.dispose();
  }
});

test("Codex copied requests, typed requests and successful file loads deduplicate without pricing candidates", async () => {
  const w = await workspace();
  try {
    const path = join(w.home, "source/SKILL.md");
    const rows = [
      meta(w.home),
      turn(w.home),
      world(0, `- one: synthetic (file: ${path})`),
      {
        type: "response_item",
        timestamp: at(1),
        payload: {
          id: "typed-input",
          type: "message",
          role: "user",
          content: [{ type: "skill", name: "one" }],
        },
      },
      {
        type: "event_msg",
        timestamp: at(2),
        payload: {
          type: "item_completed",
          item: {
            type: "CommandExecution",
            id: "read",
            exit_code: 0,
            parsed_cmd: [{ type: "read", path }],
            aggregated_output: "SYNTHETIC_PRIVATE_BODY",
          },
        },
      },
      billed(3),
    ];
    await records(join(w.codex, "sessions/a.jsonl"), rows);
    await records(join(w.codex, "sessions/b.jsonl"), rows);
    const report = analyze({
      scan: await scan(w.options),
      catalog: await prices(),
      now: time(10),
    });
    const row = checks(report).find(
      (r) => r.target === identity(`skill:${path}`),
    )!;
    assert.equal(row.exposure.candidateRequests, 1);
    assert.equal(row.exposure.wireRequests, 0);
    assert.equal(row.use.requested, 1);
    assert.equal(row.use.loads, 1);
    assert.equal(row.use.successfulLoads, 1);
    assert.equal(row.listing.gross.totalUsd, null);
    assert.equal(row.bodies.gross.totalUsd, null);
    assert.equal(row.action, "advisory");
  } finally {
    await w.dispose();
  }
});

test("duplicate rendered folders map to current aliases; disabled or unknown scopes remain advisory", async () => {
  const w = await workspace();
  try {
    await mkdir(join(w.home, ".git"));
    const a = await install(join(w.home, ".agents/skills"), "a", "same");
    await mkdir(join(w.codex, "skills"));
    await symlink(join(w.home, ".agents/skills/a"), join(w.codex, "skills/b"));
    const b = join(w.codex, "skills/b/SKILL.md");
    const body = `- same: synthetic description (file: ${a})\n- same: synthetic description (file: ${b})`;
    await records(join(w.codex, "sessions/a.jsonl"), [
      meta(w.home),
      turn(w.home),
      world(0, body),
      billed(1),
    ]);
    let report = analyze({
      scan: await scan(w.options),
      catalog: await prices(),
      now: time(10),
    });
    assert.equal(checks(report).filter((r) => r.action === "change").length, 2);
    await writeFile(
      join(w.codex, "config.toml"),
      `[[skills.config]]\npath = ${JSON.stringify(b)}\nenabled = false\n`,
    );
    report = analyze({
      scan: await scan(w.options),
      catalog: await prices(),
      now: time(10),
    });
    assert.equal(checks(report).filter((r) => r.action === "change").length, 0);
    await mkdir(join(w.home, ".codex"), { recursive: true });
    // The same profile file is not a project override when CWD is its parent.
    assert.ok(checks(report).every((r) => r.savingsUsd === null));
  } finally {
    await w.dispose();
  }
});

test("colliding names never allocate one typed invocation to multiple source paths", async () => {
  const w = await workspace();
  try {
    await records(join(w.codex, "sessions/a.jsonl"), [
      meta(w.home),
      turn(w.home),
      world(
        0,
        "- one: first (file: /synthetic/a/SKILL.md)\n- one: second (file: /synthetic/b/SKILL.md)",
      ),
      {
        type: "response_item",
        timestamp: at(1),
        payload: {
          type: "message",
          role: "user",
          content: [{ type: "skill", name: "one" }],
        },
      },
      billed(2),
    ]);
    const rows = checks(
      analyze({
        scan: await scan(w.options),
        catalog: await prices(),
        now: time(10),
      }),
    );
    assert.equal(rows.length, 3);
    assert.ok(
      rows
        .filter((r) => r.target !== identity("skill:one"))
        .every((r) => r.use.requested === 0 && r.action === "advisory"),
    );
    assert.equal(
      rows.find((r) => r.target === identity("skill:one"))!.use.requested,
      1,
    );
    assert.ok(
      rows
        .filter((r) => r.target !== identity("skill:one"))
        .every((r) => r.uncertainty.some((s) => s.includes("name collision"))),
    );
  } finally {
    await w.dispose();
  }
});

test("manual hidden skills retain successful body loads with unknown listing overhead", async () => {
  const w = await workspace();
  try {
    const loaded = "SYNTHETIC_PRIVATE_BODY";
    await records(join(w.claude, "projects/project/a.jsonl"), [
      wire(1, "synthetic system"),
      assistant(1, [
        {
          type: "tool_use",
          name: "Skill",
          id: "hidden",
          input: { skill: "hidden" },
        },
      ]),
      {
        type: "user",
        timestamp: at(2),
        sessionId: "s",
        message: {
          content: [
            { type: "tool_result", tool_use_id: "hidden", content: loaded },
          ],
        },
      },
      wire(3, "synthetic system", loaded),
      assistant(3),
    ]);
    const row = checks(
      analyze({
        scan: await scan(w.options),
        catalog: await prices(),
        now: time(10),
      }),
    )[0]!;
    assert.equal(row.use.calls, 1);
    assert.equal(row.use.successfulCalls, 1);
    assert.equal(row.use.sessions, 1);
    assert.equal(row.exposure.wireRequests, 0);
    assert.equal(row.listing.gross.totalUsd, null);
    assert.equal(row.bodies.wireRequests, 1);
    assert.ok(row.bodies.gross.totalUsd! > 0);
    assert.equal(row.action, "advisory");
  } finally {
    await w.dispose();
  }
});

test("current documented budget violations change without invented dollars, with project uncertainty guarded", async () => {
  const w = await workspace();
  try {
    await writeFile(
      join(w.codex, "config.toml"),
      "[skills]\nmax_context_tokens = 10001\n",
    );
    await writeFile(
      join(w.claude, "settings.json"),
      JSON.stringify({
        skillListingBudgetFraction: 2,
        skillListingMaxDescChars: 0,
      }),
    );
    let report = analyze({
      scan: await scan(w.options),
      catalog: await prices(),
      now: time(10),
    });
    assert.equal(checks(report).filter((r) => r.action === "change").length, 3);
    assert.equal(
      checks(report).find((r) =>
        r.owner.endsWith(":skillListingBudgetFraction"),
      )!.recommended,
      "0.01",
    );
    assert.ok(checks(report).every((r) => r.savingsUsd === null));
    await records(join(w.codex, "sessions/a.jsonl"), [
      meta(w.home),
      turn(w.home),
      billed(1),
    ]);
    report = analyze({
      scan: await scan(w.options),
      catalog: await prices(),
      now: time(10),
    });
    const codex = checks(report).find((r) =>
      r.owner.includes("max_context_tokens"),
    )!;
    assert.equal(codex.action, "advisory"); // Project trust/launch selection at this .codex file is not established.
  } finally {
    await w.dispose();
  }
});

test("skill visibility uses last matching selectors and managed owners without removing body instructions", async () => {
  const w = await workspace();
  try {
    const file = await install(join(w.home, "sources"), "one", "one");
    const roots = [join(w.home, "sources")];
    const codex = await skillState({
      harness: "codex",
      scopes: [
        {
          file: "user",
          config: {
            skills: {
              config: [
                { name: "one", enabled: false },
                { path: file, enabled: true },
              ],
            },
          },
        },
      ],
      roots,
      env: {},
      resolved: true,
      reason: "synthetic",
    });
    assert.equal(codex.sources[0]!.visibility, "on");
    assert.match(codex.sources[0]!.owner, /\[1\]/);
    const claude = await skillState({
      harness: "claude",
      scopes: [
        { file: "user", config: { skillOverrides: { one: "off" } } },
        { file: "managed", config: { skillOverrides: { one: "name-only" } } },
      ],
      roots,
      env: {},
      resolved: true,
      reason: "synthetic",
    });
    assert.equal(claude.sources[0]!.visibility, "name-only");
    assert.match(claude.sources[0]!.owner, /^managed:/);
    assert.ok(!JSON.stringify(claude).includes("SYNTHETIC_PRIVATE"));
  } finally {
    await w.dispose();
  }
});

test("plugin skill advice names plugin controls and never claims shared instruction or MCP savings", async () => {
  const w = await workspace();
  try {
    await writeFile(
      join(w.claude, "settings.json"),
      JSON.stringify({
        skillOverrides: { "example:one": "off" },
        enabledPlugins: { "example@market": true },
      }),
    );
    const body = "- example:one: synthetic";
    await records(join(w.claude, "projects/project/a.jsonl"), [
      attachment(0, body),
      wire(1, body),
      assistant(1),
    ]);
    const row = checks(
      analyze({
        scan: await scan(w.options),
        catalog: await prices(),
        now: time(10),
      }),
    )[0]!;
    assert.match(row.owner, /enabledPlugins.example@market/);
    assert.match(row.recommended!, /plugin/);
    assert.equal(row.current, "true");
    assert.equal(row.currentResolved, false);
    assert.equal(row.action, "advisory");
    assert.equal(row.bodies.gross.totalUsd, null);
    assert.equal(row.savingsUsd, null);
  } finally {
    await w.dispose();
  }
});

test("explicit windows, name-only entries and recorded budget pressure keep no-use rankings advisory", async () => {
  const w = await workspace();
  try {
    const body = "- one\n- two: synthetic\nExceeded skills context budget.";
    await records(join(w.codex, "sessions/a.jsonl"), [
      meta(w.home),
      turn(w.home),
      world(0, body),
      {
        type: "response_item",
        timestamp: at(1),
        payload: {
          type: "message",
          role: "user",
          content: [{ type: "skill", name: "one" }],
        },
      },
      billed(3),
    ]);
    const report = analyze({
      scan: await scan({ ...w.options, since: time(2) }),
      catalog: await prices(),
      now: time(10),
    });
    const rows = checks(report);
    assert.equal(rows.length, 2);
    assert.ok(
      rows.every(
        (r) =>
          r.use.requested === 0 &&
          r.since === time(2) &&
          r.budgetPressure &&
          r.action === "advisory" &&
          r.introduced === "unknown",
      ),
    );
    assert.ok(rows.every((r) => r.reason.includes("no observed use")));
  } finally {
    await w.dispose();
  }
});

test("Claude current source precedence and canonical dedup do not turn shadowed history into a change", async () => {
  const w = await workspace();
  try {
    const enterprise = join(w.home, "enterprise"),
      personal = join(w.home, "personal"),
      project = join(w.home, "project"),
      nested = join(w.home, "nested");
    await install(personal, "one", "one");
    await install(project, "one", "one");
    const alias = await install(project, "two", "two");
    await install(nested, "two", "two");
    await mkdir(enterprise);
    await symlink(join(project, "two"), join(personal, "alias"));
    const state = await skillState({
      harness: "claude",
      scopes: [{ file: "user", config: {} }],
      roots: [enterprise, personal, project, nested],
      env: {},
      resolved: true,
      reason: "synthetic",
    });
    const one = state.sources.filter((s) => s.name === "one");
    assert.equal(one.find((s) => s.path.startsWith(personal))!.effective, true);
    assert.equal(one.find((s) => s.path.startsWith(project))!.effective, false);
    assert.equal(state.sources.find((s) => s.path === alias)!.effective, false);
    assert.equal(
      state.sources.find((s) => s.path.startsWith(nested))!.effective,
      false,
    );
    const noPersonal = await skillState({
      harness: "claude",
      scopes: [{ file: "user", config: {} }],
      roots: [enterprise, join(w.home, "missing"), project, nested],
      env: {},
      resolved: true,
      reason: "synthetic",
    });
    assert.ok(
      noPersonal.sources
        .filter((s) => s.name === "two")
        .every((s) => s.effective === null),
    );
  } finally {
    await w.dispose();
  }
});

test("malformed selectors fail safely even without installed skills", async () => {
  const w = await workspace();
  try {
    await writeFile(
      join(w.codex, "config.toml"),
      '[[skills.config]]\nname = "one"\npath = "/synthetic/one/SKILL.md"\nenabled = false\n',
    );
    await assert.rejects(scan(w.options), /invalid skill selector/);
  } finally {
    await w.dispose();
  }
});

test("Claude project/local controls outside a Git repository retain their effective owners", async () => {
  const w = await workspace();
  try {
    const project = join(w.home, "work");
    await mkdir(join(project, ".claude"), { recursive: true });
    const file = await install(join(w.claude, "skills"), "one", "one");
    await writeFile(
      join(project, ".claude/settings.json"),
      JSON.stringify({
        skillOverrides: { one: "off" },
        skillListingBudgetFraction: 0.03,
      }),
    );
    await writeFile(
      join(project, ".claude/settings.local.json"),
      JSON.stringify({ skillOverrides: { one: "name-only" } }),
    );
    const body = `- one: synthetic description (file: ${file})`;
    await records(join(w.claude, "projects/project/a.jsonl"), [
      attachment(0, body),
      { ...wire(1, body), cwd: project },
      { ...assistant(1), cwd: project },
    ]);
    const report = analyze({
      scan: await scan(w.options),
      catalog: await prices(),
      now: time(10),
    });
    const row = checks(report)[0]!;
    assert.equal(row.current, "name-only");
    assert.match(row.owner, /settings.local.json:skillOverrides.one/);
    assert.equal(row.currentResolved, true);
    assert.equal(
      row.budgets.find((b) => b.key === "skillListingBudgetFraction")!.value,
      0.03,
    );
    assert.equal(row.action, "advisory");
  } finally {
    await w.dispose();
  }
});
