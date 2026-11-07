import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmod,
  copyFile,
  link,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { existsSync } from "node:fs";
import { createServer } from "node:http";
import { delimiter, dirname, join, resolve } from "node:path";

const root = resolve(".");
const tarball = resolve(process.argv[2] ?? ".tmp/package/token-max.tgz");
const bytes = await readFile(tarball);
const artifact = JSON.parse(
  await readFile(join(dirname(tarball), "artifact.json"), "utf8"),
);
assert.equal(
  "sha512-" + createHash("sha512").update(bytes).digest("base64"),
  artifact.integrity,
);
const pkg = JSON.parse(await readFile("package.json", "utf8"));
assert.equal(artifact.name, pkg.name);
assert.equal(artifact.version, pkg.version);
assert.equal(
  pkg.dependencies,
  undefined,
  "the bundle must have no consumer dependencies",
);
for (const name of ["preinstall", "install", "postinstall", "prepare"])
  assert.equal(pkg.scripts[name], undefined);

function executable(name) {
  const file = process.platform === "win32" ? `${name}.exe` : name;
  const value = process.env.PATH ?? process.env.Path;
  const found = value
    .split(delimiter)
    .map((dir) => join(dir, file))
    .find(existsSync);
  assert.ok(found, `${file} must be on PATH`);
  return found;
}
// On Windows npm is a cmd shim; execute its JavaScript with this test's Node.
const npm =
  process.platform === "win32"
    ? join(dirname(process.execPath), "node_modules/npm/bin/npm-cli.js")
    : await realpath(
        (process.env.PATH ?? "")
          .split(delimiter)
          .map((dir) => join(dir, "npm"))
          .find(existsSync),
      );
const npx = join(dirname(npm), "npx-cli.js");
const bun = executable("bun");
await mkdir(".tmp", { recursive: true });
const scratch = await mkdtemp(resolve(".tmp/packed-"));
const consumer = join(scratch, "consumer with spaces");
const home = join(scratch, "home");
await mkdir(consumer);
await writeFile(
  join(consumer, "package.json"),
  JSON.stringify({ name: "packed-consumer", private: true }),
);
await mkdir(home);
await mkdir(join(scratch, "temp"));
const env = { ...process.env };
for (const key of Object.keys(env)) {
  if (
    /^(npm_config_|npm_token$|node_auth_token$|node_options$|bun_options$|claude_|codex_|force_prompt_|enable_prompt_|disable_auto_compact$)/i.test(
      key,
    )
  )
    delete env[key];
}
Object.assign(env, {
  BUN_TMPDIR: join(scratch, "temp"),
  TMPDIR: join(scratch, "temp"),
  TMP: join(scratch, "temp"),
  TEMP: join(scratch, "temp"),
  HOME: home,
  USERPROFILE: home,
  XDG_CACHE_HOME: join(home, ".cache"),
  CLAUDE_CONFIG_DIR: "",
  CODEX_HOME: "",
  NO_COLOR: "1",
  npm_config_cache: join(scratch, "npm-cache"),
  npm_config_userconfig: join(home, ".npmrc"),
  npm_config_ignore_scripts: "true",
  npm_config_audit: "false",
  npm_config_fund: "false",
  BUN_INSTALL: join(home, ".bun"),
  BUN_INSTALL_CACHE_DIR: join(scratch, "bun-cache"),
});
const run = (command, args, vars = env) =>
  new Promise((resolve, reject) => {
    const child = spawn(command, args, {
      cwd: consumer,
      env: vars,
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "",
      stderr = "";
    child.stdout.setEncoding("utf8").on("data", (data) => {
      stdout += data;
    });
    child.stderr.setEncoding("utf8").on("data", (data) => {
      stderr += data;
    });
    child.once("error", reject);
    child.once("close", (status, signal) =>
      resolve({ status, signal, stdout, stderr }),
    );
  });
let downloads = 0;
// Exercise the real scoped launch syntax without publishing an incomplete release.
// The fixture registry serves only this exact tarball and synthetic metadata.
const server = createServer((request, response) => {
  const path = decodeURIComponent(request.url);
  if (path === "/token-max.tgz") {
    downloads++;
    response.setHeader("Content-Type", "application/octet-stream");
    response.end(bytes);
  } else if (path === "/@kixelated/token-max") {
    response.setHeader("Content-Type", "application/json");
    response.end(
      JSON.stringify({
        name: pkg.name,
        "dist-tags": { latest: pkg.version },
        versions: {
          [pkg.version]: {
            ...pkg,
            dist: {
              tarball: `http://127.0.0.1:${server.address().port}/token-max.tgz`,
              integrity: artifact.integrity,
            },
          },
        },
      }),
    );
  } else {
    response.writeHead(404).end();
  }
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
env.npm_config_registry = `http://127.0.0.1:${server.address().port}`;
await writeFile(join(home, ".npmrc"), `registry=${env.npm_config_registry}\n`);

try {
  const installed = join(consumer, "installed");
  const install = await run(
    process.execPath,
    [
      npm,
      "install",
      "--prefix",
      installed,
      "--ignore-scripts",
      "--no-package-lock",
      tarball,
    ],
    { ...env, npm_config_cache: join(scratch, "direct-npm-cache") },
  );
  assert.equal(install.status, 0, install.stderr);
  const installedPackage = join(installed, "node_modules/@kixelated/token-max");
  const manifest = JSON.parse(
    await readFile(join(installedPackage, "package.json"), "utf8"),
  );
  assert.equal(manifest.version, artifact.version);
  assert.equal(manifest.dependencies, undefined);
  const cli = join(installedPackage, manifest.bin["token-max"]);
  assert.match(await readFile(cli, "utf8"), /^#!\/usr\/bin\/env node\n/);
  // A standalone copy cannot resolve any source tree or installed dependencies.
  const standalone = join(consumer, "standalone.mjs");
  await copyFile(cli, standalone);
  const direct = (args) => run(process.execPath, [standalone, ...args]);
  const launch = (args) =>
    run(process.execPath, [npx, "--yes", `${pkg.name}@latest`, ...args]);

  const only = join(scratch, "bun-only");
  await mkdir(only);
  const suffix = process.platform === "win32" ? ".exe" : "";
  const onlyBun = join(only, "bun" + suffix);
  const bunx = join(only, "bunx" + suffix);
  await copyFile(bun, onlyBun);
  await chmod(onlyBun, 0o755);
  await link(onlyBun, bunx);
  const bunEnv = { ...env };
  for (const key of Object.keys(bunEnv))
    if (key.toLowerCase() === "path") delete bunEnv[key];
  bunEnv.PATH = only;
  const probe = await run(
    onlyBun,
    [
      "-e",
      'try { Bun.spawnSync(["node", "--version"]); process.exit(1); } catch (error) { if (error.code !== "ENOENT") throw error; }',
    ],
    bunEnv,
  );
  assert.equal(probe.status, 0, probe.stderr);
  const bunLaunch = (args) =>
    run(bunx, ["--bun", `${pkg.name}@latest`, ...args], bunEnv);

  await mkdir(join(consumer, "profile/projects/project"), { recursive: true });
  const transcript = join(consumer, "profile/projects/project/main.jsonl");
  const settings = join(consumer, "profile/settings.json");
  await copyFile(join(root, "test/fixtures/claude.jsonl"), transcript);
  await copyFile(
    join(root, "test/fixtures/prices.json"),
    join(consumer, "prices.json"),
  );
  const original = await readFile(transcript);
  const args = ["--claude-dir", "profile", "--prices-file", "prices.json"];
  for (const [name, invoke] of [
    ["standalone Node", direct],
    ["npx", launch],
    ["bunx without Node", bunLaunch],
  ]) {
    const beforeDownload = downloads;
    const help = await invoke(["--help"]);
    if (name !== "standalone Node")
      assert.ok(
        downloads > beforeDownload,
        `${name} must download the checked tarball into a clean cache`,
      );
    assert.equal(help.status, 0, help.stderr);
    assert.match(help.stdout, /Usage: token-max/);
    const version = await invoke(["--version"]);
    assert.equal(version.status, 0, version.stderr);
    assert.equal(version.stdout, artifact.version + "\n");
    await writeFile(settings, '{"promptCacheTtl":"5m"}');
    const text = await invoke([...args, "--plain"]);
    assert.equal(text.status, 1, text.stderr);
    assert.match(text.stdout, /CHANGE.*claude-cache-ttl/);
    assert.match(text.stdout, /Estimated net saving vs baseline: \$/);
    const json = await invoke([...args, "--json"]);
    assert.equal(json.status, 1, json.stderr);
    const report = JSON.parse(json.stdout);
    assert.equal(report.schemaVersion, 1);
    assert.equal(report.scan.requests.length, 2);
    const ttl = report.checks.find(
      (check) => check.check === "claude-cache-ttl",
    );
    assert.equal(ttl.action, "change");
    assert.equal(ttl.recommended, "1h");
    assert.doesNotMatch(
      text.stdout + json.stdout + text.stderr + json.stderr,
      /SYNTHETIC_PRIVATE/,
    );
    assert.deepEqual(await readFile(transcript), original);
    assert.equal(await readFile(settings, "utf8"), '{"promptCacheTtl":"5m"}');
    await writeFile(settings, '{"promptCacheTtl":"1h"}');
    const clear = await invoke([...args, "--json"]);
    assert.equal(clear.status, 0, clear.stderr);
    assert.ok(
      JSON.parse(clear.stdout).checks.every(
        (check) => check.action !== "change",
      ),
    );
    const invalid = await invoke(["--bogus", "--json"]);
    assert.equal(invalid.status, 2, invalid.stderr);
    assert.equal(invalid.stdout, "");
    assert.match(
      JSON.parse(invalid.stderr.trim().split("\n").at(-1)).error,
      /invalid arguments/,
    );
    await writeFile(
      transcript,
      '{"type":"assistant","private":"SYNTHETIC_PRIVATE",BROKEN}\n',
    );
    const failure = await invoke([...args, "--json"]);
    assert.equal(failure.status, 2, failure.stderr);
    assert.equal(failure.stdout, "");
    assert.equal(
      JSON.parse(failure.stderr.trim().split("\n").at(-1)).location.line,
      1,
    );
    assert.doesNotMatch(failure.stderr, /SYNTHETIC_PRIVATE/);
    await writeFile(transcript, original);
    console.log(
      `Packed artifact passed: ${name}, ${process.platform}/${process.arch}`,
    );
  }
  assert.ok(
    downloads >= 2,
    "both launchers must download the checked tarball into clean caches",
  );
} finally {
  await new Promise((resolve, reject) =>
    server.close((error) => (error ? reject(error) : resolve())),
  );
  await rm(scratch, { recursive: true, force: true });
}
