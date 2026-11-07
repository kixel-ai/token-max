import { createHash } from "node:crypto";
import type { Location } from "./model.js";

/** An error whose message is safe for terminal and JSON diagnostics. */
export class ScanError extends Error {
  constructor(
    message: string,
    readonly location?: Location,
  ) {
    super(message);
  }
}
/** Accept objects without trusting their prototype or retaining surrounding content. */
export function object(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}
/** Retain only metadata identifiers, never free-form strings. */
export function label(value: unknown): string | null {
  return typeof value === "string" &&
    /^[a-zA-Z0-9_.:/@<>+-]{1,160}$/.test(value)
    ? value
    : null;
}
/** Opaque identifiers prevent accidentally exposing content put in an ID field. */
export function identity(value: string): string {
  return createHash("sha256").update(value).digest("hex");
}
/** Preserve absent counts as unknown and reject corrupt counts. */
export function count(value: unknown): number | null {
  if (value === undefined || value === null) return null;
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0)
    throw new ScanError("invalid token count or duration");
  return value;
}
/** Recognize recorded instants without date-only timezone guesses. */
export function timestamp(value: unknown): number | null {
  if (typeof value !== "string" || !/^\d{4}-\d\d-\d\dT/.test(value))
    return null;
  const result = Date.parse(value);
  return Number.isFinite(result) ? result : null;
}
/** Read a required opaque ID without including its value in errors. */
export function id(value: unknown): string {
  if (typeof value !== "string" || value.length === 0)
    throw new ScanError("missing request or session identity");
  return identity(value);
}
/** Print file paths without allowing terminal control sequences. */
export function display(value: string): string {
  return terminalText(JSON.stringify(value));
}

/** Escape terminal controls and direction overrides without dropping metadata. */
export function terminalText(value: string): string {
  return value.replace(
    /[\u0000-\u001f\u007f-\u009f\u2028-\u202e\u2066-\u2069]/g,
    (character) =>
      `\\u${character.charCodeAt(0).toString(16).padStart(4, "0")}`,
  );
}
