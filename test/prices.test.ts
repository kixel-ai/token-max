import { test } from "node:test";
import assert from "node:assert/strict";
import {
  pairedSavings,
  pricePart,
  priceRequest,
  serverCost,
  unknownCost,
} from "../src/prices.js";
import { usage } from "../src/reader.js";
import { claudeUsage, makeRequest, prices } from "./helpers.js";

test("OpenAI cache reads/writes replace ordinary input and reasoning stays a subset", async () => {
  const catalog = await prices();
  const r = makeRequest({
    harness: "codex",
    provider: "openai",
    api: "responses",
  });
  const p = {
    ...r.parts[0]!,
    model: "gpt-6.1-sol",
    usage: usage(
      {
        input_tokens: 12000,
        output_tokens: 100,
        input_tokens_details: {
          cached_tokens: 4000,
          cache_write_tokens: 7000,
          cache_write_tokens_details: {
            ttl_30m_tokens: 3000,
            ttl_12h_tokens: 4000,
          },
        },
        output_tokens_details: { reasoning_tokens: 20 },
      },
      "codex",
    ),
  };
  const result = pricePart(catalog, r, p);
  assert.ok(
    Math.abs(
      result.totalUsd! -
        (1000 * 2 + 4000 * 0.1 + 3000 * 2.5 + 4000 * 5 + 100 * 10) / 1e6,
    ) < 1e-10,
  );
  const missing = pricePart(catalog, r, {
    ...p,
    usage: usage(
      { input_tokens: 12000, output_tokens: 100, cached_input_tokens: 4000 },
      "codex",
    ),
  });
  assert.equal(missing.totalUsd, null);
  assert.match(missing.unknown.join(" "), /cache writes missing/);
  assert.equal(pricePart(catalog, { ...r, api: "unknown" }, p).totalUsd, null);
});

test("earlier OpenAI models have no additional write bill and unknown internal models are never substituted", async () => {
  const c = await prices();
  const r = makeRequest({
    harness: "codex",
    provider: "openai",
    api: "responses",
  });
  const p = {
    ...r.parts[0]!,
    model: "gpt-5.4",
    usage: usage(
      { input_tokens: 1000, output_tokens: 100, cached_input_tokens: 200 },
      "codex",
    ),
  };
  assert.equal(
    pricePart(c, r, p).totalUsd,
    (800 * 2.5 + 200 * 0.25 + 100 * 15) / 1e6,
  );
  assert.equal(
    pricePart(c, r, { ...p, model: "codex-auto-review" }).totalUsd,
    null,
  );
});

test("documented context thresholds reprice the full request, not just removed tokens", async () => {
  const c = await prices();
  const r = makeRequest({
    harness: "codex",
    provider: "openai",
    api: "responses",
  });
  const p = {
    ...r.parts[0]!,
    model: "gpt-6.1-sol",
    usage: usage(
      {
        input_tokens: 250000,
        output_tokens: 100,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
      },
      "codex",
    ),
  };
  assert.equal(pricePart(c, r, p).totalUsd, 0.501);
  assert.equal(
    pricePart(c, r, { ...p, usage: { ...p.usage, input: 272000 } }).totalUsd,
    0.545,
  );
  assert.equal(
    pricePart(c, r, { ...p, usage: { ...p.usage, input: 272001 } }).totalUsd,
    1.089504,
  );
  const h = makeRequest();
  h.parts[0]!.model = "claude-haiku-5-5";
  h.parts[0]!.usage = claudeUsage({ input: 100001, write: 0, output: 0 });
  assert.equal(pricePart(c, h, h.parts[0]!).totalUsd, 0.0500005);
});

test("Claude TTL, region, speed, server fees and missing prices remain explicit", async () => {
  const c = await prices();
  const r = makeRequest();
  const p = {
    ...r.parts[0]!,
    usage: claudeUsage({ write: 1000, read: 200, output: 100, ttl: "1h" }),
    geo: "us",
    speed: "fast",
  };
  const cost = pricePart(c, r, p);
  assert.ok(
    Math.abs(
      cost.totalUsd! - ((1000 * 8 + 200 * 0.2 + 100 * 20) * 2 * 1.1) / 1e6,
    ) < 1e-10,
  );
  assert.equal(pricePart(c, r, { ...p, tier: "priority" }).totalUsd, null);
  assert.equal(pricePart(c, { ...r, provider: "custom" }, p).totalUsd, null);
  assert.equal(
    serverCost({
      ...r,
      serverTools: { web_search_requests: 2, web_fetch_requests: 3 },
    }).totalUsd,
    0.02,
  );
  assert.equal(
    serverCost({ ...r, serverTools: { code_execution_requests: 1 } }).totalUsd,
    null,
  );
  assert.equal(
    priceRequest(c, { ...r, parts: [p], serverTools: null }).totalUsd,
    null,
  );
});

test("unavailable Claude geography uses the labelled global API equivalent", async () => {
  const r = makeRequest();
  const c = await prices();
  const p = { ...r.parts[0]!, geo: "not_available" };
  const result = pricePart(c, r, p);
  assert.equal(result.totalUsd, pricePart(c, r, { ...p, geo: null }).totalUsd);
  assert.match(
    result.assumptions.join(" "),
    /unrecorded geography assumed global/,
  );
  assert.equal(
    pricePart(c, r, { ...p, geo: "unexpected_region" }).totalUsd,
    null,
  );
});

test("paired savings decide only when unpriced parts cannot flip the sign", () => {
  const known = (usd: number) => ({
    knownUsd: usd,
    totalUsd: usd,
    unknown: [],
    assumptions: [],
  });
  const missing = unknownCost("missing");
  const pair = (baseline: number, alternative: number) => ({
    baseline: known(baseline),
    alternative: known(alternative),
  });
  const gap = { baseline: missing, alternative: missing };
  assert.deepEqual(pairedSavings([pair(3, 1), pair(2, 1)]), {
    usd: 3,
    priced: 2,
    total: 2,
    decided: true,
  });
  // Each unpriced part may differ by up to the largest priced difference (2).
  assert.equal(pairedSavings([pair(3, 1), pair(2, 1), gap]).decided, true);
  assert.equal(
    pairedSavings([pair(3, 1), pair(2, 1), gap, gap]).decided,
    false,
  );
  assert.equal(pairedSavings([gap]).decided, false);
  assert.equal(pairedSavings([]).decided, true);
});
