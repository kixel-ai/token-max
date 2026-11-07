import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { writeFile } from "node:fs/promises";
import { stripVTControlCharacters } from "node:util";
import type { Report } from "../src/model.js";
import { scan } from "../src/reader.js";
import { analyze } from "../src/replay.js";
import { formatReport } from "../src/report.js";
import { fixture, prices, workspace } from "./helpers.js";

async function syntheticReport(): Promise<Report> {
  const w = await workspace();
  try {
    await fixture(
      join(w.claude, "projects/project/main.jsonl"),
      "claude.jsonl",
    );
    await writeFile(join(w.claude, "settings.json"), '{"promptCacheTtl":"5m"}');
    return analyze({
      scan: await scan({ ...w.options, claudeDirs: [w.claude] }),
      catalog: await prices(),
      now: Date.parse("2026-11-02T00:00:00Z"),
    });
  } finally {
    await w.dispose();
  }
}

test("recommendations lead advisory/keep findings, with separate baselines and complete narrow output", async () => {
  const report = await syntheticReport();
  const change = report.checks[0]!;
  change.savingsUsd = 12.34;
  change.monthlySavingsUsd = 24.68;
  const advisory = structuredClone(change);
  advisory.action = "advisory";
  advisory.evidence = "guidance";
  advisory.check = "synthetic-rare-use-ranking";
  advisory.current = null;
  advisory.recommended = null;
  advisory.savingsUsd = null;
  advisory.monthlySavingsUsd = null;
  advisory.baseline =
    "synthetic recorded profile, holding other settings constant";
  advisory.assumptions = [
    "Synthetic provider guidance; review low-use components manually.",
  ];
  advisory.uncertainty = [
    "Synthetic incomplete invocation visibility; absence of use is unknown.",
  ];
  const keep = structuredClone(change);
  keep.action = "keep";
  keep.current = keep.recommended;
  keep.check = "synthetic-keep";
  report.checks = [advisory, keep, change];
  const profile = report.scan.profiles[0]!;
  profile.root = "/synthetic/東京/" + "long-profile-name-".repeat(8);
  change.owner =
    profile.root + "/settings.json (env.CLAUDE_CODE_PROMPT_CACHE_TTL)";
  const other = {
    ...profile,
    id: "another-profile",
    root: "/synthetic/second-profile",
  };
  report.scan.profiles.push(other);
  advisory.profile = other.id;
  const before = structuredClone(report);
  for (const width of [28, 40, 80]) {
    const text = formatReport(report, { width, details: true });
    assert.ok(
      text.indexOf("Recommended changes") < text.indexOf("Advisory findings"),
    );
    assert.ok(
      text.indexOf("Advisory findings") < text.indexOf("Keep current settings"),
    );
    // The fixture's only wide characters are these two CJK characters.
    assert.ok(
      text
        .split("\n")
        .every((l) => [...l.replace(/[東京]/g, "xx")].length <= width),
    );
    const compact = text.replace(/\s/g, "");
    for (const value of [
      profile.root,
      change.owner,
      other.root,
      advisory.baseline,
      ...advisory.assumptions,
      ...advisory.uncertainty,
    ])
      assert.ok(compact.includes(value.replace(/\s/g, "")), value);
    assert.match(text, /\$12\.34/);
    assert.match(text, /\$24\.68/);
    assert.ok(compact.includes("Current:5m"));
    assert.ok(compact.includes("Suggested:1h"));
    assert.ok(compact.includes("[guidance]"));
    assert.ok(
      compact.includes(
        "Estimatednetsavingvsbaseline:unknownoverwindow;unknown/30days",
      ),
    );
    assert.doesNotMatch(text, /total sav|SYNTHETIC_PRIVATE|\x1b|…/i);
  }
  assert.deepEqual(report, before, "rendering must not alter JSON or analysis");
});

test("details retain every assumption, unknown cost, witness and finding without a top-N cutoff", async () => {
  const report = await syntheticReport();
  const result = report.checks[0]!;
  result.cost["1h"] = {
    knownUsd: 9.87,
    totalUsd: null,
    unknown: ["synthetic unknown fixed charge"],
    assumptions: ["synthetic price assumption"],
  };
  result.assumptions = Array.from(
    { length: 15 },
    (_, i) => `synthetic assumption ${i}`,
  );
  result.uncertainty.push("synthetic final uncertainty");
  result.usageEstimates.push({
    request: "synthetic-attempt",
    evidence: "unknown",
    basis: [],
    part: null,
    maxOutputTokens: 2048,
    reason: "no supported input estimate",
    assumptions: ["synthetic attempt assumption"],
  });
  result.cacheEvidence.push({
    request: "synthetic-cache-request",
    part: 0,
    ttl: "1h",
    state: "unknown",
    outcome: "unknown",
    reason: "synthetic unresolved predecessor",
    predecessors: [
      {
        request: "synthetic-cache-predecessor",
        part: 0,
        time: 0,
        state: "unknown",
        prefixEvidence: "unknown",
        cohortKnown: false,
        reason: "synthetic witness reason",
      },
    ],
  });
  report.checks.push(
    ...Array.from({ length: 12 }, (_, i) => ({
      ...structuredClone(result),
      check: `synthetic-advisory-${i}`,
      action: "advisory" as const,
      evidence: "unknown" as const,
      current: null,
      recommended: null,
      savingsUsd: null,
      monthlySavingsUsd: null,
    })),
  );
  const text = formatReport(report, { details: true, width: 120 });
  for (const value of [
    ...result.assumptions,
    ...result.uncertainty,
    ...result.sources,
    "synthetic price assumption",
    "synthetic attempt assumption",
    "synthetic-advisory-11",
    "synthetic-attempt: unknown",
    "2048 (not observed output)",
    "unknown ($9.87 known subtotal)",
  ])
    assert.ok(
      text.replace(/\s/g, "").includes(value.replace(/\s/g, "")),
      value,
    );
  assert.ok(text.includes(result.cacheEvidence[0]!.request));
  assert.match(text, /Predecessor:/);
  assert.match(formatReport(report), /--details/);
  const styled = formatReport(report, {
    details: true,
    width: 120,
    color: true,
  });
  assert.match(styled, /\x1b\[/);
  assert.equal(stripVTControlCharacters(styled), text);
});

test("terminal controls in all metadata and prose are escaped, while JSON retains exact metadata", async () => {
  const report = await syntheticReport();
  const hostile =
    "synthetic\x1b]8;;https://invalid\x07\r\n\t\u009b31m\u202ereversed\u2066";
  const result = report.checks[0]!;
  result.owner = hostile;
  result.baseline = hostile;
  result.assumptions.push(hostile);
  result.uncertainty.push(hostile);
  report.scan.profiles[0]!.root = hostile;
  report.prices.source = hostile;
  const text = formatReport(report, { details: true });
  assert.doesNotMatch(text, /[\x00-\x09\x0b-\x1f\x7f-\x9f\u202e\u2066]/);
  const compact = text.replace(/\s/g, "");
  assert.ok(compact.includes("\\u009b31m\\u202ereversed\\u2066"));
  assert.equal(JSON.parse(JSON.stringify(report)).checks[0].owner, hostile);
});
