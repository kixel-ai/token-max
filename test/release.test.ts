import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { copyFile, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
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
        protection_rules: [{ type: "required_reviewers", reviewers: [{}] }],
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
    assert.equal(run().status, 0, run().stderr);
    for (const changes of [
      { RELEASE_VERSION: "0.2.0" },
      { RELEASE_VERSION: "bad" },
      { GITHUB_REF: "refs/pull/1/merge" },
      { GITHUB_REF: "refs/tags/v0.1.0" },
      { GITHUB_REF_PROTECTED: "false" },
      { GITHUB_REPOSITORY: "other/token-max" },
      { API_STATUS: "404" },
      {
        PROTECTION: JSON.stringify({
          protection_rules: [],
          deployment_branch_policy: { custom_branch_policies: true },
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
