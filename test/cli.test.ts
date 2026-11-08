import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { resolve, join } from "node:path";
import { readFile, readdir, writeFile } from "node:fs/promises";
import { watch } from "node:fs";
import { fixture, workspace } from "./helpers.js";

function environment(home: string): NodeJS.ProcessEnv {
  return {
    ...process.env,
    HOME: home,
    CLAUDE_CONFIG_DIR: "",
    CODEX_HOME: "",
    FORCE_PROMPT_CACHING_5M: "",
    ENABLE_PROMPT_CACHING_1H: "",
    CLAUDE_CODE_PROMPT_CACHE_TTL: "",
    CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL: "",
    CLAUDE_CODE_AUTO_COMPACT_WINDOW: "",
    DISABLE_AUTO_COMPACT: "",
    NO_COLOR: "",
    TERM: "xterm-256color",
  };
}

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
      '{"timestamp":"2020-01-01T00:00:00Z","type":"assistant","private":"SYNTHETIC_PRIVATE_REPLY","message":{"usage":{"input_tokens":"bad"}}}\n',
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

test("built CLI renders priced compaction windows from synthetic transcripts alongside TTL checks", async () => {
  const w = await workspace();
  try {
    const file = join(w.claude, "projects/project/main.jsonl");
    await fixture(file, "claude.jsonl");
    const records = (await readFile(file, "utf8"))
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line));
    const api = records[0];
    api.id = "sample-api";
    const message = records[1];
    message.requestRef = "sample-api";
    message.timestamp = "2026-11-01T00:00:01Z";
    const usage = (write: number, input = 0, output = 10) => ({
      ...message.message.usage,
      input_tokens: input,
      output_tokens: output,
      cache_creation_input_tokens: write,
      cache_creation: {
        ephemeral_5m_input_tokens: write,
        ephemeral_1h_input_tokens: 0,
      },
    });
    const initial = usage(20000);
    const summary = usage(0, 300000, 2000);
    message.message.usage = {
      ...initial,
      iterations: [
        { ...initial, type: "message" },
        { ...summary, type: "compaction" },
      ],
    };
    const boundary = records.at(-1);
    boundary.timestamp = "2026-11-01T00:00:02Z";
    boundary.requestRef = "sample-api";
    boundary.compactMetadata = {
      trigger: "auto",
      preTokens: 300000,
      postTokens: 2000,
    };
    const resume = structuredClone(message);
    delete resume.requestRef;
    resume.uuid = "sample-resumed-block";
    resume.message.id = "sample-resume";
    resume.timestamp = "2026-11-01T00:00:03Z";
    resume.message.usage = initial;
    const large = Array.from({ length: 4 }, (_, i) => {
      const r = structuredClone(message);
      delete r.requestRef;
      r.uuid = `large-block-${i}`;
      r.message.id = `large-${i}`;
      r.timestamp = `2026-11-01T0${i + 1}:00:00Z`;
      r.message.usage = {
        ...initial,
        iterations: [
          {
            ...initial,
            type: "message",
            cache_creation_input_tokens: 150000,
            cache_creation: {
              ephemeral_5m_input_tokens: 150000,
              ephemeral_1h_input_tokens: 0,
            },
          },
        ],
      };
      return r;
    });
    await writeFile(
      file,
      [api, message, boundary, resume, ...large]
        .map((r) => JSON.stringify(r))
        .join("\n") + "\n",
    );
    await writeFile(
      join(w.claude, "settings.json"),
      '{"promptCacheTtl":"5m","autoCompactWindow":400000}',
    );
    const args = [
      resolve("dist/src/cli.js"),
      "--claude-dir",
      w.claude,
      "--prices-file",
      resolve("test/fixtures/prices.json"),
    ];
    const env = environment(w.home);
    const structured = spawnSync(process.execPath, [...args, "--json"], {
      env,
      encoding: "utf8",
    });
    const report = JSON.parse(structured.stdout);
    assert.equal(structured.status, 1, structured.stderr);
    const result = report.checks.find(
      (c: { check: string }) => c.check === "compaction-window",
    );
    assert.equal(result.action, "change");
    assert.equal(result.current, 400000);
    assert.equal(result.recommended, 100000);
    const text = spawnSync(
      process.execPath,
      [...args, "--details", "--plain"],
      { env, encoding: "utf8" },
    );
    assert.equal(text.status, 1, text.stderr);
    assert.equal(text.stderr, "");
    for (const value of [
      "claude-cache-ttl",
      "CHANGE  compaction-window",
      "Current: 400000 tokens",
      "Suggested: 100000 tokens",
      "Conditional TTL baseline",
      "Added compaction",
      "Geometry basis",
      "compaction loses detail",
      result.owner,
    ])
      assert.ok(text.stdout.includes(value), value);
    assert.doesNotMatch(text.stdout, /SYNTHETIC_PRIVATE|\x1b|total sav/i);
  } finally {
    await w.dispose();
  }
});

test("built text CLI handles all-clear, empty windows, missing histories, offline prices and help", async () => {
  const w = await workspace();
  try {
    const args = [
      resolve("dist/src/cli.js"),
      "--claude-dir",
      w.claude,
      "--prices-file",
      resolve("test/fixtures/prices.json"),
    ];
    const env = environment(w.home);
    const run = (extra: string[] = [], flags = args) =>
      spawnSync(process.execPath, [...flags, ...extra], {
        env,
        encoding: "utf8",
      });
    const empty = run();
    assert.equal(empty.status, 0, empty.stderr);
    assert.equal(empty.stderr, "");
    assert.match(empty.stdout, /No transcript histories found/);
    assert.match(empty.stdout, /--claude-dir/);
    await fixture(
      join(w.claude, "projects/project/main.jsonl"),
      "claude.jsonl",
    );
    await writeFile(join(w.claude, "settings.json"), '{"promptCacheTtl":"1h"}');
    const clear = run(["--plain"]);
    assert.equal(clear.status, 0, clear.stderr);
    assert.match(clear.stdout, /No setting changes recommended/);
    assert.match(clear.stdout, /^Keep$/m);
    assert.doesNotMatch(clear.stdout, /\x1b|→|SYNTHETIC_PRIVATE/);
    assert.equal(clear.stderr, "");
    const none = run(["--since", "2027-01-01"]);
    assert.equal(none.status, 0, none.stderr);
    assert.match(none.stdout, /No requests found/);
    assert.match(none.stdout, /earlier --since/);
    const offline = run(
      ["--offline", "--price-cache", resolve("test/fixtures/prices.json")],
      args.slice(0, 3),
    );
    assert.equal(offline.status, 0, offline.stderr);
    assert.match(offline.stdout, /Prices: cached from/);
    const failure = run(
      ["--offline", "--price-cache", join(w.home, "absent-prices")],
      args.slice(0, 3),
    );
    assert.equal(failure.status, 2);
    assert.equal(failure.stdout, "");
    assert.match(failure.stderr, /fetch prices once/);
    const help = run(["--help"]);
    assert.equal(help.status, 0, help.stderr);
    assert.match(help.stdout, /^  --since /m);
    assert.match(help.stdout, /^  token-max --since /m);
    for (const row of help.stdout
      .split("\n")
      .filter((line) => /^  -/.test(line))) {
      const option = /^ {2}-.*? {2,}(?=\S)/.exec(row)!;
      assert.equal(option[0].length, 26, row);
    }
    const invalid = run(["--bogus"]);
    assert.equal(invalid.status, 2);
    assert.equal(invalid.stdout, "");
    assert.equal((invalid.stderr.match(/use --help/gi) ?? []).length, 1);
    for (const flag of [
      "--since",
      "--claude-dir",
      "--codex-dir",
      "--details",
      "--plain",
      "--json",
      "NO_COLOR",
      "Exit 0",
      "Exit 1",
      "Exit 2",
    ])
      assert.ok(help.stdout.includes(flag), flag);
  } finally {
    await w.dispose();
  }
});

test("built CLI confines progress to interactive stderr and honors NO_COLOR, plain, narrow output and JSON", async () => {
  const w = await workspace();
  try {
    await fixture(
      join(w.claude, "projects/project/main.jsonl"),
      "claude.jsonl",
    );
    await writeFile(join(w.claude, "settings.json"), '{"promptCacheTtl":"5m"}');
    // Simulate stream capabilities, leaving the built command and all I/O real.
    const terminal = join(w.home, "terminal.mjs");
    await writeFile(
      terminal,
      `Object.defineProperty(process.stdout, 'isTTY', {value: true});
Object.defineProperty(process.stderr, 'isTTY', {value: true});
process.stdout.columns = 40;`,
    );
    const args = [
      "--import",
      terminal,
      resolve("dist/src/cli.js"),
      "--claude-dir",
      w.claude,
      "--prices-file",
      resolve("test/fixtures/prices.json"),
    ];
    const env = environment(w.home);
    const run = (extra: string[] = [], vars = env) =>
      spawnSync("node", [...args, ...extra], { env: vars, encoding: "utf8" });
    const rich = run();
    assert.equal(rich.status, 1, rich.stderr);
    assert.match(rich.stdout, /\x1b\[/);
    for (const phase of [
      "Scanning local histories",
      "Loading public prices",
      "Checking settings",
    ])
      assert.ok(rich.stderr.includes(phase));
    assert.doesNotMatch(
      rich.stdout,
      /Scanning|Loading public|Checking settings|SYNTHETIC_PRIVATE/,
    );
    const noColor = run([], { ...env, NO_COLOR: "1" });
    assert.equal(noColor.status, 1, noColor.stderr);
    assert.doesNotMatch(noColor.stdout, /\x1b/);
    assert.ok(noColor.stdout.split("\n").every((line) => line.length <= 40));
    const utf8 = run([], { ...env, LC_ALL: "en_US.UTF-8" });
    assert.match(utf8.stdout, /→/);
    const ascii = run([], { ...env, LC_ALL: "C", LC_CTYPE: "en_US.UTF-8" });
    assert.doesNotMatch(ascii.stdout, /→/);
    const help = run(["--help"]);
    assert.match(help.stdout, /^  --since /m);
    assert.match(help.stdout, /^  token-max --since /m);
    assert.ok(help.stdout.split("\n").every((line) => line.length <= 40));
    assert.match(help.stdout, /^ {26}\S/m);
    const plain = run(["--plain", "--details"]);
    assert.equal(plain.status, 1, plain.stderr);
    assert.doesNotMatch(plain.stdout + plain.stderr, /\x1b|→/);
    assert.match(plain.stdout, /Uncertainty:/);
    const json = run(["--json", "--details", "--plain"]);
    assert.equal(json.status, 1, json.stderr);
    assert.equal(json.stderr, "");
    const report = JSON.parse(json.stdout);
    assert.equal(report.schemaVersion, 1);
    assert.equal(report.scan.requests.length, 2);
    assert.equal(report.checks[0].action, "change");
    const piped = spawnSync("node", args.slice(2), { env, encoding: "utf8" });
    assert.equal(piped.stderr, "");
    assert.doesNotMatch(piped.stdout, /\x1b|→/);
    assert.ok(
      piped.stdout
        .split("\n")
        .includes("      Set in ~/.claude/settings.json:promptCacheTtl"),
      "piped paths remain on one copyable line",
    );
    const dumb = run([], { ...env, TERM: "dumb" });
    assert.equal(dumb.stderr, "");
    assert.doesNotMatch(dumb.stdout, /\x1b|→/);
    await writeFile(
      join(w.claude, "projects/project/main.jsonl"),
      '{"private":"SYNTHETIC_PRIVATE",BROKEN}\n',
    );
    const skipped = run();
    assert.notEqual(skipped.status, 2);
    assert.match(skipped.stdout, /1 unreadable transcript\s+lines in 1 files/);
    assert.doesNotMatch(skipped.stdout + skipped.stderr, /SYNTHETIC_PRIVATE/);
  } finally {
    await w.dispose();
  }
});

test("SIGINT and SIGTERM abort pricing atomically in text and JSON modes", async () => {
  const w = await workspace();
  try {
    await fixture(
      join(w.claude, "projects/project/main.jsonl"),
      "claude.jsonl",
    );
    const preload = join(w.home, "pending-prices.mjs");
    // No network or sleeps: IPC tells the parent exactly when fetch is pending.
    await writeFile(
      preload,
      `process.on('message', () => {});
globalThis.fetch = (_url, {signal}) => new Promise((_resolve, reject) => {
  signal.addEventListener('abort', () => { reject(signal.reason); process.disconnect(); }, {once: true});
  process.send('fetching');
});`,
    );
    for (const signal of ["SIGINT", "SIGTERM"] as const) {
      for (const json of [false, true]) {
        const child = spawn(
          "node",
          [
            "--import",
            preload,
            resolve("dist/src/cli.js"),
            "--claude-dir",
            w.claude,
            "--price-cache",
            join(w.home, "prices.json"),
            ...(json ? ["--json"] : []),
          ],
          {
            env: environment(w.home),
            stdio: ["ignore", "pipe", "pipe", "ipc"],
          },
        );
        let stdout = "";
        let stderr = "";
        child.stdout!.setEncoding("utf8").on("data", (chunk) => {
          stdout += chunk;
        });
        child.stderr!.setEncoding("utf8").on("data", (chunk) => {
          stderr += chunk;
        });
        child.once("message", () => child.kill(signal));
        const status = await new Promise<number | null>((resolve, reject) => {
          child.once("error", reject);
          child.once("close", resolve);
        });
        assert.equal(status, 2, stderr);
        assert.equal(stdout, "");
        assert.doesNotMatch(
          stderr,
          /SYNTHETIC_PRIVATE|recommended|schemaVersion/,
        );
        assert.match(
          json ? JSON.parse(stderr).error : stderr,
          /cancelled; no report produced/,
        );
      }
    }
    const stalled = join(w.home, "stalled-prices.mjs");
    await writeFile(
      stalled,
      `process.on('message', () => {});
globalThis.fetch = (_url, {signal}) => {
  signal.addEventListener('abort', () => process.send('aborted'), {once: true});
  return new Response(new ReadableStream({start(controller) {
    controller.enqueue(new TextEncoder().encode('{"anthropic":{"models":{'));
  }}));
};`,
    );
    const child = spawn(
      "node",
      [
        "--import",
        stalled,
        resolve("dist/src/cli.js"),
        "--claude-dir",
        w.claude,
        "--price-cache",
        join(w.home, "forced-prices.json"),
      ],
      { env: environment(w.home), stdio: ["ignore", "pipe", "pipe", "ipc"] },
    );
    let interrupted = false;
    const observer = watch(w.home, (_event, name) => {
      if (!interrupted && name?.startsWith("forced-prices.json.")) {
        interrupted = true;
        child.kill("SIGINT");
      }
    });
    child.once("close", () => observer.close());
    let output = "";
    child.stdout!.setEncoding("utf8").on("data", (chunk) => {
      output += chunk;
    });
    child.stderr!.setEncoding("utf8").on("data", (chunk) => {
      output += chunk;
    });
    child.on("message", () => child.kill("SIGTERM"));
    const status = await new Promise<number | null>((resolve, reject) => {
      child.once("error", reject);
      child.once("close", resolve);
    });
    assert.equal(status, 2);
    assert.ok(
      interrupted,
      "force cancellation starts after a real temporary cache file exists",
    );
    assert.ok(
      !(await readdir(w.home)).some((name) =>
        name.startsWith("forced-prices.json."),
      ),
      "force cancellation removes the partial price cache",
    );
    assert.equal(
      output,
      "",
      "force cancellation must not report success even if I/O ignores abort",
    );
  } finally {
    await w.dispose();
  }
});
