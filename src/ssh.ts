import { spawn } from "node:child_process";
import {
  decode,
  envelopeSchema,
  fields,
  PROTOCOL,
  type Envelope,
} from "./protocol.js";
import type { HelperArtifact } from "./artifact.js";
import type { Scan } from "./model.js";
import { display, ScanError } from "./safe.js";
import { validateScan } from "./validate.js";

/** A capability-selected runtime on one explicitly selected POSIX destination. */
export interface RemoteRuntime {
  kind: "node" | "bun";
  version: string;
  path: string;
  platform: "linux" | "darwin";
}

const runtimeSchema = fields({
  kind: { union: [{ literal: "node" }, { literal: "bun" }] },
  version: { scalar: "string" },
  path: { scalar: "string" },
  platform: { scalar: "string" },
});
const preflightSchema = fields({
  protocol: { literal: PROTOCOL },
  platform: { scalar: "string" },
  node: { union: [{ literal: null }, runtimeSchema] },
  bun: { union: [{ literal: null }, runtimeSchema] },
});

function quote(value: string): string {
  return "'" + value.replaceAll("'", "'\\''") + "'";
}

/** Accept OpenSSH aliases and user/host addresses, excluding options and shell syntax. */
export function destination(value: string): string {
  if (
    !/^(?:[a-zA-Z0-9_][a-zA-Z0-9_.+-]*@)?(?:[a-zA-Z0-9_][a-zA-Z0-9_.+-]*|\[[a-fA-F0-9:.]+\])$/.test(
      value,
    )
  )
    throw new ScanError(
      "invalid --ssh destination; use an OpenSSH alias or user@host",
    );
  return value;
}

function probe(kind: "node" | "bun"): string {
  const version =
    kind === "node" ? "process.versions.node" : "process.versions.bun";
  const code = `process.stdout.write(JSON.stringify({kind:${JSON.stringify(kind)},version:${version},path:process.execPath,platform:process.platform}))`;
  return `if command -v ${kind} >/dev/null 2>&1; then ${kind} -e ${quote(code)}; else printf null; fi`;
}

// No remote file, registry, runtime installer, shell interpolation of user input,
// T3 catalog or settings mutation is involved in capability discovery.
const PREFLIGHT = `printf '{"protocol":1,"platform":"'; uname -s | tr -d '\\n'; printf '","node":'; ${probe("node")}; printf ',"bun":'; ${probe("bun")}; printf '}'`;

/** Select once from advertised capabilities; execution never falls back to another runtime. */
export function selectRuntime(value: {
  platform: string;
  node: RemoteRuntime | null;
  bun: RemoteRuntime | null;
}): RemoteRuntime {
  const platform =
    value.platform === "Linux"
      ? "linux"
      : value.platform === "Darwin"
        ? "darwin"
        : null;
  if (!platform)
    throw new ScanError("unsupported SSH platform; POSIX Linux/macOS required");
  for (const kind of ["node", "bun"] as const) {
    const candidate = value[kind];
    if (!candidate) continue;
    const version = /^(\d+)\.(\d+)\.(\d+)$/.exec(candidate.version);
    const parts = version?.slice(1).map(Number);
    const supported =
      parts &&
      (kind === "node"
        ? parts[0]! >= 22
        : parts[0] === 1 &&
          (parts[1]! > 4 || (parts[1] === 4 && parts[2]! >= 4)));
    if (
      candidate.kind === kind &&
      candidate.platform === platform &&
      candidate.path.startsWith("/") &&
      !candidate.path.includes("\0") &&
      supported
    )
      return candidate;
  }
  throw new ScanError(
    `unsupported SSH runtime; require installed Node >=22 or Bun >=1.4.4 (1.x) on PATH; found Node ${/^\d+\.\d+\.\d+$/.test(value.node?.version ?? "") ? value.node!.version : "unavailable"}, Bun ${/^\d+\.\d+\.\d+$/.test(value.bun?.version ?? "") ? value.bun!.version : "unavailable"}`,
  );
}

// The helper's source is streamed as one checked base64 line. Keep stdin open
// after loading it: EOF/cancel exits the remote runtime immediately, even while
// it is scanning and has no stdout to write. No remote child is left behind.
function bootstrap(since: number | undefined): string {
  return `let code='';const stop=()=>process.exit(2);process.stdin.once('end',stop);process.stdin.on('error',stop);const load=chunk=>{code+=chunk;const end=code.indexOf('\\n');if(end<0)return;process.stdin.off('data',load);process.stdin.on('data',stop);globalThis.tokenMaxSince=${JSON.stringify(since ?? null)}??undefined;import('data:text/javascript;base64,'+code.slice(0,end)).then(()=>{process.stdin.off('end',stop);process.stdin.off('data',stop);process.stdin.destroy();},()=>{process.stderr.write('token-max helper failed\\n');process.exit(2);});};process.stdin.setEncoding('utf8');process.stdin.on('data',load);`;
}

interface TransportOptions {
  host: string;
  signal?: AbortSignal;
}

async function exchange<T>(
  options: TransportOptions,
  command: string,
  schema: Parameters<typeof decode>[1],
  input?: string,
): Promise<T> {
  options.signal?.throwIfAborted();
  const child = spawn(
    "ssh",
    [
      "-T",
      "-o",
      "BatchMode=yes",
      "-o",
      "ClearAllForwardings=yes",
      "-o",
      "PermitLocalCommand=no",
      "-o",
      "RemoteCommand=none",
      "--",
      options.host,
      command,
    ],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  // Never echo remote stderr: banners, runtime exceptions and login scripts can
  // contain credentials or content. Classify failures using exit/control data.
  child.stderr.resume();
  child.stdin.on("error", () => {});
  let spawnFailed = false;
  const completion = new Promise<number | null>((resolve) => {
    child.once("error", () => {
      spawnFailed = true;
    });
    child.once("close", (code) => resolve(code));
  });
  const cancel = () => {
    if (input === undefined) child.kill("SIGTERM");
    else child.stdin.end();
  };
  options.signal?.addEventListener("abort", cancel, { once: true });
  try {
    if (input === undefined) child.stdin.end();
    else child.stdin.write(input);
    let data: T | undefined;
    let failed = false;
    let stoppedForProtocol = false;
    try {
      data = await decode<T>(child.stdout, schema);
    } catch {
      failed = true;
      child.stdin.end();
      if (!child.stdout.readableEnded) {
        stoppedForProtocol = true;
        child.kill("SIGTERM");
      }
    }
    const code = await completion;
    options.signal?.throwIfAborted();
    if (spawnFailed) throw new ScanError("cannot start OpenSSH client");
    if (code === 255)
      throw new ScanError(
        "OpenSSH failed; check authentication, configuration and host keys using ssh",
      );
    if (failed)
      throw new ScanError(
        code === 0 || stoppedForProtocol
          ? "invalid remote metadata protocol"
          : "SSH runtime or POSIX preflight failed; no report produced",
      );
    // A helper's valid failure envelope has exit 2. No success envelope may do so.
    if (code !== 0 && !(code === 2 && !!(data as Envelope).failure))
      throw new ScanError("SSH runtime failed; no report produced");
    return data!;
  } finally {
    options.signal?.removeEventListener("abort", cancel);
    child.stdin.end();
    if (child.exitCode === null && child.signalCode === null)
      child.kill("SIGTERM");
    await completion;
  }
}

/** Run only discovery/readers/exposure on a host, leaving pricing and checks local. */
export async function remoteScan(
  options: TransportOptions & { artifact: HelperArtifact; since?: number },
): Promise<{ scan: Scan; runtime: RemoteRuntime }> {
  const host = destination(options.host);
  try {
    const capabilities = await exchange<{
      platform: string;
      node: RemoteRuntime | null;
      bun: RemoteRuntime | null;
    }>(options, `sh -c ${quote(PREFLIGHT)}`, preflightSchema);
    const runtime = selectRuntime(capabilities);
    const command = `sh -c ${quote(`exec ${quote(runtime.path)} --input-type=module -e ${quote(bootstrap(options.since))}`)}`;
    const envelope = await exchange<Envelope>(
      options,
      command,
      envelopeSchema(options.artifact.build),
      Buffer.from(options.artifact.source).toString("base64") + "\n",
    );
    if (!envelope.complete || !!envelope.scan === !!envelope.failure)
      throw new ScanError("invalid remote completion metadata");
    if (envelope.failure)
      throw new ScanError(
        "remote scan failed; no report produced",
        envelope.failure.location
          ? { ...envelope.failure.location, host }
          : undefined,
      );
    if (envelope.scan!.since !== (options.since ?? null))
      throw new ScanError("remote selection does not match requested window");
    validateScan(envelope.scan!);
    return { scan: envelope.scan!, runtime };
  } catch (error) {
    if (options.signal?.aborted) throw error;
    if (error instanceof ScanError)
      throw new ScanError(
        `SSH ${display(host)}: ${error.message}`,
        error.location,
      );
    throw new ScanError(
      `SSH ${display(host)}: audit failed; no report produced`,
    );
  }
}
