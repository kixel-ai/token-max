import { claudeFastSupport } from "./controls.js";
import type {
  BillingPart,
  Cost,
  Request,
  Scan,
  SpeedCheckResult,
} from "./model.js";
import {
  addCosts,
  priceComponents,
  unknownCost,
  zeroCost,
  type Catalog,
} from "./prices.js";

const SOURCES = [
  "https://code.claude.com/docs/en/fast-mode (2026-11-07)",
  "https://code.claude.com/docs/en/settings-reference#fastmode (2026-11-07)",
  "https://learn.chatgpt.com/docs/agent-configuration/speed (2026-11-07)",
  "https://learn.chatgpt.com/docs/config-file/config-reference (2026-11-07)",
  "https://learn.chatgpt.com/docs/config-file/config-advanced#profiles (2026-11-07)",
  "https://developers.openai.com/api/docs/guides/fast-mode (2026-11-07)",
  "https://developers.openai.com/api/docs/guides/ultrafast-mode (2026-11-07)",
  "https://developers.openai.com/api/docs/pricing (2026-11-07)",
  "https://developers.openai.com/api/docs/models/gpt-5.6 (2026-11-07)",
];

function premium(r: Request, p: BillingPart): boolean | null {
  if (r.harness === "claude") {
    if (!["standard", "default"].includes(p.tier ?? "")) return null;
    return p.speed === "fast" ? true : p.speed === "standard" ? false : null;
  }
  return ["fast", "priority", "ultrafast"].includes(p.tier ?? "")
    ? true
    : ["standard", "default"].includes(p.tier ?? "")
      ? false
      : null;
}
function standard(p: BillingPart): BillingPart {
  return { ...p, tier: "default", speed: "standard" };
}
function difference(catalog: Catalog, r: Request, p: BillingPart): Cost {
  if (
    r.evidence === "unknown" ||
    r.issues.some(
      (s) =>
        s.includes("no verified request delta") ||
        s.includes("usage is absent") ||
        s.includes("no linked assistant usage"),
    )
  )
    return unknownCost("no verified billed usage for speed comparison");
  const billed = priceComponents(catalog, r, p);
  const normal = priceComponents(catalog, r, standard(p));
  return addCosts(
    (["cache", "ordinary", "output"] as const).map((key): Cost => {
      const a = billed[key],
        b = normal[key];
      // A matched missing category contributes unknown to both sides. Only the
      // identically covered numeric categories contribute a known difference.
      const sharedCoverage =
        JSON.stringify([...a.unknown].sort()) ===
        JSON.stringify([...b.unknown].sort());
      return {
        knownUsd:
          (a.totalUsd !== null && b.totalUsd !== null) || sharedCoverage
            ? a.knownUsd - b.knownUsd
            : 0,
        totalUsd:
          a.totalUsd !== null && b.totalUsd !== null
            ? a.totalUsd - b.totalUsd
            : null,
        unknown: [...new Set([...a.unknown, ...b.unknown])],
        assumptions: [...new Set([...a.assumptions, ...b.assumptions])],
      };
    }),
  );
}

/** Compare speed alone at the current profile baseline, independently of TTL/window recommendations. */
export function speedChecks(options: {
  scan: Scan;
  catalog: Catalog;
  since: number | undefined;
  now: number;
}): SpeedCheckResult[] {
  const { scan, catalog, now } = options;
  return scan.profiles.flatMap((profile): SpeedCheckResult[] => {
    if (!profile.speed) return [];
    const requests = scan.requests.filter(
      (r) =>
        r.profiles.includes(profile.id) &&
        (options.since === undefined || r.time >= options.since),
    );
    const groups = new Map<
      string,
      { control: NonNullable<typeof profile.speed>; requests: Request[] }
    >();
    for (const r of requests) {
      const control =
        (r.cwd ? profile.speedDirectories?.[r.cwd] : undefined) ??
        profile.speed;
      const key = JSON.stringify(control);
      const group = groups.get(key) ?? { control, requests: [] };
      group.requests.push(r);
      groups.set(key, group);
    }
    if (!requests.length)
      groups.set("default", { control: profile.speed, requests: [] });
    return [...groups.values()].map(
      ({ control, requests }): SpeedCheckResult => {
        let premiumParts = 0,
          standardParts = 0,
          unknownParts = 0;
        let pricedParts = 0,
          estimateUnknown = 0;
        const observed: Cost[] = [];
        const estimated: Cost[] = [];
        for (const r of requests) {
          if (!r.parts.length) {
            unknownParts++;
            observed.push(unknownCost("request parts are absent"));
            estimateUnknown++;
            estimated.push(unknownCost("request parts are absent"));
          }
          for (const p of r.parts) {
            const state = premium(r, p);
            if (state === true) {
              premiumParts++;
              observed.push(difference(catalog, r, p));
            } else if (state === false) {
              standardParts++;
              observed.push(zeroCost());
            } else {
              unknownParts++;
              observed.push(
                unknownCost(
                  "billed response speed/tier is unknown; requested controls are not billing evidence",
                ),
              );
            }
            if (control.value === null || !control.resolved) {
              estimateUnknown++;
              estimated.push(
                unknownCost("current speed ownership is unresolved"),
              );
              continue;
            }
            if (control.value === "standard") {
              estimated.push(zeroCost());
              continue;
            }
            // Claude only enables fast mode on supported Opus; other models stay standard.
            if (
              r.harness === "claude" &&
              claudeFastSupport(p.model) === false
            ) {
              estimated.push(zeroCost());
              continue;
            }
            if (p.kind === "advisor" || p.kind === "unknown") {
              estimateUnknown++;
              estimated.push(
                unknownCost(
                  "current speed ownership of advisor/unknown iteration is not established",
                ),
              );
              continue;
            }
            const current =
              r.harness === "claude"
                ? { ...p, tier: "standard", speed: "fast" }
                : { ...p, tier: control.value, speed: null };
            const delta = difference(catalog, r, current);
            if (delta.totalUsd === null) estimateUnknown++;
            else pricedParts++;
            estimated.push(delta);
          }
        }
        const start = options.since ?? requests[0]?.time ?? null;
        const month = (v: number | null) =>
          v !== null && start !== null && now > start
            ? (v * 30 * 86400000) / (now - start)
            : null;
        const observedCost = observed.length
          ? addCosts(observed)
          : unknownCost("no selected billed usage");
        const currentCost = estimated.length
          ? addCosts(estimated)
          : unknownCost("no selected usage for current-setting estimate");
        const action =
          !control.resolved || control.value === null
            ? "advisory"
            : control.value === "standard"
              ? "keep"
              : "change";
        return {
          check: "fast-mode",
          bucket: "all",
          profile: profile.id,
          historyProfiles: [...new Set(requests.flatMap((r) => r.profiles))],
          evidence: "guidance",
          action,
          current: control.value,
          recommended: control.resolved ? "standard" : null,
          owner: control.owner,
          replacement: control.replacement,
          reason: control.reason,
          since: start,
          until: now,
          windowBasis:
            options.since === undefined
              ? "available selected usage; current files applied to history, no current session override inferred"
              : "explicit --since; current files applied to history",
          baseline:
            "current profile speed, holding recorded model, work, tokens, cache categories, geography, context tiers and all other settings constant; independent of TTL/window/idle recommendations",
          requests: requests.length,
          savingsUsd: action === "change" ? currentCost.totalUsd : null,
          monthlySavingsUsd:
            action === "change" ? month(currentCost.totalUsd) : null,
          breakEven: null,
          observed: {
            premiumParts,
            standardParts,
            unknownParts,
            premium: observedCost,
            monthlyPremiumUsd: month(observedCost.totalUsd),
          },
          currentEstimate: {
            premium: currentCost,
            pricedParts,
            unknownParts: estimateUnknown,
          },
          usageEstimates: [],
          assumptions: [
            "maintainer cost preference: fast and ultrafast off; setting warning does not require historical billing metadata",
            "rate-only API-price equivalent at present public prices, not an invoice, subscription bill, purchased-credit or rate-limit multiplier",
            "recorded cache categories and context tiers held constant; Claude fast/standard have separate caches, so switching cold-cache effects are excluded",
            "fixed behavior and work; latency value is not modeled; no summed per-check savings",
            "current local files and active-profile environment supply defaults; historical session overrides never establish current session state",
          ],
          uncertainty: [
            control.reason,
            "account/server policy, availability, cloud-managed defaults, launch overrides and live session toggles not observed in local files may override these defaults",
            ...observedCost.unknown,
            ...currentCost.unknown,
          ],
          sources: SOURCES,
        };
      },
    );
  });
}
