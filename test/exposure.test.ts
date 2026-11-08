import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { scan } from "../src/reader.js";
import { analyze } from "../src/replay.js";
import { calibrate, measureSize } from "../src/exposure.js";
import { controlText } from "../src/context.js";
import { formatReport } from "../src/report.js";
import { id } from "../src/safe.js";
import { prices, workspace } from "./helpers.js";

const at = (second: number) =>
  new Date(Date.UTC(2026, 10, 1, 0, 0, second)).toISOString();
const codexMeta = {
  type: "session_meta",
  timestamp: at(0),
  payload: {
    id: "session",
    model_provider: "openai",
    cli_version: "0.167.0",
    originator: "codex_cli_rs",
    source: "cli",
  },
};
const turn = {
  type: "turn_context",
  timestamp: at(0),
  payload: { model: "gpt-6.1-sol", turn_id: "turn", service_tier: "standard" },
};
function billed(second: number) {
  return {
    type: "token_usage_record",
    timestamp: at(second),
    payload: {
      response_id: `response-${second}`,
      turn_id: "turn",
      usage: {
        input_tokens: 1000,
        output_tokens: 10,
        cached_input_tokens: 500,
        cache_write_input_tokens: 0,
      },
    },
  };
}
const world = (second: number, state: unknown, full = false) => ({
  type: "world_state",
  timestamp: at(second),
  payload: { full, state },
});
const attachment = (second: number, value: unknown) => ({
  type: "attachment",
  timestamp: at(second),
  sessionId: "session",
  uuid: `attachment-${second}`,
  version: "2.1.314",
  attachment: value,
});
function assistant(second: number, content: unknown[] = []) {
  return {
    type: "assistant",
    timestamp: at(second),
    sessionId: "session",
    version: "2.1.314",
    entrypoint: "sdk-ts",
    requestRef: `api-${second}`,
    message: {
      id: `message-${second}`,
      model: "claude-opus-5-5",
      content,
      usage: {
        input_tokens: 1000,
        output_tokens: 10,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        service_tier: "standard",
        speed: "standard",
        inference_geo: "global",
      },
    },
  };
}
async function transcript(
  harness: "claude" | "codex",
  records: unknown[],
  since?: number,
) {
  const w = await workspace();
  try {
    const file =
      harness === "codex"
        ? join(w.codex, "sessions/main.jsonl")
        : join(w.claude, "projects/project/main.jsonl");
    await writeFile(
      file,
      records.map((v) => JSON.stringify(v)).join("\n") + "\n",
    );
    return await scan({
      ...w.options,
      ...(since === undefined ? {} : { since }),
    });
  } finally {
    await w.dispose();
  }
}

test("world state deltas preserve opaque identities, removals, inheritance and compaction uncertainty", async () => {
  const data = await transcript(
    "codex",
    [
      codexMeta,
      turn,
      world(
        0,
        {
          host_skills: { body: "SYNTHETIC_PRIVATE_LISTING" },
          tools: { mcp__example: "SYNTHETIC_PRIVATE_DESCRIPTION" },
          top_level_tools: {
            functions: "0123456789012345678901234567890123456789",
          },
        },
        true,
      ),
      billed(1),
      world(2, {
        tools: { mcp__example: null },
        host_skills: { body: "SYNTHETIC_PRIVATE_EXPANDED_LISTING" },
      }),
      billed(3),
      { type: "compacted", timestamp: at(4), payload: {} },
      billed(5),
    ],
    Date.parse(at(1)),
  );
  const e = data.exposure;
  assert.equal(e.requests.length, 3);
  assert.equal(e.requests[0]!.inherited.length, 3);
  assert.equal(e.requests[1]!.removed.length, 2);
  assert.equal(e.requests[1]!.candidates.length, 2);
  assert.equal(e.requests[2]!.coverage, "unknown");
  const hash = e.components.find(
    (c) => c.provenance === "opaque-schema-identity",
  )!;
  assert.equal(hash.characters, null);
  assert.equal(hash.recordedTokens, null);
  assert.equal(
    e.components.find((c) => c.category === "skill-listing")!.owner,
    "skills",
  );
  assert.ok(
    e.visibility.every(
      (v) => !v.automaticUnused && v.nestedCalls === "unknown",
    ),
  );
  assert.doesNotMatch(JSON.stringify(e), /SYNTHETIC_PRIVATE|0123456789/);
});

test("Claude attachment deltas track deferred definitions and whole listing replacement without pricing candidates", async () => {
  const data = await transcript("claude", [
    attachment(0, {
      type: "skill_listing",
      content: "SYNTHETIC_PRIVATE_FIRST",
      names: ["one"],
      isInitial: true,
    }),
    attachment(1, {
      type: "instructions",
      files: [
        { path: "/synthetic/CLAUDE.md", content: "SYNTHETIC_PRIVATE_RULE" },
      ],
    }),
    attachment(2, {
      type: "deferred_tools_record",
      entries: [
        {
          name: "mcp__example__search",
          description: "SYNTHETIC_PRIVATE_SCHEMA",
          defer_loading: true,
          input_schema: { type: "object" },
        },
      ],
    }),
    attachment(3, {
      type: "deferred_tools_delta",
      addedNames: ["mcp__example__search"],
      addedLines: ["SYNTHETIC_PRIVATE_SHORT"],
      removedNames: [],
    }),
    {
      type: "api-request",
      timestamp: at(4),
      sessionId: "session",
      id: "api-5",
      requestDigest: "digest-one",
      keep: 0,
      tail: ["message-digest"],
      params: { model: "claude-opus-5-5" },
    },
    assistant(5),
    attachment(6, {
      type: "skill_listing",
      content: "SYNTHETIC_PRIVATE_BACKFILL_LONGER",
      names: ["two", "three"],
    }),
    attachment(7, {
      type: "deferred_tools_delta",
      addedNames: [],
      addedLines: [],
      removedNames: ["mcp__example__search"],
      surfacedDefinitions: [
        {
          name: "mcp__example__search",
          definition: {
            name: "mcp__example__search",
            description: "SYNTHETIC_PRIVATE_EXPANDED",
            input_schema: { type: "object" },
          },
        },
      ],
    }),
    {
      type: "api-request",
      timestamp: at(8),
      sessionId: "session",
      id: "api-9",
      base: "digest-one",
      keep: 1,
      tail: ["next-digest"],
      requestDigest: "digest-two",
      params: { model: "claude-opus-5-5" },
    },
    assistant(9),
  ]);
  const e = data.exposure;
  assert.equal(e.requests[0]!.deferred.length, 1);
  assert.equal(e.requests[0]!.components.length, 0);
  assert.equal(e.requests[1]!.references!.items!.length, 2);
  assert.equal(e.requests[1]!.removed.length, 2);
  const report = analyze({ scan: data, catalog: await prices() });
  assert.equal(report.exposure.calibration.length, 0);
  // Digest-only requests prove no exact shape, so nothing is priced per piece.
  assert.equal(report.context.length, 0);
  assert.ok(e.components.every((c) => c.current === "unresolved"));
  assert.doesNotMatch(JSON.stringify(report), /SYNTHETIC_PRIVATE/);
});

test("text-only wire requests calibrate matching cohorts and preserve unknown net effects", async () => {
  const data = await transcript("claude", [
    {
      type: "api-request",
      id: "api-1",
      timestamp: at(0),
      sessionId: "session",
      params: {
        model: "claude-opus-5-5",
        system: "SYNTHETIC_PRIVATE_SYSTEM",
        messages: [{ role: "user", content: "SYNTHETIC_PRIVATE_PROMPT" }],
        tools: [
          {
            name: "mcp__example__search",
            description: "SYNTHETIC_PRIVATE_TOOL",
            input_schema: { type: "object" },
          },
        ],
      },
    },
    assistant(1),
  ]);
  const report = analyze({ scan: data, catalog: await prices() });
  const calibration = report.exposure.calibration[0]!;
  assert.equal(calibration.samples.length, 1);
  assert.equal(calibration.samples[0]!.tokens, 1000);
  assert.equal(measureSize({ text: "🌲木abc" }).characters, 5);
  // Full requests list tools before the system prompt, matching the cache prefix.
  const order = data.exposure.requests[0]!.components.map(
    (id) => data.exposure.components.find((c) => c.id === id)!.category,
  );
  assert.deepEqual(order, ["mcp-schema", "other"]);
  const server = report.context.find((c) => c.kind === "mcp-server")!;
  assert.equal(server.name, "example");
  assert.equal(server.priced, 1);
  assert.ok(server.costUsd > 0);
  assert.equal(
    calibrate(
      data.exposure,
      data.requests.map((r) => ({ ...r, parts: [...r.parts, ...r.parts] })),
    ).length,
    0,
  );
  assert.doesNotMatch(JSON.stringify(report), /SYNTHETIC_PRIVATE/);
});

test("image inputs and unlinked requests never calibrate from partial text", async () => {
  const data = await transcript("claude", [
    {
      type: "api-request",
      id: "api-1",
      timestamp: at(0),
      sessionId: "session",
      params: {
        model: "claude-opus-5-5",
        system: "synthetic",
        messages: [
          {
            role: "user",
            content: [
              {
                type: "image",
                source: { type: "base64", data: "SYNTHETIC_PRIVATE_IMAGE" },
              },
            ],
          },
        ],
        tools: [
          {
            name: "Artifact",
            description: "a",
            input_schema: { type: "object" },
          },
        ],
      },
    },
    assistant(1),
    assistant(2),
  ]);
  assert.equal(calibrate(data.exposure, data.requests).length, 0);
  assert.notEqual(data.exposure.requests[1]!.linkage, "wire");
  assert.doesNotMatch(JSON.stringify(data), /SYNTHETIC_PRIVATE/);
  // Without a calibration nothing is priced, and nothing is shown as $0.
  const report = analyze({ scan: data, catalog: await prices() });
  assert.ok(report.context.every((c) => c.estimatedUsd === null));
  const text = formatReport(report, { width: 200 });
  assert.match(text, /Artifact tool( \[[^\]]*\])?: on\. Cost unknown/);
  assert.match(text, /and the skill and subagent lists: cost unknown\./);
});

test("typed/mentioned skills, actual nested completions and script references stay distinct", async () => {
  const data = await transcript("codex", [
    codexMeta,
    turn,
    {
      type: "event_msg",
      timestamp: at(1),
      payload: {
        type: "item_completed",
        item: {
          type: "UserMessage",
          id: "user-one",
          content: [
            {
              type: "skill",
              name: "synthetic-skill",
              path: "/synthetic/SKILL.md",
            },
            {
              type: "Text",
              text: "Use $synthetic-skill SYNTHETIC_PRIVATE_PROMPT",
            },
          ],
        },
      },
    },
    {
      type: "response_item",
      timestamp: at(2),
      payload: {
        type: "function_call",
        id: "call-one",
        call_id: "call-one",
        name: "exec",
        arguments:
          "await mcp__example__nested({secret: 'SYNTHETIC_PRIVATE_ARGS'})",
      },
    },
    {
      type: "event_msg",
      timestamp: at(3),
      payload: {
        type: "item_completed",
        item: {
          type: "McpToolCall",
          id: "nested-one",
          server: "example",
          tool: "nested",
          status: "completed",
          result: {
            content: [{ type: "text", text: "SYNTHETIC_PRIVATE_RESULT" }],
          },
        },
      },
    },
    billed(4),
  ]);
  const invocations = data.exposure.invocations;
  assert.equal(invocations.filter((i) => i.kind === "skill-request").length, 2);
  assert.equal(
    invocations.find((i) => i.kind === "mcp-call")!.status,
    "completed",
  );
  assert.equal(
    invocations.find((i) => i.kind === "script-reference")!.status,
    "reference",
  );
  assert.ok(
    data.exposure.visibility.every(
      (v) =>
        v.nestedCalls === "partial" &&
        v.typedSkills === "observed" &&
        !v.automaticUnused,
    ),
  );
  assert.doesNotMatch(JSON.stringify(data), /SYNTHETIC_PRIVATE/);
});

test("Skill and file loads retain derived bodies and completion status without call arguments", async () => {
  const data = await transcript("claude", [
    assistant(1, [
      {
        type: "tool_use",
        id: "skill-call",
        name: "Skill",
        input: { skill: "example:skill", args: "SYNTHETIC_PRIVATE_ARGS" },
      },
      {
        type: "tool_use",
        id: "read-call",
        name: "Read",
        input: { file_path: "/synthetic/SKILL.md" },
      },
    ]),
    {
      type: "user",
      timestamp: at(2),
      sessionId: "session",
      message: {
        content: [
          {
            type: "tool_result",
            tool_use_id: "skill-call",
            content: "SYNTHETIC_PRIVATE_BODY",
          },
          {
            type: "tool_result",
            tool_use_id: "read-call",
            content: "SYNTHETIC_PRIVATE_FILE",
          },
        ],
      },
    },
    assistant(3),
  ]);
  assert.equal(data.exposure.invocations.length, 2);
  assert.ok(data.exposure.invocations.every((i) => i.status === "completed"));
  assert.equal(
    data.exposure.components.filter((c) => c.category === "skill-body").length,
    2,
  );
  assert.doesNotMatch(JSON.stringify(data), /SYNTHETIC_PRIVATE|example:skill/);
});

test("copied history produces one request exposure; conflicting copies remain unknown", async () => {
  const w = await workspace();
  try {
    const records = [
      codexMeta,
      turn,
      world(0, { host_skills: { body: "synthetic listing" } }, true),
      billed(1),
    ];
    const encoded = records.map((r) => JSON.stringify(r)).join("\n") + "\n";
    await writeFile(join(w.codex, "sessions/a.jsonl"), encoded);
    await writeFile(join(w.codex, "sessions/b.jsonl"), encoded);
    let data = await scan(w.options);
    assert.equal(data.exposure.requests.length, 1);
    assert.equal(data.exposure.requests[0]!.candidates.length, 1);
    records[2] = world(0, { host_skills: { body: "different listing" } }, true);
    await writeFile(
      join(w.codex, "sessions/b.jsonl"),
      records.map((r) => JSON.stringify(r)).join("\n") + "\n",
    );
    data = await scan(w.options);
    assert.equal(data.exposure.requests[0]!.linkage, "unknown");
    assert.match(data.exposure.requests[0]!.reasons.join(), /disagree/);
    assert.equal(data.requests[0]!.id, `codex:${id("response-1")}`);
  } finally {
    await w.dispose();
  }
});

test("malformed selected context and digest chains fail atomically with safe diagnostics", async () => {
  await assert.rejects(
    transcript("codex", [codexMeta, world(0, { tools: [] }, true), billed(1)]),
    /invalid context object/,
  );
  await assert.rejects(
    transcript("claude", [
      attachment(0, {
        type: "skill_listing",
        content: { private: "SYNTHETIC_PRIVATE" },
      }),
      assistant(1),
    ]),
    /invalid context text/,
  );
  await assert.rejects(
    transcript("claude", [
      {
        type: "api-request",
        id: "one",
        timestamp: at(0),
        sessionId: "session",
        requestDigest: "digest",
        keep: 0,
        tail: [],
        params: {},
      },
      {
        type: "api-request",
        id: "two",
        timestamp: at(1),
        sessionId: "session",
        base: "digest",
        keep: 1,
        tail: [],
        params: {},
      },
    ]),
    /invalid request digest prefix/,
  );
});

test("recorded request shapes price each piece at its cache position and count its uses", async () => {
  const tool = (name: string, size: number) => ({
    name,
    description: "x".repeat(size),
    input_schema: { type: "object" },
  });
  const tools = [
    tool("Artifact", 4000),
    tool("Bash", 1000),
    tool("mcp__docs__read", 500),
  ];
  const system = [{ type: "text", text: "s".repeat(2000) }];
  const message = { role: "user", content: "SYNTHETIC_PRIVATE_PROMPT" };
  const call = (id: string, name: string) => ({
    type: "tool_use",
    id,
    name,
    input: {},
  });
  const used = assistant(2, [call("t1", "mcp__docs__read")]);
  const reply = assistant(1, [call("t2", "Bash")]);
  reply.message.usage.input_tokens = 100;
  reply.message.usage.cache_read_input_tokens = 2000;
  reply.message.usage.cache_creation_input_tokens = 900;
  Object.assign(reply.message.usage, {
    cache_creation: {
      ephemeral_5m_input_tokens: 0,
      ephemeral_1h_input_tokens: 900,
    },
  });
  const data = await transcript("claude", [
    attachment(0, {
      type: "skill_listing",
      content: "- review: SYNTHETIC_PRIVATE_DESCRIPTION",
      names: ["review"],
      isInitial: true,
    }),
    // Seen first as a surfaced definition, then sent in the request's tools: it
    // is priced where the request puts it, not where it first appeared.
    attachment(0, {
      type: "deferred_tools_delta",
      addedNames: [],
      addedLines: [],
      removedNames: [],
      surfacedDefinitions: [{ name: tools[2]!.name, definition: tools[2] }],
    }),
    {
      type: "api-request-shape",
      sessionId: "session",
      timestamp: at(0),
      shapeHash: "shape",
      shape: { system, tools },
    },
    { type: "api-request-blob", sessionId: "session", hash: "m1", message },
    {
      type: "api-request",
      id: "api-1",
      timestamp: at(0),
      sessionId: "session",
      shapeHash: "shape",
      keep: 0,
      tail: ["m1"],
      requestDigest: "d1",
      params: { model: "claude-opus-5-5" },
    },
    reply,
    // A repeated snapshot of the same call is one use.
    reply,
    used,
  ]);
  const report = analyze({ scan: data, catalog: await prices() });
  const listing = data.exposure.components.find(
    (c) => c.category === "skill-listing" && c.scope === "aggregate",
  )!;
  const characters =
    JSON.stringify(system).length +
    tools.reduce((n, t) => n + JSON.stringify(t).length, 0) +
    JSON.stringify(message).length;
  const ratio = 3000 / characters;
  assert.ok(
    Math.abs(report.exposure.calibration[0]!.tokensPerCharacter - ratio) <
      1e-12,
  );
  const row = (name: string) => report.context.find((c) => c.name === name)!;
  const artifact = row("Artifact");
  assert.equal(controlText(artifact.control!), `"enableArtifact": false`);
  assert.equal(artifact.calls, 0);
  assert.ok(artifact.current);
  // Tools lead the prefix, so Artifact sits entirely inside the 2,000 cached-read tokens.
  const artifactTokens = JSON.stringify(tools[0]).length * ratio;
  assert.ok(artifactTokens < 2000);
  assert.ok(Math.abs(artifact.costUsd - (artifactTokens * 0.2) / 1e6) < 1e-12);
  assert.equal(row("Bash").control, null);
  assert.equal(row("Bash").calls, 1);
  assert.equal(row("docs").calls, 1);
  assert.equal(row("docs").kind, "mcp-server");
  assert.equal(row("docs").priced, 1);
  assert.equal(data.exposure.requests[0]!.prefix, tools.length + 1);
  const review = row("review");
  assert.equal(review.kind, "skill");
  assert.equal(
    controlText(review.control!),
    `"skillOverrides": {"review": "user-invocable-only"}`,
  );
  assert.ok(listing.characters! > 0);
  // No earlier request bounds where the listing sits among these cached reads,
  // so it stays unpriced rather than guessed.
  assert.equal(review.priced, 0);
  assert.equal(review.estimatedUsd, null);
  assert.equal(row("Claude Code system prompt").control, null);
  assert.doesNotMatch(JSON.stringify(report), /SYNTHETIC_PRIVATE/);
  // Every priced piece here costs well under the default $1 per use; an
  // unpriced one gets no verdict.
  assert.ok(
    report.context.every(
      (c) =>
        c.verdict === (c.control && c.estimatedUsd !== null ? "keep" : null),
    ),
  );

  // A piece is recommended off when its cost per use, counting no use as one,
  // exceeds the limit. Typed skill requests would not count as uses.
  const strict = analyze({
    scan: data,
    catalog: await prices(),
    maxCostPerUseUsd: artifact.costUsd / 2,
  });
  const verdict = (name: string) =>
    strict.context.find((c) => c.name === name)!.verdict;
  assert.equal(verdict("Artifact"), "off");
  assert.equal(verdict("docs"), "off");
  assert.equal(verdict("review"), null);
  assert.equal(verdict("Bash"), null);
  const text = formatReport(strict, { width: 200 });
  assert.match(
    text,
    /Artifact tool( \[[^\]]*\])?: on -> off\. Costs .*; never used\./,
  );
  assert.match(text, /Skill review( \[[^\]]*\])?: on\. Cost unknown/);
  const snippet = text.slice(text.indexOf("To turn these off"));
  assert.match(snippet, /"enableArtifact": false/);
  assert.match(snippet, /"deny": \[\s*"mcp__docs"\s*\]/);
  assert.doesNotMatch(snippet, /skillOverrides/);
});

test("attachments take their latest possible cache position; media and copied calls do not skew counts", async () => {
  const tools = [
    {
      name: "Artifact",
      description: "a".repeat(2000),
      input_schema: { type: "object" },
    },
  ];
  const system = [{ type: "text", text: "s".repeat(1000) }];
  // Tool input named data is billed text; only thinking signatures are opaque.
  const first = {
    role: "assistant",
    content: [
      { type: "thinking", thinking: "t", signature: "z".repeat(5000) },
      {
        type: "tool_use",
        id: "c1",
        name: "Artifact",
        // Tool input can hold fields named like opaque ones; they are billed text.
        input: {
          type: "thinking",
          signature: "q".repeat(100),
          data: "d".repeat(4000),
        },
      },
    ],
  };
  const image = {
    role: "user",
    content: [
      {
        type: "image",
        source: { type: "base64", media_type: "image/png", data: "AAAA" },
      },
    ],
  };
  const usage = (input: number, read: number, write: number) => ({
    input_tokens: input,
    cache_read_input_tokens: read,
    cache_creation_input_tokens: write,
    cache_creation: {
      ephemeral_5m_input_tokens: 0,
      ephemeral_1h_input_tokens: write,
    },
  });
  const request = (n: number, tail: string[]) => ({
    type: "api-request",
    id: `api-${n}`,
    timestamp: at(n),
    sessionId: "session",
    shapeHash: "shape",
    keep: 0,
    tail,
    requestDigest: `d${n}`,
    params: { model: "claude-opus-5-5" },
  });
  const reply = (n: number, u: object) => {
    const a = assistant(n, [
      { type: "tool_use", id: "c1", name: "Artifact", input: {} },
    ]);
    Object.assign(a.message.usage, u);
    return a;
  };
  const records = [
    attachment(0, {
      type: "skill_listing",
      content: "- review: " + "r".repeat(300),
      names: ["review"],
      isInitial: true,
    }),
    {
      type: "api-request-shape",
      sessionId: "session",
      timestamp: at(0),
      shapeHash: "shape",
      shape: { system, tools },
    },
    {
      type: "api-request-blob",
      sessionId: "session",
      hash: "m1",
      message: first,
    },
    request(1, ["m1"]),
    reply(1, usage(100, 0, 2900)),
    attachment(2, {
      type: "instructions",
      files: [{ path: "/repo/CLAUDE.md", content: "i".repeat(600) }],
    }),
    {
      type: "api-request-blob",
      sessionId: "session",
      hash: "m2",
      message: image,
    },
    request(3, ["m1", "m2"]),
    reply(3, usage(0, 3000, 500)),
    // A changed file at the same path is new content, not carried content.
    attachment(4, {
      type: "instructions",
      files: [{ path: "/repo/CLAUDE.md", content: "j".repeat(600) }],
    }),
    request(5, ["m1", "m2"]),
    reply(5, usage(0, 3500, 500)),
  ];
  const w = await workspace();
  try {
    for (const file of ["a", "b"])
      await writeFile(
        join(w.claude, `projects/project/${file}.jsonl`),
        records.map((r) => JSON.stringify(r)).join("\n") + "\n",
      );
    // A second profile holds a copy without the call, so the request belongs to
    // both profiles while the call was recorded in one.
    const other = join(w.home, ".claude-work/projects/project");
    await mkdir(other, { recursive: true });
    await writeFile(
      join(other, "c.jsonl"),
      records
        .map((r) =>
          JSON.stringify(
            r.type === "assistant"
              ? {
                  ...r,
                  message: {
                    ...(r as unknown as { message: object }).message,
                    content: [],
                  },
                }
              : r,
          ),
        )
        .join("\n") + "\n",
    );
    const data = await scan(w.options);
    const report = analyze({ scan: data, catalog: await prices() });
    const [calibration] = report.exposure.calibration;
    // Only the text-only request calibrates, with its tool input counted.
    assert.equal(calibration!.samples.length, 1);
    const { signature: _, ...thinking } = first.content[0]!;
    const expected =
      JSON.stringify(system).length +
      JSON.stringify(tools[0]).length +
      JSON.stringify({ ...first, content: [thinking, first.content[1]] })
        .length;
    assert.equal(calibration!.samples[0]!.characters, expected);
    const ratio = calibration!.tokensPerCharacter;
    const row = (name: string) => report.context.find((c) => c.name === name)!;
    // The same call copied into a second file counts once, and still counts for
    // a row shared with a profile whose copy lacks it.
    assert.equal(row("Artifact").profiles.length, 2);
    assert.equal(row("Artifact").calls, 1);
    // The listing came with the first request, so the second reads it from cache.
    const review = row("review");
    const instructions = row("/repo/CLAUDE.md");
    assert.equal(instructions.priced, 2);
    const tokens = 600 * ratio;
    assert.ok(tokens < 500, "the new file fits in the second request's writes");
    // The file is new in the second request, and its changed revision is new
    // in the third, so both pay the 1h write price rather than a cached read.
    assert.ok(
      Math.abs(instructions.costUsd / (tokens * (8 + 8) * 1e-6) - 1) < 0.001,
    );
    assert.ok(review.costUsd > 0 && review.priced === 3);
    assert.ok(review.estimatedUsd === review.costUsd);
  } finally {
    await w.dispose();
  }
});

test("provided schemas, listing member ownership and identified command file reads are derived once", async () => {
  const data = await transcript("codex", [
    codexMeta,
    turn,
    world(
      0,
      {
        host_skills: {
          body: "Available skills\n- example:skill: SYNTHETIC_PRIVATE_DESCRIPTION\n\nHow to use skills\nSYNTHETIC_PRIVATE_GUIDANCE",
        },
      },
      true,
    ),
    {
      type: "response_item",
      timestamp: at(1),
      payload: {
        type: "additional_tools",
        role: "developer",
        tools: [
          {
            type: "function",
            name: "mcp__example__search",
            description: "SYNTHETIC_PRIVATE_SCHEMA",
            parameters: { type: "object" },
          },
        ],
      },
    },
    {
      type: "response_item",
      timestamp: at(2),
      payload: {
        type: "function_call",
        namespace: "mcp__example",
        name: "search",
        call_id: "tool-call",
        id: "mcp-one",
        arguments: "{}",
      },
    },
    {
      type: "event_msg",
      timestamp: at(3),
      payload: {
        type: "item_completed",
        item: {
          type: "McpToolCall",
          id: "mcp-one",
          server: "example",
          tool: "search",
          status: "completed",
          result: { content: [] },
        },
      },
    },
    {
      type: "event_msg",
      timestamp: at(4),
      payload: {
        type: "item_completed",
        item: {
          type: "CommandExecution",
          id: "read-one",
          exit_code: 0,
          status: "completed",
          parsed_cmd: [
            {
              type: "read",
              name: "SKILL.md",
              path: "/synthetic/skill/SKILL.md",
              cmd: "cat /synthetic/skill/SKILL.md",
            },
          ],
          aggregated_output: "SYNTHETIC_PRIVATE_BODY",
        },
      },
    },
    billed(5),
  ]);
  const e = data.exposure;
  const aggregate = e.components.find((c) => c.scope === "aggregate")!;
  assert.equal(aggregate.members.length, 1);
  const member = e.components.find((c) => c.id === aggregate.members[0])!;
  assert.equal(
    member.characters,
    "- example:skill: SYNTHETIC_PRIVATE_DESCRIPTION".length,
  );
  assert.ok(!e.requests[0]!.candidates.includes(member.id));
  assert.equal(e.invocations.filter((i) => i.kind === "mcp-call").length, 1);
  assert.equal(
    e.invocations.find((i) => i.kind === "mcp-call")!.status,
    "completed",
  );
  assert.equal(
    e.invocations.find((i) => i.kind === "skill-load")!.status,
    "completed",
  );
  assert.ok(
    e.components.some(
      (c) =>
        c.provenance === "provided-additional-tools" && c.characters !== null,
    ),
  );
  assert.doesNotMatch(JSON.stringify(e), /SYNTHETIC_PRIVATE/);
});

test("invalid pre-window exposure records invalidate context without aborting selected billing", async () => {
  const data = await transcript(
    "claude",
    [
      attachment(0, {
        type: "skill_listing",
        content: "old listing",
        names: ["old"],
      }),
      {
        type: "api-request",
        timestamp: at(1),
        sessionId: "session",
        tail: ["digest"],
        params: {},
      },
      attachment(2, { type: "skill_listing", content: { invalid: true } }),
      {
        type: "api-request",
        id: "old",
        timestamp: at(3),
        sessionId: "session",
        tail: [{ invalid: true }],
        params: {},
      },
      assistant(5),
    ],
    Date.parse(at(4)),
  );
  assert.equal(data.requests.length, 1);
  assert.equal(data.exposure.requests[0]!.coverage, "unknown");
  assert.match(data.coverage.issues.join(), /invalid pre-window component/);
  assert.match(data.exposure.requests[0]!.reasons.join(), /invalid pre-window/);
});

test("cohort calibration outside measured request support leaves that comparison unknown", async () => {
  const wire = (second: number) => ({
    type: "api-request",
    id: `api-${second}`,
    timestamp: at(second - 1),
    sessionId: "session",
    params: {
      model: "claude-opus-5-5",
      system: "SYNTHETIC_PRIVATE_".repeat(100),
      messages: [{ role: "user", content: "synthetic" }],
      tools: [],
    },
  });
  const low = assistant(1);
  low.message.usage.input_tokens = 10;
  const high = assistant(3);
  high.message.usage.input_tokens = 10000;
  const data = await transcript("claude", [wire(1), low, wire(3), high]);
  const report = analyze({ scan: data, catalog: await prices() });
  // The pooled ratio puts the low request's pieces above its recorded input.
  const system = report.context.find((c) => c.kind === "system")!;
  assert.equal(system.requests, 2);
  assert.equal(system.priced, 1);
  // The unpriced request is scaled from the priced one, never counted as free.
  assert.equal(system.estimatedUsd, system.costUsd * 2);
  assert.doesNotMatch(JSON.stringify(report), /SYNTHETIC_PRIVATE/);
});

test("deferred definitions do not remove active revisions and repeated results remain in history", async () => {
  const userResult = (second: number, call: string) => ({
    type: "user",
    timestamp: at(second),
    sessionId: "session",
    message: {
      content: [
        {
          type: "tool_result",
          tool_use_id: call,
          content: `SYNTHETIC_PRIVATE_${second}`,
        },
      ],
    },
  });
  const data = await transcript("claude", [
    attachment(0, {
      type: "prompt_snapshot",
      systemPrompt: ["synthetic"],
      tools: [
        {
          name: "mcp__example__search",
          description: "active",
          schema: { type: "object" },
        },
      ],
    }),
    assistant(1, [
      { type: "tool_use", id: "one", name: "mcp__example__search", input: {} },
    ]),
    userResult(2, "one"),
    attachment(3, {
      type: "deferred_tools_record",
      entries: [
        {
          name: "mcp__example__search",
          defer_loading: true,
          description: "deferred",
          input_schema: { type: "object" },
        },
      ],
    }),
    assistant(4, [
      { type: "tool_use", id: "two", name: "mcp__example__search", input: {} },
    ]),
    userResult(5, "two"),
    assistant(6),
  ]);
  const e = data.exposure;
  const active = e.components.find(
    (c) => c.provenance === "prompt-snapshot-tool",
  )!;
  assert.ok(e.requests[1]!.candidates.includes(active.id));
  assert.ok(!e.requests[1]!.removed.includes(active.id));
  const results = e.components.filter((c) => c.category === "mcp-result");
  assert.equal(results.length, 2);
  assert.ok(
    results.every(
      (c) =>
        e.requests[2]!.candidates.includes(c.id) &&
        !e.requests[2]!.removed.includes(c.id),
    ),
  );
});

test("skill references need a known catalog and native user aliases keep repeated messages separate", async () => {
  const records = [
    codexMeta,
    turn,
    world(
      0,
      { host_skills: { body: "- known-skill: synthetic description" } },
      true,
    ),
  ];
  const content = "Use $known-skill, inspect $HOME, costs $20";
  const response = {
    type: "response_item",
    timestamp: at(1),
    payload: {
      type: "message",
      role: "user",
      content: [{ type: "input_text", text: content }],
    },
  };
  const event = {
    type: "event_msg",
    timestamp: at(1),
    payload: { type: "user_message", message: content },
  };
  const data = await transcript("codex", [
    ...records,
    response,
    event,
    response,
    event,
    billed(2),
  ]);
  const requests = data.exposure.invocations.filter(
    (i) => i.kind === "skill-request",
  );
  assert.equal(requests.length, 2);
  assert.ok(
    requests.every(
      (i) =>
        i.identityBasis === "content-occurrence" && i.locations.length === 2,
    ),
  );
  assert.equal(
    data.exposure.invocations.filter((i) => i.kind === "skill-reference")
      .length,
    4,
  );
  const claude = await transcript("claude", [
    attachment(0, {
      type: "skill_listing",
      names: ["known-skill"],
      content: "synthetic",
    }),
    {
      type: "user",
      timestamp: at(1),
      sessionId: "session",
      isMeta: true,
      message: { content: "$known-skill $ARGUMENTS" },
    },
    {
      type: "user",
      timestamp: at(2),
      sessionId: "session",
      message: { content: "<command-name>/compact</command-name>" },
    },
    assistant(3),
  ]);
  assert.equal(
    claude.exposure.invocations.filter((i) => i.kind === "skill-request")
      .length,
    0,
  );
});

test("copied MCP calls deduplicate across inherited session aliases", async () => {
  const w = await workspace();
  try {
    const records = [
      codexMeta,
      turn,
      {
        type: "event_msg",
        timestamp: at(1),
        payload: {
          type: "item_completed",
          item: {
            type: "McpToolCall",
            id: "shared-call",
            server: "example",
            tool: "search",
            status: "completed",
            result: { content: [] },
          },
        },
      },
      billed(2),
    ];
    await writeFile(
      join(w.codex, "sessions/a.jsonl"),
      records.map((r) => JSON.stringify(r)).join("\n") + "\n",
    );
    await writeFile(
      join(w.codex, "sessions/b.jsonl"),
      records
        .map((r, i) =>
          JSON.stringify(
            i === 0
              ? { ...codexMeta, payload: { ...codexMeta.payload, id: "fork" } }
              : r,
          ),
        )
        .join("\n") + "\n",
    );
    const data = await scan(w.options);
    assert.equal(data.exposure.invocations.length, 1);
    assert.equal(data.exposure.invocations[0]!.sessions.length, 2);
  } finally {
    await w.dispose();
  }
});

test("current Claude and Codex record shapes: request blobs, block-rendered attachments, sameAs definitions, null skill bodies", async () => {
  const call = {
    type: "tool_use",
    id: "tool-1",
    name: "mcp__example__search",
    input: {},
  };
  const definition = {
    name: "mcp__example__search",
    definition: { name: "mcp__example__search", input_schema: {} },
  };
  const data = await transcript("claude", [
    attachment(0, {
      type: "deferred_tools_delta",
      addedNames: [],
      addedLines: [],
      removedNames: [],
      surfacedDefinitions: [definition],
    }),
    attachment(1, {
      type: "deferred_tools_delta",
      addedNames: [],
      addedLines: [],
      removedNames: [],
      surfacedDefinitions: [
        { name: definition.name, sameAs: "attachment-0", listing: "" },
      ],
    }),
    {
      ...attachment(2, { type: "queued_command", prompt: [] }),
      rendered: [
        { content: [{ type: "text", text: "SYNTHETIC_PRIVATE_QUEUED" }] },
      ],
    },
    // Hash-addressed copy of a turn: no timestamp, never a second invocation.
    {
      type: "api-request-blob",
      hash: "blob",
      sessionId: "session",
      message: { role: "assistant", content: [call] },
    },
    assistant(3, [call]),
  ]);
  assert.equal(
    data.exposure.invocations.filter((i) => i.kind === "mcp-call").length,
    1,
  );
  assert.equal(
    data.exposure.components.filter((c) => c.category === "mcp-schema").length,
    1,
  );
  assert.ok(
    data.exposure.components.some(
      (c) => c.provenance === "unclassified-rendered-attachment",
    ),
  );
  await assert.rejects(
    transcript("claude", [
      attachment(0, {
        type: "deferred_tools_delta",
        addedNames: [],
        addedLines: [],
        removedNames: [],
        surfacedDefinitions: [
          { name: definition.name, sameAs: "attachment-9", listing: "" },
        ],
      }),
    ]),
    /unresolved surfaced definition reference/,
  );
  const codex = await transcript("codex", [
    codexMeta,
    turn,
    world(1, { host_skills: { body: null, version: 1 } }),
    billed(2),
  ]);
  assert.ok(
    codex.exposure.components.some(
      (c) => c.provenance === "world-state-metadata-only",
    ),
  );
});
