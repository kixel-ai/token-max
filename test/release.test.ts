import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { createHash } from "node:crypto";
import { join, resolve } from "node:path";

test("release preparation fails closed on unfinished work, refs, versions and environment protection", async () => {
  await mkdir(".tmp", { recursive: true });
  const dir = await mkdtemp(resolve(".tmp/release-"));
  try {
    await mkdir(join(dir, "quest/m0"), { recursive: true });
    await writeFile(join(dir, "quest/m0/README.md"), "# m0\n");
    const script = join(dir, "check-release.mjs");
    await copyFile(resolve("scripts/check-release.mjs"), script);
    const preload = join(dir, "api.mjs");
    await writeFile(
      preload,
      `globalThis.fetch = async (url) => {
      if (!url.startsWith('https://api.github.com/repos/kixelated/token-max/environments/npm')) throw Error('unexpected API');
      const status = process.env.API_STATUS ? Number(process.env.API_STATUS) : 200;
      const data = url.endsWith('deployment-branch-policies')
        ? {branch_policies: JSON.parse(process.env.POLICIES)}
        : JSON.parse(process.env.PROTECTION);
      return new Response(JSON.stringify(data), {status});
    };`,
    );
    const good = {
      ...process.env,
      RELEASE_VERSION: "0.1.0",
      GITHUB_REF: "refs/heads/main",
      GITHUB_REF_PROTECTED: "true",
      GITHUB_REPOSITORY: "kixelated/token-max",
      PROTECTION: JSON.stringify({
        deployment_branch_policy: { custom_branch_policies: true },
      }),
      POLICIES: JSON.stringify([{ name: "main", type: "branch" }]),
      API_STATUS: "200",
    };
    const run = (changes = {}) =>
      spawnSync("node", ["--import", preload, script], {
        cwd: dir,
        env: { ...good, ...changes },
        encoding: "utf8",
      });
    const version = (value: string) =>
      writeFile(
        join(dir, "package.json"),
        JSON.stringify({ name: "@kixelated/token-max", version: value }),
      );
    await version("0.1.0");
    const result = run();
    assert.equal(result.status, 0, result.stderr);
    for (const changes of [
      { RELEASE_VERSION: "0.2.0" },
      { RELEASE_VERSION: "bad" },
      { GITHUB_REF: "refs/pull/1/merge" },
      { GITHUB_REF: "refs/tags/v0.1.0" },
      { GITHUB_REF_PROTECTED: "false" },
      { GITHUB_REPOSITORY: "other/token-max" },
      { API_STATUS: "404" },
      { PROTECTION: "{}" },
      { POLICIES: "[]" },
      {
        PROTECTION: JSON.stringify({
          deployment_branch_policy: { custom_branch_policies: false },
        }),
      },
      { POLICIES: JSON.stringify([{ name: "*", type: "branch" }]) },
      { POLICIES: JSON.stringify([{ name: "main", type: "tag" }]) },
      {
        POLICIES: JSON.stringify([
          { name: "main", type: "branch" },
          { name: "*", type: "tag" },
        ]),
      },
    ])
      assert.notEqual(run(changes).status, 0, JSON.stringify(changes));
    await writeFile(
      join(dir, "package.json"),
      JSON.stringify({
        name: "@kixelated/token-max",
        version: "0.1.0",
        private: true,
      }),
    );
    assert.match(run().stderr, /private guard/);
    await version("0.0.0");
    assert.match(
      run({ RELEASE_VERSION: "0.0.0" }).stderr,
      /development placeholder/,
    );
    await version("0.1.0");
    await writeFile(join(dir, "quest/m0/unfinished.md"), "# Unfinished\n");
    assert.match(run().stderr, /every child merges/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("publication verifies the artifact at its credential boundary without checkout code", async () => {
  const workflow = await readFile(".github/workflows/release.yml", "utf8");
  const code = /node --input-type=module <<'JS'\n([\s\S]*?)\n +JS/.exec(
    workflow,
  )?.[1];
  assert.ok(code, "exercise the actual publication heredoc");
  await mkdir(".tmp", { recursive: true });
  const dir = await mkdtemp(resolve(".tmp/publication-"));
  try {
    await mkdir(join(dir, "package"));
    const bytes = Buffer.from("synthetic packed bytes");
    const artifact = {
      name: "@kixelated/token-max",
      version: "0.1.0",
      integrity:
        "sha512-" + createHash("sha512").update(bytes).digest("base64"),
    };
    await writeFile(join(dir, "package/token-max.tgz"), bytes);
    const run = async (changes = {}, version = "0.1.0") => {
      await writeFile(
        join(dir, "package/artifact.json"),
        JSON.stringify({ ...artifact, ...changes }),
      );
      return spawnSync("node", ["--input-type=module"], {
        cwd: dir,
        env: { ...process.env, RELEASE_VERSION: version },
        input: code,
        encoding: "utf8",
      });
    };
    const valid = await run();
    assert.equal(valid.status, 0, valid.stderr);
    for (const changes of [
      { name: "@other/token-max" },
      { version: "0.2.0" },
      { integrity: "sha512-tampered" },
    ])
      assert.notEqual((await run(changes)).status, 0, JSON.stringify(changes));
    assert.notEqual((await run({ version: "0.0.0" }, "0.0.0")).status, 0);
    await writeFile(
      join(dir, "package/token-max.tgz"),
      "changed after validation",
    );
    assert.notEqual((await run()).status, 0);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
