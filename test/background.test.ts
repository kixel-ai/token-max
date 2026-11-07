import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { backgroundControls, backgroundChecks } from "../src/background.js";
import { scan } from "../src/reader.js";
import { analyze } from "../src/replay.js";
import { priceRequest } from "../src/prices.js";
import { formatReport } from "../src/report.js";
import { fixture, makeRequest, prices, workspace } from "./helpers.js";

const controls = (
  config: Record<string, unknown> = {},
  env: Record<string, unknown> = {},
) =>
  backgroundControls({
    harness: "claude",
    scopes: [{ config, file: "user.json" }],
    env,
  });

test("background controls preserve per-feature owners and environment precedence without retaining arbitrary environment text", () => {
  const c = backgroundControls({
    harness: "claude",
    scopes: [
      {
        file: "user.json",
        config: {
          promptSuggestionEnabled: false,
          awaySummaryEnabled: true,
          env: { CLAUDE_CODE_GOAL_CHECKIN_MINUTES: "60" },
        },
      },
      {
        file: "local.json",
        config: { promptSuggestionEnabled: true, awaySummaryEnabled: false },
      },
      {
        file: "managed.json",
        config: {
          promptSuggestionEnabled: false,
          env: { CLAUDE_CODE_ENABLE_AWAY_SUMMARY: "1" },
        },
      },
    ],
    env: {
      CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION: "true",
      CLAUDE_CODE_GOAL_CHECKIN_MINUTES: "0",
      UNRELATED_SECRET: "SYNTHETIC_PRIVATE",
    },
  });
  assert.equal(c["prompt-suggestions"]!.value, true);
  assert.equal(
    c["prompt-suggestions"]!.owner,
    "CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION",
  );
  assert.equal(c["session-recap"]!.value, true);
  assert.equal(
    c["session-recap"]!.owner,
    "managed.json:env.CLAUDE_CODE_ENABLE_AWAY_SUMMARY",
  );
  assert.equal(c["goal-checkins"]!.value, 60);
  assert.equal(
    c["goal-checkins"]!.owner,
    "user.json:env.CLAUDE_CODE_GOAL_CHECKIN_MINUTES",
  );
  assert.equal(
    controls(
      { env: { CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION: "off" } },
      { CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION: "on" },
    )["prompt-suggestions"]!.value,
    false,
  );
  assert.equal(
    controls({}, { DISABLE_TELEMETRY: "0" })["prompt-suggestions"]!.value,
    false,
  );
  assert.doesNotMatch(JSON.stringify(c), /SYNTHETIC_PRIVATE|UNRELATED_SECRET/);
  assert.equal(
    controls(
      { awaySummaryEnabled: false },
      { CLAUDE_CODE_ENABLE_AWAY_SUMMARY: "0" },
    )["session-recap"]!.value,
    false,
  );
  for (const value of ["constructor", "__proto__", "unsupported"])
    assert.equal(
      controls({}, { CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION: value })[
        "prompt-suggestions"
      ]!.value,
      null,
    );
  assert.throws(
    () => controls({ promptSuggestionEnabled: "yes" }),
    /invalid promptSuggestionEnabled/,
  );
});

test("provider prerequisites and the documented goal interval fallback do not become usage evidence", () => {
  const c = controls(
    { promptSuggestionEnabled: true },
    { DISABLE_TELEMETRY: "1", CLAUDE_CODE_GOAL_CHECKIN_MINUTES: "10081" },
  );
  assert.equal(c["prompt-suggestions"]!.value, false);
  assert.equal(c["prompt-suggestions"]!.owner, "DISABLE_TELEMETRY");
  assert.equal(c["goal-checkins"]!.value, 30);
  assert.equal(
    controls(
      {},
      {
        CLAUDE_CODE_USE_BEDROCK: "1",
        CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION: "1",
      },
    )["prompt-suggestions"]!.value,
    true,
  );
  for (const value of ["-1", "1.5", "1e2", "garbage"])
    assert.equal(
      controls({}, { CLAUDE_CODE_GOAL_CHECKIN_MINUTES: value })[
        "goal-checkins"
      ]!.value,
      30,
    );
  for (const value of ["0", "10080"])
    assert.equal(
      controls({}, { CLAUDE_CODE_GOAL_CHECKIN_MINUTES: value })[
        "goal-checkins"
      ]!.value,
      Number(value),
    );
});

async function synthetic() {
  const w = await workspace();
  await fixture(join(w.claude, "projects/project/main.jsonl"), "claude.jsonl");
  const s = await scan({ ...w.options, claudeDirs: [w.claude] });
  s.exposure = {
    components: [],
    requests: [],
    invocations: [],
    visibility: [],
  };
  s.events = [];
  s.cacheContext = { requests: [], boundaries: [] };
  s.profiles[0]!.background = controls();
  s.requests = [
    makeRequest({
      id: "first",
      profiles: [s.profiles[0]!.id],
      client: "cli",
      source: "repl_main_thread",
      version: "2.1.319",
    }),
  ];
  return { w, s, catalog: await prices() };
}

test("long gaps, short replies and enabled current controls never invent background usage or disable savings", async () => {
  const { w, s, catalog } = await synthetic();
  try {
    s.requests.push({ ...s.requests[0]!, id: "later", time: 86400000 });
    const checks = backgroundChecks({ scan: s, catalog, now: 86400001 });
    assert.equal(checks.length, 3);
    for (const c of checks) {
      assert.equal(c.action, "advisory");
      assert.equal(c.recommended, null);
      assert.equal(c.savingsUsd, null);
      assert.equal(c.monthlySavingsUsd, null);
      assert.equal(c.attributedCost.totalUsd, null);
      assert.equal(c.totalFeatureCost.totalUsd, null);
      assert.deepEqual(c.attributedRequests, []);
      assert.equal(c.applicability, "eligible-client");
    }
    assert.match(
      checks.find((c) => c.bucket === "goal-checkins")!.guidance,
      /autonomous follow-on work/,
    );
    s.requests[0]!.client = "sdk-ts";
    s.requests.pop();
    const sdk = backgroundChecks({ scan: s, catalog, now: 1 });
    assert.equal(sdk[0]!.applicability, "ineligible-client");
    assert.doesNotMatch(sdk[0]!.guidance, /consider disabling/);
    s.requests[0]!.client = null;
    assert.equal(
      backgroundChecks({ scan: s, catalog, now: 1 })[0]!.applicability,
      "unknown",
    );
  } finally {
    await w.dispose();
  }
});

test("only a verified explicit suggestion source prices its deduplicated subset, never a complete feature or net saving", async () => {
  const { w, s, catalog } = await synthetic();
  try {
    const r = s.requests[0]!;
    r.source = "prompt_suggestion";
    const report = analyze({ scan: s, catalog, now: 100 });
    const c = report.checks.find(
      (c) =>
        c.check === "background-generation" &&
        c.bucket === "prompt-suggestions",
    )!;
    assert.equal(c.check, "background-generation");
    if (c.check !== "background-generation") return;
    assert.deepEqual(c.attributedRequests, [r.id]);
    assert.deepEqual(c.attributedCost, priceRequest(catalog, r));
    assert.equal(c.totalFeatureCost.totalUsd, null);
    assert.equal(c.totalFeatureCost.knownUsd, c.attributedCost.knownUsd);
    assert.equal(c.savingsUsd, null);
    assert.equal(c.action, "advisory");
    assert.equal(
      report.historical[0]!.cost.knownUsd,
      c.attributedCost.knownUsd,
    );
    r.parts[0]!.usage.output = null;
    assert.equal(
      backgroundChecks({ scan: s, catalog, now: 100 })[0]!.attributedCost
        .totalUsd,
      null,
    );
    for (const source of [
      "away_summary",
      "compact",
      "guardian",
      "goal_checkin",
      "agent:worker",
      "recap",
      null,
    ]) {
      r.source = source;
      assert.ok(
        backgroundChecks({ scan: s, catalog, now: 100 }).every(
          (c) => c.attributedRequests.length === 0,
        ),
      );
    }
  } finally {
    await w.dispose();
  }
});

test("background windows inherit billing selection and every existing replay exposes the incomplete baseline", async () => {
  const { w, s, catalog } = await synthetic();
  try {
    s.since = 50;
    s.requests[0]!.time = 60;
    const report = analyze({ scan: s, catalog, now: 100 });
    for (const c of report.checks) {
      if (c.check === "background-generation") {
        assert.equal(c.since, 50);
        assert.match(c.windowBasis, /explicit/);
      } else
        assert.match(
          c.baselineCoverage!.join(" "),
          /cache refreshes.*recorded-request baseline/,
        );
    }
    for (const details of [false, true]) {
      const text = formatReport(report, { details });
      assert.match(text, /background-generation \/ prompt-suggestions/);
      assert.match(text, /Current: 30 minutes/);
      assert.match(text, /Total feature cost: unknown/);
      assert.match(text, /Baseline coverage: Native background/);
      assert.doesNotMatch(text, /SYNTHETIC_PRIVATE/);
    }
  } finally {
    await w.dispose();
  }
});

test("discovery resolves current Claude project/local/managed controls and keeps shell overrides on the active profile", async () => {
  const w = await workspace();
  try {
    const project = join(w.home, "project");
    await mkdir(join(project, ".git"), { recursive: true });
    await mkdir(join(project, ".claude"));
    await writeFile(
      join(project, ".claude/settings.local.json"),
      JSON.stringify({ awaySummaryEnabled: false }),
    );
    const file = join(w.claude, "projects/project/main.jsonl");
    await fixture(file, "claude.jsonl");
    const rows = (await readFile(file, "utf8"))
      .trim()
      .split("\n")
      .map((line) => ({ ...JSON.parse(line), cwd: project }));
    await writeFile(file, rows.map((r) => JSON.stringify(r)).join("\n") + "\n");
    await writeFile(
      w.options.managedSettingsFile,
      JSON.stringify({ promptSuggestionEnabled: false }),
    );
    const env = {
      CLAUDE_CONFIG_DIR: w.claude,
      CLAUDE_CODE_GOAL_CHECKIN_MINUTES: "0",
    };
    const s = await scan({ ...w.options, env, claudeDirs: [w.claude] });
    const c = s.profiles[0]!.backgroundDirectories![project]!;
    assert.equal(c["session-recap"]!.value, false);
    assert.equal(
      c["session-recap"]!.owner,
      `${project}/.claude/settings.local.json:awaySummaryEnabled`,
    );
    assert.equal(
      c["prompt-suggestions"]!.owner,
      `${w.options.managedSettingsFile}:promptSuggestionEnabled`,
    );
    assert.equal(c["goal-checkins"]!.value, 0);
    const inactive = await scan({
      ...w.options,
      env: { ...env, CLAUDE_CONFIG_DIR: w.codex },
      claudeDirs: [w.claude],
    });
    assert.equal(
      inactive.profiles[0]!.backgroundDirectories![project]!["goal-checkins"]!
        .value,
      30,
    );
    assert.deepEqual(
      await readFile(file, "utf8"),
      rows.map((r) => JSON.stringify(r)).join("\n") + "\n",
    );
  } finally {
    await w.dispose();
  }
});

test("Codex current recap state stays unresolved with named profiles or project layers and absent metadata", async () => {
  const w = await workspace();
  try {
    await fixture(join(w.codex, "sessions/main.jsonl"), "codex.jsonl");
    await writeFile(
      join(w.codex, "config.toml"),
      "[tui]\nauto_recap = false\n[profiles.custom]\nmodel = 'gpt-6'\n",
    );
    const s = await scan({ ...w.options, codexDirs: [w.codex] });
    const result = backgroundChecks({
      scan: s,
      catalog: await prices(),
      now: 100,
    });
    assert.equal(result.length, 1);
    assert.equal(result[0]!.bucket, "auto-recap");
    assert.equal(result[0]!.current, null);
    assert.equal(result[0]!.savingsUsd, null);
    assert.equal(result[0]!.action, "advisory");
    const defaults = backgroundControls({
      harness: "codex",
      scopes: [{ file: "config.toml", config: { tui: { auto_recap: false } } }],
      env: {},
    });
    assert.equal(defaults["auto-recap"]!.value, false);
    assert.throws(
      () =>
        backgroundControls({
          harness: "codex",
          scopes: [
            { file: "config.toml", config: { tui: { auto_recap: "false" } } },
          ],
          env: {},
        }),
      /invalid tui.auto_recap/,
    );
  } finally {
    await w.dispose();
  }
});

test("built CLI keeps background-only guidance advisory and fails atomically for invalid selected controls", async () => {
  const w = await workspace();
  try {
    const settings = join(w.codex, "config.toml");
    await writeFile(settings, "[tui]\nauto_recap = true\n");
    const before = await readFile(settings);
    const args = [
      resolve("dist/src/cli.js"),
      "--codex-dir",
      w.codex,
      "--prices-file",
      resolve("test/fixtures/prices.json"),
      "--json",
    ];
    const run = () => spawnSync(process.execPath, args, { encoding: "utf8" });
    const ok = run();
    assert.equal(ok.status, 0, ok.stderr);
    const c = JSON.parse(ok.stdout).checks.find(
      (c: { check: string }) => c.check === "background-generation",
    );
    assert.equal(c.check, "background-generation");
    assert.equal(c.savingsUsd, null);
    assert.equal(c.action, "advisory");
    assert.deepEqual(await readFile(settings), before);
    await writeFile(settings, "[tui]\nauto_recap = 'SYNTHETIC_PRIVATE'\n");
    const invalid = run();
    assert.equal(invalid.status, 2);
    assert.equal(invalid.stdout, "");
    assert.doesNotMatch(invalid.stderr, /SYNTHETIC_PRIVATE/);
  } finally {
    await w.dispose();
  }
});
