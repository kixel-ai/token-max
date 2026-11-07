import { JSONParser, TokenType } from "@streamparser/json";
import type { Writable } from "node:stream";
import { once } from "node:events";
import { scanSchema } from "./wire-schema.js";
import type { Location, Scan } from "./model.js";
import { ScanError } from "./safe.js";

/** The private metadata protocol changes independently of report schema version 1. */
export const PROTOCOL = 1;

/** A schema derived from Scan, with no unconstrained object or executable payload. */
export interface Schema {
  literal?: string | number | boolean | null;
  scalar?: "string" | "number";
  array?: Schema;
  properties?: Record<string, Schema>;
  optional?: string[];
  values?: Schema;
  union?: Schema[];
  first?: string[];
}

/** Construct a strict schema for a small protocol control object. */
export function fields(
  properties: Record<string, Schema>,
  optional: string[] = [],
): Schema {
  return { properties, optional };
}

/** Scan metadata and a completion marker are required before any report is eligible. */
export interface Envelope {
  protocol: 1;
  build: string;
  scan?: Scan;
  failure?: { code: "scan-failed" | "cancelled"; location?: Location };
  complete: true;
}

/** Bind the schema and helper to the exact checked build rather than package 0.0.0. */
export function envelopeSchema(build: string): Schema {
  return {
    ...fields(
      {
        protocol: { literal: PROTOCOL },
        build: { literal: build },
        scan: scanSchema,
        failure: fields(
          {
            code: {
              union: [{ literal: "scan-failed" }, { literal: "cancelled" }],
            },
            location: fields({
              file: { scalar: "string" },
              line: { scalar: "number" },
            }),
          },
          ["location"],
        ),
        complete: { literal: true },
      },
      ["scan", "failure"],
    ),
    first: ["protocol", "build"],
  };
}

function invalid(): never {
  throw new ScanError("invalid remote metadata protocol");
}

function branch(schema: Schema, token: TokenType, value: unknown): Schema {
  if (schema.union) {
    for (const option of schema.union) {
      try {
        return branch(option, token, value);
      } catch {
        /* Try the next declared type. */
      }
    }
    return invalid();
  }
  if (token === TokenType.LEFT_BRACE && schema.properties) return schema;
  if (token === TokenType.LEFT_BRACKET && schema.array) return schema;
  if (
    Object.hasOwn(schema, "literal") &&
    value === schema.literal &&
    token !== TokenType.LEFT_BRACE &&
    token !== TokenType.LEFT_BRACKET
  )
    return schema;
  if (schema.scalar === "string" && token === TokenType.STRING) return schema;
  if (
    schema.scalar === "number" &&
    token === TokenType.NUMBER &&
    typeof value === "number" &&
    Number.isFinite(value)
  )
    return schema;
  return invalid();
}

/** Decode incrementally into validated metadata, never buffering raw stdout or stderr. */
export async function decode<T>(
  input: AsyncIterable<Uint8Array>,
  schema: Schema,
): Promise<T> {
  const parser = new JSONParser();
  const stack: {
    schema: Schema;
    seen: Set<string>;
    key?: string;
    state: "key" | "value" | "after";
  }[] = [];
  let started = false;
  let result: unknown;
  parser.onToken = ({ token, value }) => {
    const frame = stack.at(-1);
    if (token === TokenType.COLON) return;
    if (token === TokenType.COMMA) {
      if (!frame || frame.state !== "after") invalid();
      frame.state = frame.schema.properties ? "key" : "value";
      return;
    }
    if (token === TokenType.RIGHT_BRACE || token === TokenType.RIGHT_BRACKET) {
      if (!frame) invalid();
      for (const key of Object.keys(frame.schema.properties ?? {}))
        if (!frame.seen.has(key) && !frame.schema.optional?.includes(key))
          invalid();
      stack.pop();
      return;
    }
    if (frame?.schema.properties && frame.state === "key") {
      if (
        token !== TokenType.STRING ||
        typeof value !== "string" ||
        ["__proto__", "constructor", "prototype"].includes(value) ||
        frame.seen.has(value)
      )
        invalid();
      if (
        !Object.hasOwn(frame.schema.properties, value) &&
        !frame.schema.values
      )
        invalid();
      const next = frame.schema.first?.[frame.seen.size];
      if (next !== undefined && value !== next) invalid();
      frame.key = value;
      frame.seen.add(value);
      frame.state = "value";
      return;
    }
    let expected: Schema;
    if (frame) {
      if (frame.state !== "value") invalid();
      expected =
        frame.schema.array ??
        frame.schema.properties?.[frame.key!] ??
        frame.schema.values!;
      frame.state = "after";
    } else {
      if (started) invalid();
      started = true;
      expected = schema;
    }
    const selected = branch(expected, token, value);
    if (token === TokenType.LEFT_BRACE || token === TokenType.LEFT_BRACKET)
      stack.push({
        schema: selected,
        seen: new Set(),
        state: selected.properties ? "key" : "value",
      });
  };
  parser.onValue = ({ value, stack }) => {
    if (!stack.length) result = value;
  };
  try {
    // The only retained tree is the normalized metadata needed by the analyzer.
    // Schema-directed depth rejects hostile nesting before allocating its tree.
    const utf8 = new TextDecoder("utf-8", { fatal: true });
    for await (const chunk of input)
      parser.write(utf8.decode(chunk, { stream: true }));
    const tail = utf8.decode();
    if (tail) parser.write(tail);
    if (!parser.isEnded) parser.end();
    if (!started || stack.length || result === undefined) invalid();
    return result as T;
  } catch {
    return invalid();
  }
}

function* encode(value: unknown): Generator<string> {
  if (value === null || typeof value !== "object") {
    yield JSON.stringify(value);
    return;
  }
  const array = Array.isArray(value);
  yield array ? "[" : "{";
  let first = true;
  for (const [key, child] of Object.entries(value)) {
    if (child === undefined) continue;
    if (!first) yield ",";
    first = false;
    if (!array) yield JSON.stringify(key) + ":";
    yield* encode(child);
  }
  yield array ? "]" : "}";
}

/** Write metadata with pipe backpressure without constructing a serialized scan copy. */
export async function transmit(
  output: Writable,
  value: unknown,
): Promise<void> {
  for (const token of encode(value))
    if (!output.write(token)) await once(output, "drain");
}
