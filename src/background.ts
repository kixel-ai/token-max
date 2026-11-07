import type { CheckBase, Cost, Profile, Request, Scan } from "./model.js";
import { scopedEnvironment } from "./controls.js";
import { object, ScanError } from "./safe.js";
import { addCosts, priceRequest, unknownCost, type Catalog } from "./prices.js";

/** Native controls are separate from host-generated helpers and idle compaction. */
export type BackgroundFeature =
  "prompt-suggestions" | "session-recap" | "goal-checkins" | "auto-recap";
/** A currently configured value does not establish historical eligibility or generation. */
export interface BackgroundControl {
  value: boolean | number | null;
  owner: string;
  resolved: boolean;
  reason: string;
}
/** Each native feature keeps its own effective control and owner. */
export type BackgroundControls = Partial<
  Record<BackgroundFeature, BackgroundControl>
>;
/** Provider guidance remains advisory until feature usage and net effects can be priced. */
export interface BackgroundCheckResult extends CheckBase<boolean | number> {
  check: "background-generation";
  bucket: BackgroundFeature;
  applicability: "eligible-client" | "ineligible-client" | "unknown";
  clients: {
    client: string | null;
    version: string | null;
    requests: number;
  }[];
  coverage: string[];
  guidance: string;
  /** Only explicitly identified requests; missing calls keep the feature total unknown. */
  attributedRequests: string[];
  attributedCost: Cost;
  totalFeatureCost: Cost;
}

const CLAUDE_SETTINGS = "https://code.claude.com/docs/en/settings-reference";
const CLAUDE_ENV = "https://code.claude.com/docs/en/env-vars";
const CLAUDE_INTERACTIVE = "https://code.claude.com/docs/en/interactive-mode";
const CODEX_CONFIG =
  "https://learn.chatgpt.com/docs/config-file/config-reference";

/** Resolve native background controls from observed files and the active profile environment. */
export function backgroundControls(options: {
  harness: Profile["harness"];
  scopes: { config: Record<string, unknown>; file: string }[];
  env: Record<string, unknown>;
}): BackgroundControls {
  const first = options.scopes[0]?.file ?? "settings.json";
  // Native sessions apply settings.env over inherited shell values. Other checks
  // retain their own environment semantics; no settings or process env are changed.
  const { values: env, owners } = scopedEnvironment({
    scopes: options.scopes,
    env: {},
  });
  for (const [key, value] of Object.entries(options.env))
    if (!Object.hasOwn(env, key)) {
      env[key] = value;
      owners[key] = key;
    }
  function toggle(key: string, nested = false): BackgroundControl {
    let value: boolean | null = true;
    let owner = `${first}:${nested ? "tui." : ""}${key}`;
    for (const { config, file } of options.scopes) {
      const row = nested ? object(config.tui) : config;
      if (!Object.hasOwn(row, key)) continue;
      if (typeof row[key] !== "boolean")
        throw new ScanError(`invalid ${nested ? "tui." : ""}${key}`, {
          file,
          line: 1,
        });
      value = row[key];
      owner = `${file}:${nested ? "tui." : ""}${key}`;
    }
    return {
      value,
      owner,
      resolved: true,
      reason:
        "current observed files; documented default is on; unrecorded launch overrides remain unknown",
    };
  }
  if (options.harness === "codex") {
    const c = toggle("auto_recap", true);
    const user = options.scopes[0]?.config ?? {};
    if (
      user.profile !== undefined ||
      Object.keys(object(user.profiles)).length
    ) {
      c.value = null;
      c.resolved = false;
      c.reason =
        "Codex named profile selection and launch overrides are not established";
    }
    return { "auto-recap": c };
  }
  const suggestions = toggle("promptSuggestionEnabled");
  const recap = toggle("awaySummaryEnabled");
  function override(
    c: BackgroundControl,
    key: string,
    forms: Record<string, boolean>,
  ) {
    if (env[key] === undefined) return;
    const value = String(env[key]).toLowerCase();
    c.value = Object.hasOwn(forms, value) ? forms[value]! : null;
    c.owner = owners[key]!;
    c.resolved = c.value !== null;
    c.reason = c.resolved
      ? "current environment override takes precedence over the toggle"
      : "environment override is not a verified boolean form; effective state unknown";
  }
  const forms = {
    true: true,
    false: false,
    "1": true,
    "0": false,
    yes: true,
    no: false,
    on: true,
    off: false,
  };
  override(suggestions, "CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION", forms);
  override(recap, "CLAUDE_CODE_ENABLE_AWAY_SUMMARY", forms);
  const blocked =
    ["DISABLE_TELEMETRY", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"].find(
      (key) => env[key] !== undefined && String(env[key]) !== "",
    ) ??
    [
      "DO_NOT_TRACK",
      "CLAUDE_CODE_USE_BEDROCK",
      "CLAUDE_CODE_USE_VERTEX",
      "CLAUDE_CODE_USE_FOUNDRY",
    ].find((key) =>
      ["1", "true", "yes", "on"].includes(String(env[key]).toLowerCase()),
    );
  if (env.CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION === undefined && blocked) {
    suggestions.value = false;
    suggestions.owner = owners[blocked]!;
    suggestions.reason =
      "provider or telemetry conditions disable suggestions without an explicit environment opt-in; the settings toggle alone has no effect";
  }
  const key = "CLAUDE_CODE_GOAL_CHECKIN_MINUTES";
  const raw = env[key];
  const minutes =
    raw !== undefined && /^\d+$/.test(String(raw)) && Number(raw) <= 10080
      ? Number(raw)
      : 30;
  return {
    "prompt-suggestions": suggestions,
    "session-recap": recap,
    "goal-checkins": {
      value: minutes,
      owner: owners[key] ?? `${first}:env.${key}`,
      resolved: true,
      reason:
        raw === undefined
          ? "documented default: 30 minutes; requires an active goal waiting on background work"
          : minutes === Number(raw)
            ? "current environment interval; zero disables goal check-ins"
            : "invalid interval uses the documented 30 minute default",
    },
  };
}

function applicability(
  requests: Request[],
  feature: BackgroundFeature,
): BackgroundCheckResult["applicability"] {
  const clients = requests.map((r) => {
    if (r.agent === "subagent")
      return feature === "goal-checkins" ? "unknown" : "ineligible-client";
    if (r.harness === "codex")
      return r.source === "cli" &&
        ["cli", "codex_cli_rs"].includes(r.client ?? "") &&
        /^0\.(\d+)\.\d+$/.test(r.version ?? "") &&
        Number(r.version!.split(".")[1]) >= 167
        ? "eligible-client"
        : r.source === "exec" ||
            r.client === "codex_exec" ||
            r.client === "codex_vscode"
          ? "ineligible-client"
          : "unknown";
    if (
      r.client?.startsWith("sdk") ||
      r.client === "mcp" ||
      r.client === "claude-code-github-action"
    )
      return feature === "goal-checkins" ? "unknown" : "ineligible-client";
    const patch = /^2\.1\.(\d+)$/.exec(r.version ?? "");
    if (
      !patch ||
      Number(patch[1]) <
        (feature === "goal-checkins"
          ? 234
          : feature === "session-recap"
            ? 319
            : 238)
    )
      return "unknown";
    return r.client === "cli" && r.source === "repl_main_thread"
      ? "eligible-client"
      : "unknown";
  });
  return clients.includes("eligible-client")
    ? "eligible-client"
    : clients.length && clients.every((c) => c === "ineligible-client")
      ? "ineligible-client"
      : "unknown";
}

/** Report native features separately without inventing attribution from idle time or reply length. */
export function backgroundChecks(options: {
  scan: Scan;
  catalog: Catalog;
  since?: number | undefined;
  now: number;
}): BackgroundCheckResult[] {
  const { scan, now } = options;
  const checks: BackgroundCheckResult[] = [];
  for (const profile of scan.profiles) {
    const requests = scan.requests.filter((r) =>
      r.profiles.includes(profile.id),
    );
    const groups = new Map<
      string,
      {
        feature: BackgroundFeature;
        control: BackgroundControl;
        requests: Request[];
      }
    >();
    const add = (controls: BackgroundControls, subset: Request[]) => {
      for (const [feature, control] of Object.entries(controls) as [
        BackgroundFeature,
        BackgroundControl,
      ][]) {
        const key = JSON.stringify([feature, control]);
        const group = groups.get(key) ?? { feature, control, requests: [] };
        group.requests.push(...subset);
        groups.set(key, group);
      }
    };
    if (!requests.length) add(profile.background ?? {}, []);
    for (const r of requests)
      add(
        (r.cwd && profile.backgroundDirectories?.[r.cwd]) ||
          profile.background ||
          {},
        [r],
      );
    for (const { feature, control, requests: relevant } of groups.values()) {
      const since = options.since ?? scan.since ?? relevant[0]?.time ?? null;
      const selected = relevant.filter(
        (r) => since === null || r.time >= since,
      );
      const active = control.value !== false && control.value !== 0;
      const goal = feature === "goal-checkins";
      // The installed 2.1.319 generator supplies this source explicitly. Away-summary
      // source alone cannot distinguish automatic recaps from an explicit /recap.
      const attributed =
        feature === "prompt-suggestions"
          ? selected.filter(
              (r) => r.harness === "claude" && r.source === "prompt_suggestion",
            )
          : [];
      const attributedCost = attributed.length
        ? addCosts(attributed.map((r) => priceRequest(options.catalog, r)))
        : unknownCost("no verified feature-attributed billed requests");
      const totalFeatureCost = addCosts([
        attributedCost,
        unknownCost(
          "generation ledger is incomplete; unlogged feature usage is not measured zero",
        ),
      ]);
      const coverage = [
        "No complete generation ledger for this client/version; total feature dollars are unknown, never zero. Explicitly attributed usage, when present, is a subset already included in recorded-request costs.",
        "Current settings do not prove historical activation. Missing historical settings, client lifetime, focus, goal state and feature flags prevent a documented-trigger count.",
        "Unlogged calls and cache refreshes can change the recorded-request baseline and every affected cache, window and idle replay.",
        "T3 ephemeral helper generation is distinct from native features and may be unlogged; it is outside this check.",
        feature === "prompt-suggestions"
          ? "A completed response is not a generation count: cold caches, short conversations, plan mode, errors, usage limits, feature flags and backoff can suppress suggestions. Print/SDK mode needs explicit opt-in."
          : feature === "session-recap"
            ? "Automatic recaps need an interactive session with at least three turns, three minutes after completion and an unfocused terminal, and do not run twice in a row. Elapsed time alone establishes none of these conditions."
            : feature === "goal-checkins"
              ? "The interval applies only to an active goal waiting on background work; scheduled tasks, explicit requests, guardian reviews and subagent calls alone are not goal check-ins."
              : "Automatic Codex recaps require the native unfocused terminal UI. Explicit /recap remains available with tui.auto_recap=false; an exec or IDE transcript is not evidence of this TUI work.",
      ];
      const applies = applicability(selected, feature);
      const guidance = !control.resolved
        ? "Resolve the effective current control and its owner before changing this feature; historical spend remains unknown."
        : !active
          ? "Current control disables this feature; historical spend remains unknown."
          : applies === "ineligible-client"
            ? "Selected clients do not establish an applicable interactive UI feature. An enabled toggle does not prove background generation; no change is recommended."
            : goal
              ? "Keep goal check-ins conditional: disabling can remove autonomous follow-on work. No priced net comparison supports a change."
              : "If you do not use this UI generation, consider disabling it. Lost suggestions or recaps and any cache warming are the tradeoff; no priced net comparison supports an automatic change.";
      const clients = new Map<
        string,
        BackgroundCheckResult["clients"][number]
      >();
      for (const r of selected) {
        const key = JSON.stringify([r.client, r.version]);
        const c = clients.get(key) ?? {
          client: r.client,
          version: r.version,
          requests: 0,
        };
        c.requests++;
        clients.set(key, c);
      }
      checks.push({
        check: "background-generation",
        bucket: feature,
        profile: profile.id,
        historyProfiles: [...new Set(selected.flatMap((r) => r.profiles))],
        evidence: attributed.length
          ? attributedCost.totalUsd === null
            ? "unknown"
            : "estimated"
          : "guidance",
        action: "advisory",
        current: control.resolved ? control.value : null,
        recommended: null,
        owner: control.owner,
        since,
        until: now,
        windowBasis:
          options.since !== undefined || scan.since !== null
            ? "explicit --since; historical feature settings are not established"
            : "available history; current controls applied to historical sessions without a recorded setting-change history",
        baseline:
          "recorded/current profile baseline with all other settings held constant; unlogged background generation and cache refreshes remain unknown",
        requests: selected.length,
        savingsUsd: null,
        monthlySavingsUsd: null,
        breakEven: null,
        assumptions: [
          control.reason,
          "fixed behavior would hold retained user requests constant; gross generation cost alone is insufficient to establish removable net cost",
          "per-check savings are independent and must not be summed",
        ],
        uncertainty: [
          ...coverage,
          "No verified complete context/output proxies, cache-refresh linkage or follow-on comparison; no generation count is estimated from elapsed time. Reading or accepting UI generation is unobserved, a fixed-behavior caveat rather than a veto on a supported net estimate.",
          ...(feature === "session-recap"
            ? [
                "away_summary source can also serve explicit /recap; it alone does not attribute automatic generation or a removable charge.",
              ]
            : []),
        ],
        sources:
          profile.harness === "claude"
            ? [
                CLAUDE_SETTINGS,
                CLAUDE_ENV,
                CLAUDE_INTERACTIVE,
                "https://code.claude.com/docs/en/costs#background-token-usage",
              ]
            : [
                CODEX_CONFIG,
                "https://learn.chatgpt.com/docs/developer-commands",
              ],
        applicability: applies,
        clients: [...clients.values()],
        coverage,
        guidance,
        attributedRequests: attributed.map((r) => r.id),
        attributedCost,
        totalFeatureCost,
      });
    }
  }
  return checks;
}
