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

/** Estimate isolated attempts from matching or same-cache-lifetime growing input brackets. */
export function estimateUsage(scan: Scan): Map<string, UsageEstimate> {
  const result = new Map<string, UsageEstimate>();
  const boundaries = new Map<string, number[]>();
  for (const event of scan.events) {
    if (event.harness !== "claude" || event.kind !== "compaction") continue;
    for (const origin of event.origins) {
      const times = boundaries.get(origin.session) ?? [];
      times.push(event.time);
      boundaries.set(origin.session, times);
    }
  }
  for (const times of boundaries.values()) times.sort((a, b) => a - b);
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
        before.parts.length !== 1 ||
        after.parts.length !== 1 ||
        r.parts.length !== 1 ||
        before.evidence !== "measured" ||
        after.evidence !== "measured" ||
        pending.usage.input !== null ||
        pending.usage.read !== null ||
        pending.usage.write !== null
      ) {
        e.reason =
          "adjacent requests do not supply matching measured single-part input evidence";
        continue;
      }
      if (
        !commonFile ||
        !r.reference ||
        !r.source ||
        !r.model ||
        !r.provider ||
        before.endTime >= r.time ||
        after.time <= r.time ||
        [before, after].some(
          (n) =>
            n.source !== r.source ||
            n.model !== r.model ||
            n.provider !== r.provider ||
            n.agent !== r.agent ||
            n.version !== r.version ||
            n.client !== r.client ||
            [...n.profiles].sort().join(":") !== profiles,
        )
      ) {
        e.reason =
          "no matching source/model/provider/version/client/profile provenance and nonoverlapping file-local brackets";
        continue;
      }
      if (
        p.kind === "unknown" ||
        p.kind !== pending.kind ||
        p.model !== r.model ||
        (["kind", "model", "tier", "speed", "geo"] as const).some(
          (k) => p[k] !== next[k],
        ) ||
        (["tier", "speed", "geo"] as const).some(
          (k) => pending[k] !== null && pending[k] !== p[k],
        )
      ) {
        e.reason =
          "brackets lack matching recorded input/cache/pricing metadata or contradict the request";
        continue;
      }
      const exact = sameInput(p, next);
      const complete = [p, next].every(
        (part) =>
          part.usage.input !== null &&
          part.usage.read !== null &&
          part.usage.write !== null,
      );
      const previousPrefix = (p.usage.read ?? 0) + (p.usage.write ?? 0);
      const inputBeforeNewWrites =
        next.usage.input === null || next.usage.write === null
          ? null
          : next.usage.input - next.usage.write;
      // Five minutes is the shortest supported cache lifetime, not a retry timeout.
      const growing =
        complete &&
        after.time - before.time < 5 * 60_000 &&
        next.usage.read! > 0 &&
        inputBeforeNewWrites! >= p.usage.input! &&
        next.usage.read! >= previousPrefix;
      if (!exact && !growing) {
        e.reason =
          "no matching geometry or growing-context warm-read evidence within the five-minute cache lifetime";
        continue;
      }
      const times = boundaries.get(r.session) ?? [];
      let low = 0;
      let high = times.length;
      while (low < high) {
        const middle = Math.floor((low + high) / 2);
        if (times[middle]! <= before.time) low = middle + 1;
        else high = middle;
      }
      if (low < times.length && times[low]! <= after.time) {
        e.reason = "matching brackets are interrupted by a compaction boundary";
        continue;
      }
      e.evidence = "estimated";
      e.part = {
        ...(exact ? p : next),
        usage: {
          ...(exact ? p.usage : next.usage),
          output: null,
          reasoning: null,
          ...(!exact
            ? {
                input: inputBeforeNewWrites,
                read: previousPrefix,
                write: next.usage.read! - previousPrefix,
                // Replay assigns the chosen TTL, not an invented observed split.
                write5m: null,
                write1h: null,
              }
            : {}),
        },
      };
      e.reason = exact
        ? "matching adjacent input geometry and pricing metadata in the same conversation/file"
        : "growing-context downstream cache-read evidence within one five-minute cache lifetime, with matching pricing/provenance";
      e.assumptions = [
        exact
          ? "one billed API attempt with the same input/cache geometry and model/tier/speed/geography as both recorded brackets; this is not measured execution or prefix identity"
          : "one billed attempt with the preceding cached prefix as warm reads and any additional next-request reads attributed as its writes; next-request new writes are excluded from its input, and next-request ordinary input is assumed unchanged",
        "retry/content identity, cache survival and attribution of extra warm reads to this attempt rather than another cache warmer are assumptions, not measured billing",
        "actual usage and historical spend stay unknown; output is not imputed and cancels only under fixed behavior across TTL scenarios",
        "no-billed-attempt and cold-cache sensitivities are conditional scenarios, not guaranteed bounds on hidden work or context changes",
      ];
    }
  }
  return result;
}
