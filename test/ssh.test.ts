import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { chmod, mkdir, readFile, symlink, writeFile } from "node:fs/promises";
import { watch } from "node:fs";
import { join, resolve, delimiter } from "node:path";
import { Readable, Writable } from "node:stream";
import { once } from "node:events";
import { fixture, prices, workspace } from "./helpers.js";
import { combineHosts, type HostScan } from "../src/hosts.js";
import { destination, remoteScan, selectRuntime } from "../src/ssh.js";
import { decode, envelopeSchema, transmit } from "../src/protocol.js";
import { scan } from "../src/reader.js";
import { analyze } from "../src/replay.js";
import { CacheChain } from "../src/cache.js";
import type { HelperArtifact } from "../src/artifact.js";
import { validateScan } from "../src/validate.js";
import { formatReport } from "../src/report.js";

const posix = process.platform !== "win32";
const quote = (text: string) => "'" + text.replaceAll("'", "'\\''") + "'";
const runtime = (name: string) =>
  spawnSync(name, ["-p", "process.execPath"], {
    encoding: "utf8",
  }).stdout.trim();
const host = (
  data: Awaited<ReturnType<typeof scan>>,
  id: string,
): HostScan => ({
  scan: data,
  id,
  transport: "ssh",
  destination: id,
  platform: "linux",
  runtime: "node 24.0.0",
});

test("host selection rejects options and shell injection before any connection; runtime selection is explicit", () => {
  for (const name of [
    "nixos",
    "me@host-name",
    "[::1]",
    "me@[::1]",
    "a_b.example",
  ])
    assert.equal(destination(name), name);
  for (const name of [
    "-oProxyCommand=id",
    "x;id",
    "x y",
    "x\n",
    "$(id)",
    "x'",
    "",
    "ssh://x",
  ])
    assert.throws(() => destination(name));
  const node = {
    kind: "node" as const,
    version: "22.0.0",
    path: "/runtime with spaces/node's",
    platform: "linux" as const,
  };
  const bun = {
    kind: "bun" as const,
    version: "1.4.4",
    path: "/bun",
    platform: "linux" as const,
  };
  assert.equal(selectRuntime({ platform: "Linux", node, bun }), node);
  assert.equal(
    selectRuntime({
      platform: "Linux",
      node: { ...node, version: "20.1.0" },
      bun,
    }),
    bun,
  );
  for (const value of [
    { platform: "Windows_NT", node, bun },
    {
      platform: "Linux",
      node: { ...node, version: "20.1.0" },
      bun: { ...bun, version: "1.3.0" },
    },
    { platform: "Darwin", node, bun: null },
    { platform: "Linux", node: { ...node, path: "--eval" }, bun: null },
  ])
    assert.throws(() => selectRuntime(value));
});

test("streaming protocol validates types, fields, depth, duplicates, exact build and completion without raw diagnostics", async () => {
  const w = await workspace();
  try {
    await fixture(join(w.claude, "projects/project/a.jsonl"), "claude.jsonl");
    const data = await scan(w.options);
    const envelope = {
      protocol: 1,
      build: "checked",
      scan: data,
      complete: true,
    };
    const chunks: Buffer[] = [];
    const sink = new Writable({
      highWaterMark: 1,
      write(chunk, _encoding, done) {
        chunks.push(Buffer.from(chunk));
        done();
      },
    });
    await transmit(sink, envelope);
    assert.ok(
      chunks.length > 100,
      "tokens use pipe backpressure instead of a full serialized scan",
    );
    const result = await decode<typeof envelope>(
      Readable.from(chunks),
      envelopeSchema("checked"),
    );
    assert.deepEqual(result, JSON.parse(JSON.stringify(envelope)));
    validateScan(result.scan);
    const valid = JSON.stringify(envelope);
    for (const text of [
      valid.replace('"checked"', '"different"'),
      valid.replace('"complete":true', '"complete":false'),
      valid.replace('"protocol":1', '"protocol":1,"protocol":1'),
      valid.replace('"protocol":1', '"protocol":1,"prompt":"SYNTHETIC_SECRET"'),
      valid.replace('"protocol":1', '"protocol":1,"__proto__":{}'),
      valid.replace('"protocol":1', '"protocol":1,"scan":[[[[[[[['),
      valid.slice(0, -1),
      valid + "SYNTHETIC_SECRET",
      valid + valid,
      valid.replace('"input":100000', '"input":"SYNTHETIC_SECRET"'),
    ].filter((text) => text !== valid)) {
      await assert.rejects(
        decode(Readable.from([Buffer.from(text)]), envelopeSchema("checked")),
        /invalid remote metadata protocol/,
      );
    }
    const corrupt = structuredClone(data);
    corrupt.requests[0]!.parts[0]!.usage.input = -1;
    assert.throws(() => validateScan(corrupt));
  } finally {
    await w.dispose();
  }
});

test("copied native requests count once with all origins and separate settings owners; conflicting copies fail", async () => {
  const w = await workspace();
  try {
    await fixture(join(w.claude, "projects/project/a.jsonl"), "claude.jsonl");
    const data = await scan(w.options);
    const copy = structuredClone(data);
    copy.profiles[0]!.ttl.main.value = "1h";
    const merged = combineHosts([
      host(structuredClone(data), "machine A"),
      host(copy, "machine B"),
    ]);
    assert.equal(merged.requests.length, data.requests.length);
    assert.equal(merged.profiles.length, data.profiles.length * 2);
    assert.equal(
      merged.requests[0]!.origins!.length,
      data.requests[0]!.locations.length * 2,
    );
    assert.equal(merged.requests[0]!.profiles.length, 2);
    assert.equal(merged.requests[0]!.locations.at(-1)!.host, "machine B");
    assert.notEqual(
      merged.profiles[0]!.id,
      merged.profiles[data.profiles.length]!.id,
    );
    assert.match(merged.profiles[0]!.ttl.main.owner, /machine A/);
    assert.match(
      merged.profiles[data.profiles.length]!.ttl.main.owner,
      /machine B/,
    );
    const catalog = await prices();
    const original = analyze({ scan: data, catalog });
    const combined = analyze({ scan: merged, catalog });
    assert.deepEqual(
      combined.historical.map((h) => h.cost),
      original.historical.map((h) => h.cost),
    );
    assert.equal(combined.prices.source, original.prices.source);
    assert.match(formatReport(combined, { details: true }), /machine A/);
    assert.match(formatReport(combined, { details: true }), /machine B/);
    const conflict = structuredClone(data);
    conflict.requests[0]!.parts[0]!.usage.output!++;
    assert.throws(
      () =>
        combineHosts([host(structuredClone(data), "A"), host(conflict, "B")]),
      /copied request metadata conflicts/,
    );
  } finally {
    await w.dispose();
  }
});

test("identical paths/session aliases on separate hosts cannot warm one another's cache", async () => {
  const w = await workspace();
  try {
    await fixture(join(w.claude, "projects/project/a.jsonl"), "claude.jsonl");
    const data = await scan(w.options);
    const a = structuredClone(data),
      b = structuredClone(data);
    a.requests = [a.requests[0]!];
    b.requests = [b.requests[1]!];
    a.exposure.requests = a.exposure.requests.filter(
      (e) => e.request === a.requests[0]!.id,
    );
    b.exposure.requests = b.exposure.requests.filter(
      (e) => e.request === b.requests[0]!.id,
    );
    const merged = combineHosts([host(a, "A"), host(b, "B")]);
    assert.notEqual(merged.requests[0]!.session, merged.requests[1]!.session);
    const chain = new CacheChain(merged.events);
    for (const r of merged.requests) {
      const replay = chain.replay({
        request: r,
        p: r.parts[0]!,
        part: 0,
        estimated: false,
        ttl: "1h",
        cold: false,
        selected: true,
      });
      assert.ok(!replay?.evidence?.predecessors.length);
    }
  } finally {
    await w.dispose();
  }
});

async function transportFixture() {
  const w = await workspace();
  const bin = join(w.home, "bin");
  await mkdir(bin);
  const executable = runtime("node");
  const bun = runtime("bun");
  const runtimeDir = join(w.home, "runtime with spaces and 'quotes'");
  await mkdir(runtimeDir);
  const node = join(runtimeDir, "node runtime");
  await symlink(executable, node);
  const script = join(w.home, "ssh.mjs");
  await writeFile(
    script,
    `import {spawn} from 'node:child_process';
import {appendFileSync} from 'node:fs';
const args=process.argv.slice(2), host=args.at(-2), command=args.at(-1);
appendFileSync(process.env.SSH_LOG, JSON.stringify({host,command})+'\\n');
if(host==='failure'){process.stderr.write('SYNTHETIC_SECRET\\n');process.exit(255);}
if(command.includes('uname -s')) {
 process.stdout.write(JSON.stringify({protocol:1,platform:'Linux',node:host==='bun'?null:{kind:'node',version:'24.0.0',path:process.env.REMOTE_NODE,platform:'linux'},bun:{kind:'bun',version:'1.4.4',path:process.env.REMOTE_BUN,platform:'linux'}}));process.exit(0);
}
process.stderr.write('SYNTHETIC_SECRET stderr noise\\n');
const env={...process.env,HOME:process.env.REMOTE_HOME,CLAUDE_CONFIG_DIR:'',CODEX_HOME:'',XDG_CACHE_HOME:process.env.REMOTE_HOME+'/.cache'};
const child=spawn('/bin/sh',['-c',command],{stdio:host==='corrupt'||host==='mismatch'?['inherit','pipe','inherit']:'inherit',env});
if(child.stdout){let data='';for await(const chunk of child.stdout)data+=chunk;
 if(host==='corrupt')process.stdout.write(data+'SYNTHETIC_SECRET');
 else process.stdout.write(data.replace(/"build":"[a-f0-9]+"/,'"build":"wrong"'));
}
child.on('close',code=>process.exit(code??2));
`,
  );
  await writeFile(
    join(bin, "ssh"),
    `#!/bin/sh\nexec ${quote(executable)} ${quote(script)} "$@"\n`,
  );
  await chmod(join(bin, "ssh"), 0o755);
  const log = join(w.home, "ssh.log");
  await writeFile(log, "");
  const env: NodeJS.ProcessEnv = {
    ...process.env,
    PATH: bin + delimiter + process.env.PATH,
    SSH_LOG: log,
    REMOTE_NODE: node,
    REMOTE_BUN: bun,
    REMOTE_HOME: w.home,
  };
  for (const key of Object.keys(env))
    if (
      /^(claude_|codex_|force_prompt_|enable_prompt_|disable_auto_compact$)/i.test(
        key,
      )
    )
      delete env[key];
  return { ...w, env, log };
}

test(
  "bundled SSH CLI uses actual Node/Bun helpers, local-only paths, one catalog and atomic failures",
  { skip: !posix },
  async () => {
    const w = await transportFixture();
    try {
      const transcript = join(w.claude, "projects/project/main.jsonl");
      await fixture(transcript, "claude.jsonl");
      const before = await readFile(transcript);
      const invoke = (flags: string[]) =>
        spawnSync(
          "node",
          [
            resolve("dist/cli.js"),
            "--prices-file",
            resolve("test/fixtures/prices.json"),
            "--json",
            ...flags,
          ],
          { env: w.env, encoding: "utf8" },
        );
      const result = invoke(["--ssh", "node", "--ssh", "bun"]);
      assert.ok(result.status === 0 || result.status === 1, result.stderr);
      assert.equal(result.stderr, "");
      const report = JSON.parse(result.stdout);
      assert.equal(report.scan.hosts.length, 2);
      assert.match(report.scan.hosts[0].runtime, /^node/);
      assert.match(report.scan.hosts[1].runtime, /^bun/);
      assert.equal(report.scan.requests.length, 2);
      assert.equal(
        new Set(
          report.scan.requests[0].origins.map((o: { host: string }) => o.host),
        ).size,
        2,
      );
      assert.doesNotMatch(
        result.stdout + result.stderr,
        /SYNTHETIC_SECRET|SYNTHETIC_PRIVATE/,
      );
      const local = invoke([
        "--ssh",
        "node",
        "--include-local",
        "--claude-dir",
        w.claude,
      ]);
      assert.ok(local.status === 0 || local.status === 1, local.stderr);
      assert.equal(JSON.parse(local.stdout).scan.hosts[0].id, "local");
      for (const flags of [
        ["--ssh", "node", "--claude-dir", w.claude],
        ["--include-local"],
        ["--ssh", "node", "--ssh", "-oBad"],
        ["--ssh", "node", "--ssh", "failure"],
        ["--ssh", "mismatch"],
        ["--ssh", "corrupt"],
      ]) {
        const failure = invoke(flags);
        assert.equal(failure.status, 2, failure.stderr);
        assert.equal(failure.stdout, "");
        assert.doesNotMatch(
          failure.stderr,
          /SYNTHETIC_SECRET|SYNTHETIC_PRIVATE/,
        );
      }
      await writeFile(
        transcript,
        '{"type":"assistant","private":"SYNTHETIC_SECRET",BROKEN}\n',
      );
      const malformed = invoke(["--ssh", "node"]);
      assert.equal(malformed.status, 2);
      assert.equal(malformed.stdout, "");
      assert.equal(JSON.parse(malformed.stderr).location.line, 1);
      assert.equal(JSON.parse(malformed.stderr).location.host, "node");
      assert.doesNotMatch(malformed.stderr, /SYNTHETIC_SECRET/);
      await writeFile(transcript, before);
      assert.deepEqual(await readFile(transcript), before);
    } finally {
      await w.dispose();
    }
  },
);

test(
  "SSH cancellation closes stdin, terminates the remote bootstrap and reaps every subprocess",
  { skip: !posix },
  async () => {
    const w = await transportFixture();
    const oldPath = process.env.PATH;
    const keys = ["SSH_LOG", "REMOTE_NODE", "REMOTE_BUN", "REMOTE_HOME"];
    const old = Object.fromEntries(keys.map((key) => [key, process.env[key]]));
    try {
      process.env.PATH = w.env.PATH;
      for (const key of keys) process.env[key] = w.env[key];
      const ready = join(w.home, "ready");
      const controller = new AbortController();
      const observed = new Promise<void>((resolve) => {
        const watcher = watch(w.home, (_kind, file) => {
          if (file === "ready") {
            watcher.close();
            resolve();
          }
        });
      });
      const artifact: HelperArtifact = {
        build: "synthetic",
        source: `import {writeFileSync} from 'node:fs'; writeFileSync(${JSON.stringify(ready)},String(process.pid)); await new Promise(()=>{});`,
      };
      const running = remoteScan({
        host: "node",
        artifact,
        signal: controller.signal,
      });
      await observed;
      const pid = Number(await readFile(ready, "utf8"));
      controller.abort();
      await assert.rejects(running);
      assert.throws(() => process.kill(pid, 0), /ESRCH/);
    } finally {
      if (oldPath === undefined) delete process.env.PATH;
      else process.env.PATH = oldPath;
      for (const key of keys)
        if (old[key] === undefined) delete process.env[key];
        else process.env[key] = old[key];
      await w.dispose();
    }
  },
);

test(
  "a mismatched packaged helper is rejected before OpenSSH is invoked",
  { skip: !posix },
  async () => {
    const w = await workspace();
    try {
      await writeFile(join(w.home, "cli.mjs"), await readFile("dist/cli.js"));
      await writeFile(
        join(w.home, "scan-helper.js"),
        "throw new Error('SYNTHETIC_SECRET');",
      );
      const child = spawn(
        "node",
        [join(w.home, "cli.mjs"), "--ssh", "node", "--json"],
        { stdio: ["ignore", "pipe", "pipe"] },
      );
      let out = "",
        err = "";
      child.stdout.on("data", (chunk) => (out += chunk));
      child.stderr.on("data", (chunk) => (err += chunk));
      const [status] = await once(child, "close");
      assert.equal(status, 2);
      assert.equal(out, "");
      assert.match(err, /does not match this checked build/);
      assert.doesNotMatch(err, /SYNTHETIC_SECRET/);
    } finally {
      await w.dispose();
    }
  },
);
