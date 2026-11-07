import { readFile } from "node:fs/promises";
import { identity, ScanError } from "./safe.js";

declare const BUILD_ID: string;
declare const HELPER_DIGEST: string;

/** One verified packaged helper and the fingerprint shared by both bundles. */
export interface HelperArtifact {
  source: string;
  build: string;
}

/** Refuse missing or mixed artifacts before contacting any destination. */
export async function helperArtifact(): Promise<HelperArtifact> {
  if (typeof BUILD_ID === "undefined" || typeof HELPER_DIGEST === "undefined")
    throw new ScanError(
      "SSH requires the bundled CLI; run bun run build, then dist/cli.js",
    );
  let source: string;
  try {
    source = await readFile(
      new URL("./scan-helper.js", import.meta.url),
      "utf8",
    );
  } catch {
    throw new ScanError("packaged SSH helper is missing");
  }
  if (identity(source) !== HELPER_DIGEST)
    throw new ScanError(
      "packaged SSH helper does not match this checked build",
    );
  return { source, build: BUILD_ID };
}
