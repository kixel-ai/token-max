import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { resolve, join } from "node:path";

const output = resolve(".tmp/package");
await mkdir(output, { recursive: true });
const npm = "npm";
const result = spawnSync(
  npm,
  ["pack", "--ignore-scripts", "--json", "--pack-destination", output],
  {
    encoding: "utf8",
  },
);
assert.equal(result.status, 0, result.stderr || String(result.error));
const [pack] = JSON.parse(result.stdout);
assert.deepEqual(pack.files.map((file) => file.path).sort(), [
  "LICENSE",
  "README.md",
  "dist/THIRD_PARTY_NOTICES.txt",
  "dist/cli.js",
  "dist/scan-helper.js",
  "docs/semantics.md",
  "package.json",
]);
const tarball = join(output, "token-max.tgz");
await rename(join(output, pack.filename), tarball);
const integrity =
  "sha512-" +
  createHash("sha512")
    .update(await readFile(tarball))
    .digest("base64");
assert.equal(integrity, pack.integrity);
await writeFile(
  join(output, "artifact.json"),
  JSON.stringify(
    { name: pack.name, version: pack.version, integrity },
    null,
    2,
  ) + "\n",
);
console.log(`${tarball}\n${integrity}`);
