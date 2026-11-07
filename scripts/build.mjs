import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmod, mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

await mkdir(".tmp", { recursive: true });
const hash = createHash("sha256");
for (const file of [
  "package.json",
  "bun.lock",
  "flake.lock",
  "scripts/build.mjs",
  ...(await readdir("src")).sort().map((name) => join("src", name)),
]) {
  hash.update(file + "\0");
  hash.update(await readFile(file));
}
const build = hash.digest("hex");
const packages = new Set();
async function bundle(entry, output, defines) {
  const result = spawnSync(
    "bun",
    [
      "build",
      entry,
      "--target=node",
      "--format=esm",
      `--outfile=${output}`,
      "--metafile=.tmp/bundle.json",
      "--reject-unresolved",
      ...Object.entries(defines).map(
        ([key, value]) => `--define=${key}=${JSON.stringify(value)}`,
      ),
    ],
    { stdio: "inherit" },
  );
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
  const metadata = JSON.parse(await readFile(".tmp/bundle.json", "utf8"));
  for (const input of Object.keys(metadata.inputs)) {
    const match = /^(.*node_modules\/(?:@[^/]+\/)?[^/]+)/.exec(
      input.replaceAll("\\", "/"),
    );
    if (match) packages.add(match[1]);
  }
}
await bundle("src/scan-helper.ts", "dist/scan-helper.js", { BUILD_ID: build });
const helperDigest = createHash("sha256")
  .update(await readFile("dist/scan-helper.js"))
  .digest("hex");
await bundle("src/cli.ts", "dist/cli.js", {
  BUILD_ID: build,
  HELPER_DIGEST: helperDigest,
});
await chmod("dist/cli.js", 0o755);

// Retain licenses from the union of both bundles' actual dependencies.
const notices = [];
for (const dir of [...packages].sort()) {
  const pkg = JSON.parse(await readFile(join(dir, "package.json"), "utf8"));
  const files = (await readdir(dir))
    .filter((name) => /^(?:licen[sc]e|copying)(?:[.-].*)?$/i.test(name))
    .sort();
  if (!files.length)
    throw new Error(`Missing license for bundled package ${pkg.name}`);
  const licenses = await Promise.all(
    files.map((file) => readFile(join(dir, file), "utf8")),
  );
  notices.push(`${pkg.name}@${pkg.version}\n\n${licenses.join("\n\n")}`);
}
await writeFile("dist/THIRD_PARTY_NOTICES.txt", notices.join("\n\n----\n\n"));
