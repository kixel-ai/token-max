import assert from "node:assert/strict";
import { test } from "node:test";
import { CacheChain } from "../src/cache.js";
import type { Request, StructuralEvent, Ttl } from "../src/model.js";
import { claudeUsage, makeRequest } from "./helpers.js";

function feed(
  chain: CacheChain,
  r: Request,
  ttl: Ttl,
  selected = false,
  cold = false,
) {
  return chain.replay({
    request: r,
    p: r.parts[0]!,
    part: 0,
    estimated: false,
    ttl,
    cold,
    selected,
  });
}

test("compressed ambiguous histories agree with every possible last touch at lifetime boundaries", () => {
  // The oracle enumerates possible last touches, including the original anchor.
  // Dense histories exercise range-tree growth, expiration and prefix extrema.
  const anchor = makeRequest({ id: "anchor", time: 12 * 60000 });
  const attempts = Array.from({ length: 150 }, (_, index) =>
    makeRequest({
      id: `possible-${index}`,
      time: index * 30000,
      endTime: index * 30000,
      parts: [
        {
          ...anchor.parts[0]!,
          speed: null,
          usage: claudeUsage({ write: 1 + ((index * 239) % 1600) }),
        },
      ],
    }),
  );
  for (const ttl of ["5m", "1h"] as const)
    for (const observed of ["5m", "1h", null] as const)
      for (const cold of [false, true]) {
        const final = makeRequest({
          id: "final",
          time: 75 * 60000,
          endTime: 75 * 60000,
          parts: [
            {
              ...anchor.parts[0]!,
              usage: claudeUsage({
                read: 300,
                write: 700,
                ttl: observed ?? "5m",
              }),
            },
          ],
        });
        if (observed === null) {
          final.parts[0]!.usage.write5m = 350;
          final.parts[0]!.usage.write1h = 350;
        }
        const possible = [
          anchor,
          ...attempts.filter((r) => r.time >= anchor.time),
        ];
        const geometries = new Set(
          possible.map((last) => {
            const chain = new CacheChain([]);
            feed(chain, anchor, ttl);
            if (last !== anchor)
              feed(
                chain,
                {
                  ...last,
                  parts: [{ ...last.parts[0]!, speed: "standard" }],
                },
                ttl,
              );
            const result = feed(chain, final, ttl, true, cold)!;
            assert.equal(result.unknown, false);
            return JSON.stringify([result.p.usage.read, result.p.usage.write]);
          }),
        );
        const chain = new CacheChain([]);
        for (const request of [...attempts, anchor].sort(
          (a, b) => a.time - b.time,
        ))
          feed(chain, request, ttl);
        const result = feed(chain, final, ttl, true, cold)!;
        assert.equal(
          result.unknown,
          geometries.size > 1,
          `${ttl}/${observed}/${cold}`,
        );
        if (!result.unknown)
          assert.ok(
            geometries.has(
              JSON.stringify([result.p.usage.read, result.p.usage.write]),
            ),
          );
        assert.ok(result.evidence!.predecessors.length < attempts.length);
      }
});

test("zero-cache requests do not renew an older prefix or clear unknown state", () => {
  const first = makeRequest({ id: "first" });
  const empty = makeRequest({
    id: "empty",
    time: 17 * 60000,
    parts: [{ ...first.parts[0]!, usage: claudeUsage({ write: 0 }) }],
  });
  const final = makeRequest({
    id: "final",
    time: 20 * 60000,
    parts: [
      {
        ...first.parts[0]!,
        usage: claudeUsage({ read: 1000, write: 0, ttl: "1h" }),
      },
    ],
  });
  const known = new CacheChain([]);
  for (const r of [first, empty]) feed(known, r, "5m");
  assert.equal(feed(known, final, "5m", true)!.p.usage.read, 0);
  const unknown = new CacheChain([]);
  const missing = makeRequest({
    id: "missing",
    time: 16 * 60000,
    parts: [
      {
        ...first.parts[0]!,
        usage: { ...first.parts[0]!.usage, read: null, write: null },
      },
    ],
  });
  for (const r of [first, missing, empty]) feed(unknown, r, "5m");
  const result = feed(unknown, final, "5m", true)!;
  assert.equal(result.unknown, true);
  assert.ok(
    result.evidence!.predecessors.some((p) => p.request === missing.id),
  );
});

test("copied compaction boundaries reset unknown cache evidence in every origin session", () => {
  const first = makeRequest();
  const missing = makeRequest({
    id: "missing",
    time: 17 * 60000,
    parts: [
      {
        ...first.parts[0]!,
        speed: null,
        usage: { ...first.parts[0]!.usage, read: null, write: null },
      },
    ],
  });
  const final = makeRequest({
    id: "final",
    time: 20 * 60000,
    parts: [
      {
        ...first.parts[0]!,
        usage: claudeUsage({ read: 1000, write: 200, ttl: "1h" }),
      },
    ],
  });
  const boundary: StructuralEvent = {
    id: "copied-boundary",
    harness: "claude",
    kind: "compaction",
    session: "original",
    origins: ["original", "copy"].map((session) => ({
      session,
      location: { file: "synthetic", line: 1 },
    })),
    location: { file: "synthetic", line: 1 },
    time: 18 * 60000,
    request: null,
    preTokens: null,
    postTokens: null,
    durationMs: null,
    precomputed: null,
    trigger: null,
    version: "2.1.314",
    client: "sdk-ts",
    cwd: null,
    profiles: ["profile"],
    pricing: null,
    model: null,
    tool: null,
    toolId: null,
    settings: {},
    idleAttribution: "unknown",
  };
  for (const session of ["original", "copy"]) {
    const chain = new CacheChain([boundary]);
    for (const r of [first, missing]) feed(chain, { ...r, session }, "5m");
    const result = feed(chain, { ...final, session }, "5m", true)!;
    assert.equal(result.unknown, false);
    assert.equal(result.p.usage.read, 1000);
    assert.equal(result.evidence, null);
  }
});
