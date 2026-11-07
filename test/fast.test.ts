import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { writeFile } from "node:fs/promises";
import { speedControls } from "../src/controls.js";
import { speedChecks } from "../src/fast.js";
import { scan, usage } from "../src/reader.js";
import { analyze } from "../src/replay.js";
import { formatReport } from "../src/report.js";
import { pricePart } from "../src/prices.js";
import {
  claudeUsage,
  fixture,
  makeRequest,
  prices,
  workspace,
} from "./helpers.js";
import type { Scan } from "../src/model.js";

function control(
  harness: "claude" | "codex",
  config: Record<string, unknown>,
  extra: Record<string, unknown> = {},
  env: Record<string, unknown> = {},
) {
  return speedControls({
    harness,
    scopes: [
      { config, file: "user" },
      { config: extra, file: "managed" },
    ],
    env,
  });
}

test("Claude disabling controls and unsupported models suppress saved fast preferences, with exact owners", () => {
  const c = { model: "claude-opus-5-5", fastMode: true };
  assert.equal(control("claude", c).value, "fast");
  for (const [extra, env, owner] of [
    [{ fastMode: false }, {}, "managed:fastMode"],
    [{ fastModePerSessionOptIn: true }, {}, "managed:fastModePerSessionOptIn"],
    [
      {},
      { CLAUDE_CODE_DISABLE_FAST_MODE: "1" },
      "CLAUDE_CODE_DISABLE_FAST_MODE",
    ],
    [
      { env: { CLAUDE_CODE_DISABLE_FAST_MODE: "1" } },
      {},
      "managed:env.CLAUDE_CODE_DISABLE_FAST_MODE",
    ],
    [{ model: "sonnet" }, {}, "managed:model"],
  ] as const) {
    const result = control("claude", c, extra, env);
    assert.equal(result.value, "standard");
    assert.equal(result.owner, owner);
  }
  assert.equal(
    control("claude", { ...c, fastModePerSessionOptIn: true }).value,
    "standard",
  );
  assert.equal(
    control("claude", c, {}, { ANTHROPIC_MODEL: "haiku" }).value,
    "standard",
  );
  assert.equal(control("claude", {}).value, "standard");
  assert.equal(control("claude", { fastMode: true }).value, null);
  assert.throws(
    () => control("claude", { fastMode: "true" }),
    /invalid fastMode/,
  );
});

test("Codex gates are independent, support ultrafast and keep unsupported tiers unknown", () => {
  const c = { model: "gpt-6.1-sol", service_tier: "fast" };
  assert.equal(control("codex", c).value, "fast");
  assert.equal(
    control("codex", { ...c, features: { fast_mode: false } }).value,
    "standard",
  );
  assert.equal(
    control("codex", {
      ...c,
      service_tier: "ultrafast",
      features: { fast_mode: false },
    }).value,
    "ultrafast",
  );
  assert.equal(
    control("codex", {
      ...c,
      service_tier: "ultrafast",
      features: { ultrafast_mode: false },
    }).owner,
    "user:features.ultrafast_mode",
  );
  assert.equal(
    control("codex", { ...c, features: { ultrafast_mode: false } }).value,
    "fast",
  );
  assert.equal(control("codex", { ...c, model: "custom-model" }).value, null);
  assert.equal(
    control("codex", { ...c, service_tier: "ultrafast", model: "gpt-6-luna" })
      .value,
    null,
  );
  assert.equal(control("codex", { service_tier: "auto" }).value, null);
  assert.equal(control("codex", {}).value, "standard");
  assert.throws(
    () => control("codex", { ...c, features: { fast_mode: "false" } }),
    /invalid/,
  );
});

test("rate-only premium preserves Claude 5m/1h reads/writes, geography and fixed fees", async () => {
  const w = await workspace();
  try {
    await writeFile(
      join(w.claude, "settings.json"),
      JSON.stringify({ fastMode: true, model: "opus" }),
    );
    const s = await scan(w.options);
    const r = makeRequest({
      profiles: [s.profiles.find((p) => p.harness === "claude")!.id],
      parts: [
        {
          ...makeRequest().parts[0]!,
          speed: "fast",
          geo: "us",
          usage: claudeUsage({ input: 100, write: 200, read: 300, output: 50 }),
        },
        {
          ...makeRequest().parts[0]!,
          speed: "standard",
          usage: claudeUsage({
            input: 100,
            write: 200,
            read: 300,
            output: 50,
            ttl: "1h",
          }),
        },
      ],
      serverTools: { web_search_requests: 999 },
    });
    s.requests = [r];
    const report = analyze({
      scan: s,
      catalog: await prices(),
      now: 30 * 86400000,
    });
    const result = report.checks.find((c) => c.check === "fast-mode")!;
    assert.equal(result.action, "change");
    assert.equal(result.evidence, "guidance");
    const observed = ((100 * 4 + 200 * 5 + 300 * 0.2 + 50 * 20) * 1.1) / 1e6;
    assert.ok(Math.abs(result.observed.premium.totalUsd! - observed) < 1e-12);
    const estimate = observed + (100 * 4 + 200 * 8 + 300 * 0.2 + 50 * 20) / 1e6;
    assert.ok(Math.abs(result.savingsUsd! - estimate) < 1e-12);
    assert.equal(
      result.observed.monthlyPremiumUsd,
      result.observed.premium.totalUsd,
    );
    assert.equal(result.observed.premiumParts, 1);
    assert.equal(result.observed.standardParts, 1);
    assert.equal(result.currentEstimate.pricedParts, 2);
    const json = JSON.stringify(report);
    for (const details of [false, true]) {
      const text = formatReport(report, { details });
      assert.match(text, /Observed response-tier premium/);
      assert.match(text, /Current-setting premium estimate/);
      assert.match(text, /cache-switch effects excluded/);
      assert.match(text, /rate-only/);
      assert.match(text, /fastMode: false/);
    }
    assert.equal(JSON.stringify(report), json);
    s.profiles.find((p) => p.harness === "claude")!.speed = control("claude", {
      fastMode: false,
    });
    const standard = speedChecks({
      scan: s,
      catalog: await prices(),
      since: undefined,
      now: 86400000,
    }).find((c) => c.profile === r.profiles[0])!;
    assert.equal(
      standard.action,
      "keep",
      "old billed premiums must not recommend changing current standard speed",
    );
    assert.equal(standard.savingsUsd, null);
    assert.ok(standard.observed.premium.totalUsd! > 0);
  } finally {
    await w.dispose();
  }
});

test("OpenAI fast and ultrafast API repricing preserves long context and billed write lifetimes", async () => {
  const catalog = await prices();
  const p = {
    ...makeRequest().parts[0]!,
    model: "gpt-6.1-sol",
    speed: null,
    tier: "default",
    usage: usage(
      {
        input_tokens: 300000,
        output_tokens: 100,
        input_tokens_details: {
          cached_tokens: 200000,
          cache_write_tokens: 50000,
          cache_write_tokens_details: {
            ttl_30m_tokens: 20000,
            ttl_12h_tokens: 30000,
          },
        },
      },
      "codex",
    ),
  };
  const r = makeRequest({
    harness: "codex",
    provider: "openai",
    api: "responses",
    parts: [p],
  });
  const normal = pricePart(catalog, r, p).totalUsd!;
  for (const tier of ["fast", "priority", "ultrafast"]) {
    assert.equal(
      pricePart(catalog, r, { ...p, tier }).totalUsd,
      normal * (tier === "ultrafast" ? 6 : 2),
    );
  }
  assert.equal(
    pricePart(catalog, r, { ...p, tier: "ultrafast", model: "gpt-6-luna" })
      .totalUsd,
    null,
  );
  assert.equal(
    pricePart(catalog, r, {
      ...p,
      tier: "fast",
      usage: { ...p.usage, write: null },
    }).totalUsd,
    null,
  );
});

test("current-setting warning survives missing billing metadata; historical unknowns and prices stay unknown", async () => {
  const w = await workspace();
  try {
    await writeFile(
      join(w.codex, "config.toml"),
      'model = "gpt-6.1-sol"\nservice_tier = "ultrafast"\n',
    );
    await fixture(join(w.codex, "sessions/main.jsonl"), "codex.jsonl");
    const s = await scan({ ...w.options, codexDirs: [w.codex] });
    const c = await prices();
    const check = () =>
      speedChecks({
        scan: s,
        catalog: c,
        since: undefined,
        now: Date.parse("2026-11-02"),
      })[0]!;
    assert.equal(
      s.requests[0]!.parts[0]!.tier,
      null,
      "native Codex usage records do not contain billed tiers",
    );
    assert.equal(s.requests[0]!.requestedSpeed!.tier, "standard");
    assert.equal(check().action, "change");
    assert.equal(check().observed.premium.totalUsd, null);
    assert.ok(
      check().savingsUsd! > 0,
      "current config estimate may use measured tokens without inventing an observed premium",
    );
    c.models.clear();
    assert.equal(check().action, "change");
    assert.equal(check().savingsUsd, null);
    assert.equal(check().currentEstimate.premium.totalUsd, null);
    s.requests = [];
    assert.equal(check().action, "change");
    assert.equal(check().savingsUsd, null);
  } finally {
    await w.dispose();
  }
});

test("active-profile environment, managed policy and current project ownership suppress stale preferences", async () => {
  const w = await workspace();
  try {
    await writeFile(
      join(w.claude, "settings.json"),
      '{"model":"opus","fastMode":true}',
    );
    const file = join(w.claude, "projects/project/main.jsonl");
    await fixture(file, "claude.jsonl");
    const envScan = await scan({
      ...w.options,
      env: { CLAUDE_CONFIG_DIR: w.claude, CLAUDE_CODE_DISABLE_FAST_MODE: "1" },
    });
    assert.equal(
      envScan.profiles.find((p) => p.harness === "claude")!.speed!.value,
      "standard",
    );
    await writeFile(w.options.managedSettingsFile, '{"fastMode":false}');
    const managed = await scan(w.options);
    assert.equal(
      managed.profiles.find((p) => p.harness === "claude")!.speed!.owner,
      w.options.managedSettingsFile + ":fastMode",
    );
    await writeFile(w.options.managedSettingsFile, "{}");
    const { mkdir, readFile } = await import("node:fs/promises");
    await mkdir(join(w.home, ".git"));
    await mkdir(join(w.home, ".claude-project", ".claude"), {
      recursive: true,
    });
    const cwd = join(w.home, ".claude-project");
    // Project settings are at the repository root, not an arbitrary descendant.
    await writeFile(
      join(w.claude, "settings.local.json"),
      '{"fastMode":false}',
    );
    const rows = (await readFile(file, "utf8"))
      .trimEnd()
      .split("\n")
      .map((l) => ({ ...JSON.parse(l), cwd }));
    await writeFile(file, rows.map((r) => JSON.stringify(r)).join("\n") + "\n");
    const project = await scan(w.options);
    const result = speedChecks({
      scan: project,
      catalog: await prices(),
      since: undefined,
      now: Date.parse("2026-11-02"),
    }).find(
      (c) =>
        c.profile === project.profiles.find((p) => p.harness === "claude")!.id,
    )!;
    assert.equal(result.action, "keep");
    assert.equal(
      result.owner,
      join(w.claude, "settings.local.json") + ":fastMode",
    );
  } finally {
    await w.dispose();
  }
});

test("Codex legacy/named profiles and project selection stay advisory; local requirements can block premium", async () => {
  const w = await workspace();
  try {
    const config = join(w.codex, "config.toml");
    await writeFile(
      config,
      'model="gpt-6.1-sol"\nservice_tier="fast"\n[profiles.special]\nservice_tier="default"',
    );
    const options = {
      ...w.options,
      codexDirs: [w.codex],
      codexRequirementsFile: join(w.home, "requirements.toml"),
    };
    assert.equal((await scan(options)).profiles[0]!.speed!.resolved, false);
    await writeFile(config, 'model="gpt-6.1-sol"\nservice_tier="fast"');
    await writeFile(
      join(w.codex, "special.config.toml"),
      'service_tier="default"',
    );
    assert.match(
      (await scan(options)).profiles[0]!.speed!.reason,
      /--profile selection/,
    );
    const { unlink } = await import("node:fs/promises");
    await unlink(join(w.codex, "special.config.toml"));
    await writeFile(
      options.codexRequirementsFile,
      "[features]\nfast_mode=false",
    );
    assert.equal((await scan(options)).profiles[0]!.speed!.value, "standard");
    await writeFile(config, 'model="gpt-6.1-sol"\nservice_tier="ultrafast"');
    await writeFile(
      options.codexRequirementsFile,
      "[features]\nultrafast_mode=false",
    );
    assert.equal((await scan(options)).profiles[0]!.speed!.value, "standard");
  } finally {
    await w.dispose();
  }
});

test("unknown speeds, price gaps and explicit windows never turn missing premiums into zero", async () => {
  const w = await workspace();
  try {
    await writeFile(
      join(w.claude, "settings.json"),
      '{"model":"opus","fastMode":true}',
    );
    const s: Scan = await scan({ ...w.options, claudeDirs: [w.claude] });
    const p = s.profiles[0]!;
    const r = makeRequest({
      profiles: [p.id],
      time: 100,
      parts: [{ ...makeRequest().parts[0]!, speed: null }],
    });
    s.requests = [r];
    const c = await prices();
    const result = speedChecks({
      scan: s,
      catalog: c,
      since: 50,
      now: 200,
    })[0]!;
    assert.equal(result.observed.unknownParts, 1);
    assert.equal(result.observed.premium.totalUsd, null);
    assert.equal(result.observed.premium.knownUsd, 0);
    assert.ok(result.currentEstimate.premium.totalUsd! > 0);
    const excluded = speedChecks({
      scan: s,
      catalog: c,
      since: 150,
      now: 200,
    })[0]!;
    assert.equal(excluded.requests, 0);
    assert.equal(excluded.savingsUsd, null);
    r.parts[0]!.speed = "fast";
    c.models.clear();
    const unknown = speedChecks({
      scan: s,
      catalog: c,
      since: undefined,
      now: 200,
    })[0]!;
    assert.equal(unknown.observed.premium.totalUsd, null);
    assert.equal(unknown.action, "change");
  } finally {
    await w.dispose();
  }
});

test("built CLI warns from current settings alone, remains read-only and fails malformed selected scans", async () => {
  const w = await workspace();
  try {
    const { spawnSync } = await import("node:child_process");
    const { resolve } = await import("node:path");
    const { readFile } = await import("node:fs/promises");
    const settingsFile = join(w.codex, "config.toml");
    const text = 'model="gpt-6.1-sol"\nservice_tier="ultrafast"';
    await writeFile(settingsFile, text);
    const args = [
      resolve("dist/src/cli.js"),
      "--codex-dir",
      w.codex,
      "--prices-file",
      resolve("test/fixtures/prices.json"),
      "--json",
    ];
    const run = () =>
      spawnSync(process.execPath, args, {
        encoding: "utf8",
        env: { ...process.env, HOME: w.home, CODEX_HOME: w.codex },
      });
    const result = run();
    assert.equal(result.status, 1, result.stderr);
    const report = JSON.parse(result.stdout);
    assert.equal(
      report.checks.find((c: { check: string }) => c.check === "fast-mode")
        .savingsUsd,
      null,
    );
    assert.equal(await readFile(settingsFile, "utf8"), text);
    await writeFile(settingsFile, text + "\n[features]\nultrafast_mode=false");
    assert.equal(run().status, 0);
    await writeFile(
      join(w.codex, "sessions/broken.jsonl"),
      '{"private":"SYNTHETIC_PRIVATE",BROKEN}\n',
    );
    const failed = run();
    assert.equal(failed.status, 2);
    assert.equal(failed.stdout, "");
    assert.doesNotMatch(failed.stderr, /SYNTHETIC_PRIVATE/);
  } finally {
    await w.dispose();
  }
});
