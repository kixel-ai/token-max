import assert from "node:assert/strict";
import { test } from "node:test";
import { writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { scan } from "../src/reader.js";
import { analyze } from "../src/replay.js";
import { prices, workspace } from "./helpers.js";

const common = {
  version: "2.1.314",
  entrypoint: "sdk-ts",
  sessionId: "synthetic",
  querySource: "sdk",
};
const time = (minute: number) => new Date(minute * 60000).toISOString();
function assistant(minute: number, id: string, read: number, write: number) {
  return {
    ...common,
    type: "assistant",
    timestamp: time(minute),
    message: {
      id,
      content: [{ type: "text", text: "SYNTHETIC_PRIVATE_WINDOW" }],
      model: "claude-opus-5-5",
      usage: {
        input_tokens: 0,
        output_tokens: 10,
        cache_read_input_tokens: read,
        cache_creation_input_tokens: write,
        cache_creation: {
          ephemeral_5m_input_tokens: 0,
          ephemeral_1h_input_tokens: write,
        },
        service_tier: "standard",
        speed: "standard",
        inference_geo: "global",
      },
    },
  };
}
const attempt = (minute: number) => ({
  ...common,
  type: "api-request",
  id: "attempt",
  timestamp: time(minute),
  params: { model: "claude-opus-5-5" },
});
async function profile(records: unknown[]) {
  const w = await workspace();
  await writeFile(
    join(w.claude, "settings.json"),
    JSON.stringify({ promptCacheTtl: "5m" }),
  );
  await writeFile(
    join(w.claude, "projects/project/main.jsonl"),
    records.map((r) => JSON.stringify(r)).join("\n") + "\n",
  );
  return w;
}

test("explicit reader windows retain known, unsupported and estimated pre-window cache evidence without billing it", async () => {
  for (const mode of ["known", "unsupported", "estimated"] as const) {
    const last =
      mode === "estimated"
        ? assistant(20, "last", 0, 100000)
        : assistant(20, "last", 100000, 2000);
    const w = await profile([
      assistant(0, "first", 0, 100000),
      ...(mode === "known" ? [] : [attempt(mode === "estimated" ? 10 : 17)]),
      last,
    ]);
    try {
      const since = 18 * 60000;
      const full = analyze({
        scan: await scan(w.options),
        catalog: await prices(),
        since,
        now: 21 * 60000,
      });
      const selected = await scan({ ...w.options, since });
      const report = analyze({
        scan: selected,
        catalog: await prices(),
        since,
        now: 21 * 60000,
      });
      const check = report.checks[0]!;
      assert.deepEqual(
        check.comparisonCost,
        full.checks[0]!.comparisonCost,
        mode,
      );
      assert.deepEqual(
        check.cacheEvidence,
        full.checks[0]!.cacheEvidence,
        mode,
      );
      assert.deepEqual(
        check.usageEstimates,
        full.checks[0]!.usageEstimates,
        mode,
      );
      assert.equal(selected.requests.length, 1);
      assert.equal(selected.coverage.requests, 1);
      assert.equal(selected.since, since);
      const inherited = analyze({
        scan: selected,
        catalog: await prices(),
        now: 21 * 60000,
      });
      assert.deepEqual(
        inherited.checks[0]!.comparisonCost,
        check.comparisonCost,
      );
      assert.equal(inherited.checks[0]!.since, since);
      const catalog = await prices();
      assert.throws(
        () => analyze({ scan: selected, catalog, since: since - 1 }),
        /predates the scanned billing window/,
      );

      assert.equal(
        selected.cacheContext.requests.length,
        mode === "known" ? 1 : 2,
      );
      assert.equal(report.historical[0]!.requests, 1);
      assert.equal(check.requests, 1);
      const cli = spawnSync(
        process.execPath,
        [
          resolve("dist/src/cli.js"),
          "--claude-dir",
          w.claude,
          "--since",
          time(18),
          "--prices-file",
          resolve("test/fixtures/prices.json"),
          "--json",
        ],
        {
          encoding: "utf8",
          env: {
            ...process.env,
            HOME: w.home,
            CLAUDE_CONFIG_DIR: w.claude,
            FORCE_PROMPT_CACHING_5M: "",
            ENABLE_PROMPT_CACHING_1H: "",
            CLAUDE_CODE_PROMPT_CACHE_TTL: "",
            CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL: "",
          },
        },
      );
      assert.equal(cli.status, check.action === "change" ? 1 : 0, cli.stderr);
      assert.deepEqual(
        JSON.parse(cli.stdout).checks[0].comparisonCost,
        check.comparisonCost,
      );
      assert.doesNotMatch(cli.stdout, /SYNTHETIC_PRIVATE/);

      if (mode === "known") assert.equal(check.recommended, "1h");
      if (mode === "unsupported") {
        assert.equal(check.recommended, null);
        assert.equal(check.usageEstimates[0]!.evidence, "unknown");
      }
      if (mode === "estimated") {
        assert.equal(check.usageEstimates[0]!.evidence, "estimated");
        assert.ok(check.sensitivity.unlinked);
      }
    } finally {
      await w.dispose();
    }
  }
});

test("pre-window boundaries reset context while invalid unselected usage remains an unknown predecessor", async () => {
  for (const boundary of [false, true]) {
    const w = await profile([
      {
        ...assistant(0, "invalid", 0, 100000),
        message: {
          id: "invalid",
          model: "claude-opus-5-5",
          usage: { input_tokens: "invalid synthetic counter" },
        },
      },
      ...(boundary
        ? [
            {
              ...common,
              type: "system",
              subtype: "compact_boundary",
              uuid: "boundary",
              timestamp: time(18),
              compactMetadata: { preTokens: "unselected invalid proxy" },
            },
          ]
        : []),
      assistant(20, "last", 100000, 2000),
    ]);
    try {
      const selected = await scan({ ...w.options, since: 19 * 60000 });
      assert.equal(selected.cacheContext.requests[0]!.evidence, "unknown");
      assert.equal(
        selected.cacheContext.requests[0]!.parts[0]!.usage.read,
        null,
      );
      assert.equal(selected.cacheContext.boundaries.length, boundary ? 1 : 0);
      assert.equal(selected.events.length, 0);
      const check = analyze({
        scan: selected,
        catalog: await prices(),
        since: 19 * 60000,
        now: 21 * 60000,
      }).checks[0]!;
      assert.equal(check.comparisonCost["5m"].totalUsd === null, !boundary);
      if (boundary) assert.equal(check.usageEstimates.length, 0);
      else {
        assert.match(
          check.uncertainty.join(" "),
          /pre-window usage is invalid/,
        );
        assert.ok(
          check.cacheEvidence.some((e) =>
            e.predecessors.some((p) => p.prefixEvidence === "unknown"),
          ),
        );
      }
    } finally {
      await w.dispose();
    }
  }
});

test("a request linked across the explicit window retains its recorded start and belongs only to cache context", async () => {
  const w = await profile([
    attempt(17),
    { ...assistant(20, "last", 100000, 2000), requestRef: "attempt" },
  ]);
  try {
    const selected = await scan({ ...w.options, since: 18 * 60000 });
    assert.equal(selected.requests.length, 0);
    assert.equal(selected.coverage.requests, 0);
    assert.equal(selected.cacheContext.requests.length, 1);
    assert.equal(selected.cacheContext.requests[0]!.time, 17 * 60000);
    assert.equal(selected.cacheContext.requests[0]!.evidence, "measured");
    const report = analyze({
      scan: selected,
      catalog: await prices(),
      since: 18 * 60000,
    });
    assert.equal(report.checks.length, 0);
    assert.equal(report.historical.length, 0);
  } finally {
    await w.dispose();
  }
});

test("pre-window boundaries retain summary claims and explicit links before selected attribution", async () => {
  for (const explicit of [false, true]) {
    const first = assistant(0, "old-summary", 0, 0);
    const oldUsage = first.message.usage;
    const summary = {
      ...first,
      querySource: explicit ? "sdk" : "compact",
      message: {
        ...first.message,
        usage: {
          ...oldUsage,
          iterations: [
            { ...oldUsage, type: explicit ? "message" : "compaction" },
          ],
        },
      },
    };
    const boundary = (minute: number) => ({
      ...common,
      type: "system",
      subtype: "compact_boundary",
      uuid: `boundary-${minute}`,
      timestamp: time(minute),
      compactMetadata: {
        preTokens: 1000,
        postTokens: 100,
        ...(explicit && minute === 1 ? { requestId: "old-summary" } : {}),
      },
    });
    const w = await profile([
      summary,
      boundary(1),
      assistant(2, "middle", 0, 1000),
      boundary(15),
      assistant(20, "last", 0, 1000),
    ]);
    try {
      const since = 10 * 60000;
      const complete = analyze({
        scan: await scan(w.options),
        catalog: await prices(),
        since,
      });
      const selected = await scan({ ...w.options, since });
      const report = analyze({
        scan: selected,
        catalog: await prices(),
        since,
      });
      const ids = new Set(selected.events.map((e) => e.id));
      assert.deepEqual(
        report.compactions,
        complete.compactions.filter((c) => ids.has(c.event)),
      );
      assert.equal(report.compactions[0]!.evidence, "estimated");
      assert.equal(report.compactions[0]!.request, null);
      const fullSub = complete.checks.find((c) => c.bucket === "subagent")!;
      const sub = report.checks.find((c) => c.bucket === "subagent")!;
      assert.equal(sub.summaryCost["5m"].totalUsd, 0.006);
      assert.deepEqual(sub.summaryCost, fullSub.summaryCost);
      assert.deepEqual(sub.fixedCost, fullSub.fixedCost);
      assert.deepEqual(sub.cost, fullSub.cost);
      if (explicit)
        assert.equal(
          selected.cacheContext.requests[0]!.parts[0]!.kind,
          "compaction",
        );
    } finally {
      await w.dispose();
    }
  }
});
