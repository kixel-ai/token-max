import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { resolve, join } from "node:path";
import type { Request, Usage } from "../src/model.js";
import { loadPrices } from "../src/prices.js";
import { usage } from "../src/reader.js";

/** Every fixture and scratch directory is synthetic and checkout-local. */
export async function workspace() {
  const scratch = resolve(".tmp");
  await mkdir(scratch, { recursive: true });
  const home = await mkdtemp(join(scratch, "test-"));
  const claude = join(home, ".claude");
  const codex = join(home, ".codex");
  await mkdir(join(claude, "projects", "project"), { recursive: true });
  await mkdir(join(codex, "sessions"), { recursive: true });
  return {
    home,
    claude,
    codex,
    dispose: () => rm(home, { recursive: true, force: true }),
    options: {
      home,
      env: {},
      managedSettingsFile: join(home, "managed-settings.json"),
    },
  };
}
/** Copy a checked-in synthetic transcript into an isolated profile. */
export async function fixture(destination: string, name: string) {
  await writeFile(destination, await readFile(resolve("test/fixtures", name)));
}
/** Construct complete synthetic Claude usage with adjustable cache subsets. */
export function claudeUsage(
  options: {
    write?: number;
    read?: number;
    output?: number;
    ttl?: "5m" | "1h";
    input?: number;
  } = {},
): Usage {
  const {
    write = 1000,
    read = 0,
    output = 10,
    ttl = "5m",
    input = 0,
  } = options;
  return usage(
    {
      input_tokens: input,
      output_tokens: output,
      cache_read_input_tokens: read,
      cache_creation_input_tokens: write,
      cache_creation: {
        ephemeral_5m_input_tokens: ttl === "5m" ? write : 0,
        ephemeral_1h_input_tokens: ttl === "1h" ? write : 0,
      },
    },
    "claude",
  );
}
/** Construct one synthetic normalized request without reading a real session. */
export function makeRequest(overrides: Partial<Request> = {}): Request {
  return {
    evidence: "measured",
    id: "request",
    harness: "claude",
    session: "session",
    agent: "main",
    time: 0,
    endTime: 0,
    model: "claude-opus-5-5",
    provider: "anthropic",
    api: "messages",
    source: "sdk",
    trigger: null,
    reference: null,
    turn: null,
    version: "2.1.314",
    client: "sdk-ts",
    cwd: null,
    parts: [
      {
        kind: "message",
        model: "claude-opus-5-5",
        usage: claudeUsage(),
        tier: "standard",
        speed: "standard",
        geo: "global",
      },
    ],
    serverTools: {},
    profiles: ["profile"],
    locations: [{ file: "synthetic", line: 1 }],
    accounting: "individual",
    attempt: null,
    issues: [],
    ...overrides,
  };
}
/** Read deterministic synthetic public prices without using the network. */
export function prices() {
  return loadPrices({
    file: resolve("test/fixtures/prices.json"),
    cache: resolve(".tmp/unused-cache.json"),
  });
}
