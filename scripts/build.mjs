import { spawnSync } from "node:child_process";
import { chmod, mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

await mkdir(".tmp", { recursive: true });
const result = spawnSync(
  "bun",
  [
    "build",
    "src/cli.ts",
    "--target=node",
    "--format=esm",
    "--outfile=dist/cli.js",
    "--metafile=.tmp/bundle.json",
    "--reject-unresolved",
  ],
  { stdio: "inherit" },
);
if (result.error) throw result.error;
if (result.status !== 0) process.exit(result.status ?? 1);
await chmod("dist/cli.js", 0o755);

// Retain the licenses of every dependency actually included in the bundle.
const metadata = JSON.parse(await readFile(".tmp/bundle.json", "utf8"));
const packages = new Set();
for (const input of Object.keys(metadata.inputs)) {
  const match = /^(.*node_modules\/(?:@[^/]+\/)?[^/]+)/.exec(
    input.replaceAll("\\", "/"),
  );
  if (match) packages.add(match[1]);
}
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
