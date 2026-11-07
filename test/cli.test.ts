import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { resolve, join } from "node:path";
import { readFile, writeFile } from "node:fs/promises";
import { fixture, workspace } from "./helpers.js";

test("built CLI supports JSON, exit 0/1/2, and never prints partial findings or transcript content", async () => {
  const w = await workspace();
  try {
    const file = join(w.claude, "projects/project/main.jsonl");
    await fixture(file, "claude.jsonl");
    await writeFile(
      join(w.claude, "settings.json"),
      JSON.stringify({ promptCacheTtl: "5m" }),
    );
    const before = await readFile(file);
    const env = {
      ...process.env,
      HOME: w.home,
      CLAUDE_CONFIG_DIR: w.claude,
      CODEX_HOME: w.codex,
      FORCE_PROMPT_CACHING_5M: "",
      ENABLE_PROMPT_CACHING_1H: "",
      CLAUDE_CODE_PROMPT_CACHE_TTL: "",
      CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL: "",
    };
    const args = [
      resolve("dist/src/cli.js"),
      "--claude-dir",
      w.claude,
      "--prices-file",
      resolve("test/fixtures/prices.json"),
      "--json",
    ];
    const run = () =>
      spawnSync(process.execPath, args, { env, encoding: "utf8" });
    const result = run();
    assert.equal(result.status, 1, result.stderr);
    assert.equal(result.stderr, "");
    const report = JSON.parse(result.stdout);
    assert.equal(report.schemaVersion, 1);
    assert.equal(report.scan.requests.length, 2);
    assert.doesNotMatch(result.stdout, /SYNTHETIC_PRIVATE/);
    assert.deepEqual(await readFile(file), before);
    await writeFile(
      join(w.claude, "settings.json"),
      JSON.stringify({ promptCacheTtl: "1h" }),
    );
    assert.equal(run().status, 0);
    await writeFile(
      file,
      '{"type":"assistant","private":"SYNTHETIC_PRIVATE_REPLY",BROKEN}\n',
    );
    const failed = run();
    assert.equal(failed.status, 2);
    assert.equal(failed.stdout, "");
    assert.doesNotMatch(failed.stderr, /SYNTHETIC_PRIVATE|recommended/);
    assert.equal(JSON.parse(failed.stderr).location.line, 1);
  } finally {
    await w.dispose();
  }
});
