import { scan } from "./reader.js";
import { PROTOCOL, transmit, type Envelope } from "./protocol.js";
import { ScanError } from "./safe.js";

declare const BUILD_ID: string;

// The coordinator supplies only an optional time bound, never local paths,
// environment variables, price data, or transcript content.
try {
  const since: unknown = Reflect.get(globalThis, "tokenMaxSince");
  if (
    since !== undefined &&
    (typeof since !== "number" || !Number.isFinite(since))
  )
    throw new ScanError("invalid selection");
  const data = await scan(typeof since === "number" ? { since } : {});
  await transmit(process.stdout, {
    protocol: PROTOCOL,
    build: BUILD_ID,
    scan: data,
    complete: true,
  } satisfies Envelope);
} catch (error) {
  try {
    await transmit(process.stdout, {
      protocol: PROTOCOL,
      build: BUILD_ID,
      failure: {
        code: "scan-failed",
        ...(error instanceof ScanError && error.location
          ? { location: error.location }
          : {}),
      },
      complete: true,
    } satisfies Envelope);
  } catch {
    /* A closed transport cannot receive a diagnostic. */
  }
  process.exitCode = 2;
}
