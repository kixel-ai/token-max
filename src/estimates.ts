import type { BillingPart, Request, Scan, UsageEstimate } from "./model.js";

function sameInput(a: BillingPart, b: BillingPart): boolean {
  return (
    a.usage.input !== null &&
    a.usage.read !== null &&
    a.usage.write !== null &&
    JSON.stringify([
      a.kind,
      a.model,
      a.tier,
      a.speed,
      a.geo,
      a.usage.input,
      a.usage.read,
      a.usage.write,
      a.usage.write5m,
      a.usage.write1h,
    ]) ===
      JSON.stringify([
        b.kind,
        b.model,
        b.tier,
        b.speed,
        b.geo,
        b.usage.input,
        b.usage.read,
        b.usage.write,
        b.usage.write5m,
        b.usage.write1h,
      ])
  );
}

/** Estimate only isolated unlinked attempts bracketed by matching recorded input geometry. */
export function estimateUsage(scan: Scan): Map<string, UsageEstimate> {
  const result = new Map<string, UsageEstimate>();
  const sessions = new Map<string, Request[]>();
  for (const r of scan.requests.filter((r) => r.harness === "claude")) {
    const group = sessions.get(r.session) ?? [];
    group.push(r);
    sessions.set(r.session, group);
  }
  for (const group of sessions.values()) {
    group.sort((a, b) => a.time - b.time || a.id.localeCompare(b.id));
    for (let index = 0; index < group.length; index++) {
      const r = group[index]!;
      if (!r.attempt || r.evidence !== "unknown") continue;
      const e: UsageEstimate = {
        request: r.id,
        evidence: "unknown",
        basis: [],
        part: null,
        maxOutputTokens: r.attempt.maxOutputTokens,
        reason: "no matching adjacent recorded input evidence",
        assumptions: [],
      };
      result.set(r.id, e);
      const before = group[index - 1];
      const after = group[index + 1];
      if (!before || !after) continue;
      e.basis = [before.id, after.id];
      const p = before.parts[0];
      const next = after.parts[0];
      const pending = r.parts[0];
      const files = new Set(r.locations.map((l) => l.file));
      const commonFile = before.locations.some(
        (l) =>
          files.has(l.file) && after.locations.some((n) => n.file === l.file),
      );
      const profiles = [...r.profiles].sort().join(":");
      if (
        !p ||
        !next ||
        !pending ||
        pending.usage.input !== null ||
        pending.usage.read !== null ||
        pending.usage.write !== null ||
        !commonFile ||
        !r.reference ||
        !r.source ||
        !r.model ||
        !r.provider ||
        before.evidence !== "measured" ||
        after.evidence !== "measured" ||
        before.parts.length !== 1 ||
        after.parts.length !== 1 ||
        r.parts.length !== 1 ||
        before.endTime >= r.time ||
        after.time <= r.time ||
        [before, after].some(
          (n) =>
            n.source !== r.source ||
            n.model !== r.model ||
            n.provider !== r.provider ||
            n.agent !== r.agent ||
            n.version !== r.version ||
            [...n.profiles].sort().join(":") !== profiles,
        ) ||
        p.kind === "unknown" ||
        p.kind !== pending.kind ||
        p.model !== r.model ||
        !sameInput(p, next) ||
        ["tier", "speed", "geo"].some(
          (k) =>
            pending[k as "tier" | "speed" | "geo"] !== null &&
            pending[k as "tier" | "speed" | "geo"] !==
              p[k as "tier" | "speed" | "geo"],
        ) ||
        scan.events.some(
          (event) =>
            ["compaction", "settings", "client"].includes(event.kind) &&
            event.time > before.time &&
            event.time <= after.time &&
            event.origins.some((o) => o.session === r.session),
        )
      )
        continue;
      e.evidence = "estimated";
      e.part = { ...p, usage: { ...p.usage, output: null, reasoning: null } };
      e.reason =
        "matching adjacent input geometry and pricing metadata in the same conversation/file";
      e.assumptions = [
        "one billed API attempt with the same input/cache geometry and model/tier/speed/geography as both recorded brackets; this is not measured execution or prefix identity",
        "actual usage and historical spend stay unknown; output is not imputed and cancels only under fixed behavior across TTL scenarios",
        "no-billed-attempt and cold-cache sensitivities are conditional scenarios, not guaranteed bounds on hidden work or context changes",
      ];
    }
  }
  return result;
}
