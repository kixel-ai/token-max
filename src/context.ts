import type { Category, Component } from "./exposure.js";
import type { CheckBase, Cost, Report, Scan, UsageEstimate } from "./model.js";
import { addCosts, unknownCost } from "./prices.js";

/** Current supported controls, separate from historical exposure evidence. */
export interface ContextState {
  cwd: string | null;
  resolved: boolean;
  reason: string;
  controls: {
    key: string;
    owner: string;
    value: string | number | boolean | null;
  }[];
  servers: {
    name: string;
    group: string;
    owner: string;
    enabled: boolean | null;
    alwaysLoad: boolean | null;
    omitted: string[];
    blockedTools: number;
  }[];
  apps: { name: string; owner: string; enabled: boolean; omitted: string[] }[];
  files: {
    file: string;
    characters: number;
    bytes: number;
    lines: number;
    fingerprint: string;
  }[];
}
/** One category or revision is attributed once, with shared skill costs linked only. */
export interface ContextAttribution {
  component: string;
  name: string | null;
  category: Category;
  group: string | null;
  locations: Component["locations"];
  source: string | null;
  wireRequests: number;
  candidateRequests: number;
  sessions: number;
  characters: number | null;
  tokens: number | null;
  gross: Cost;
  grossRangeUsd: [number, number] | null;
  net: Cost;
}
/** Context checks use provider budgets and incomplete-visibility advisories without invented savings. */
export interface ContextCheckResult extends CheckBase<string> {
  check: "loaded-context";
  bucket: "all";
  reason: string;
  attribution: ContextAttribution[];
  categories: {
    category: Category;
    revisions: number;
    wireRequests: number;
    candidateRequests: number;
  }[];
  tools: {
    target: string;
    name: string | null;
    group: string | null;
    since: number | null;
    until: number;
    exposedRequests: number;
    candidateRequests: number;
    exposedSessions: number;
    observedCalls: number;
    completed: number;
    failed: number;
    references: number;
    lastUse: number | null;
    usage: "observed use" | "no observed use";
    visibility: "incomplete";
    newlyObserved: boolean;
    automaticUnused: false;
  }[];
  servers: {
    group: string;
    owners: string[];
    exposedRequests: number;
    candidateRequests: number;
    exposedSessions: number;
    observedCalls: number;
    completed: number;
    failed: number;
    lastUse: number | null;
    visibility: "incomplete";
  }[];
  guidance: {
    kind: string;
    owner: string;
    message: string;
    action: "change" | "advisory";
  }[];
  controls: ContextState[];
  sharedSkills: { component: string; group: string | null; owner: "skills" }[];
  usageEstimates: UsageEstimate[];
}

const SOURCES = [
  "https://code.claude.com/docs/en/mcp (2026-11-07)",
  "https://code.claude.com/docs/en/costs#reduce-mcp-server-overhead (2026-11-07)",
  "https://code.claude.com/docs/en/memory (2026-11-07)",
  "https://code.claude.com/docs/en/settings-reference#bashoutputmaxchars (2026-11-07)",
  "https://learn.chatgpt.com/docs/extend/mcp (2026-11-07)",
  "https://learn.chatgpt.com/docs/config-file/config-reference (2026-11-07)",
  "https://learn.chatgpt.com/docs/agent-configuration/agents-md (2026-11-07)",
];

function sum(costs: Cost[], reason: string): Cost {
  return costs.length ? addCosts(costs) : unknownCost(reason);
}

/** Attribute observed context independently of the cache/window policy recommendations. */
export function contextChecks(options: {
  scan: Scan;
  exposure: Report["exposure"];
  since: number | undefined;
  now: number;
}): ContextCheckResult[] {
  const { scan, exposure, now } = options;
  const requestById = new Map(scan.requests.map((r) => [r.id, r]));
  const componentById = new Map(scan.exposure.components.map((c) => [c.id, c]));
  return scan.profiles.flatMap((profile) => {
    const requests = scan.requests.filter(
      (r) =>
        r.profiles.includes(profile.id) &&
        (options.since === undefined || r.time >= options.since),
    );
    const selected = new Set(requests.map((r) => r.id));
    const views = scan.exposure.requests.filter((e) => selected.has(e.request));
    const allIds = new Set(
      views.flatMap((e) => [...e.components, ...e.candidates, ...e.deferred]),
    );
    const components = [...allIds]
      .map((id) => componentById.get(id)!)
      .filter(Boolean);
    const controls = (profile.context ?? []).filter((s) =>
      requests.length ? requests.some((r) => r.cwd === s.cwd) : s.cwd === null,
    );
    if (
      !components.some((c) => c.owner === "context") &&
      !controls.some(
        (c) =>
          c.files.length ||
          c.servers.length ||
          c.controls.some((v) => v.value !== null),
      )
    )
      return [];
    const starts = [
      ...requests.map((r) => r.time),
      ...components.map((c) => c.firstSeen).filter((t) => t > 0),
      ...scan.exposure.invocations
        .filter((i) => i.profiles.includes(profile.id))
        .map((i) => i.time),
    ];
    const since = options.since ?? (starts.length ? Math.min(...starts) : null);
    const costs = new Map<
      string,
      (typeof exposure.requests)[number]["components"]
    >();
    for (const e of exposure.requests.filter((e) => selected.has(e.request)))
      for (const c of e.components)
        costs.set(c.component, [...(costs.get(c.component) ?? []), c]);
    const attribution = components
      .filter((c) => c.owner === "context")
      .map((c): ContextAttribution => {
        const wire = views.filter((e) => e.components.includes(c.id));
        const candidates = views.filter((e) => e.candidates.includes(c.id));
        const priced = costs.get(c.id) ?? [];
        const ranges = priced.map((p) => p.grossRangeUsd);
        return {
          component: c.id,
          name: c.name ?? null,
          category: c.category,
          group: c.group,
          locations: c.locations,
          source: c.source ?? null,
          wireRequests: wire.length,
          candidateRequests: candidates.length,
          sessions: new Set(
            [...wire, ...candidates].map(
              (e) => requestById.get(e.request)!.session,
            ),
          ).size,
          characters: c.characters,
          tokens:
            priced.length &&
            priced.every(
              (p) => p.exposure === "wire" && p.tokens.estimate !== null,
            )
              ? priced.reduce((n, p) => n + p.tokens.estimate!, 0)
              : null,
          gross: sum(
            priced.map((p) => p.gross),
            "no priced wire exposure; deferred definitions and hashes do not establish input",
          ),
          grossRangeUsd:
            ranges.length && ranges.every((r) => r !== null)
              ? ranges.reduce<[number, number]>(
                  (n, r) => [n[0] + r![0], n[1] + r![1]],
                  [0, 0],
                )
              : null,
          net: unknownCost(
            "discovery requests, replacement/backfill, cache invalidation and repeated reads are not established; attributed overhead is not net savings",
          ),
        };
      });
    const calls = scan.exposure.invocations.filter(
      (i) =>
        i.profiles.includes(profile.id) &&
        (since === null || i.time >= since) &&
        i.time <= now,
    );
    const targets = new Set([
      ...components
        .filter((c) => c.category.startsWith("mcp-"))
        .flatMap((c) => c.targets),
      ...calls
        .filter((i) => i.kind === "mcp-call" || i.kind === "script-reference")
        .map((i) => i.target),
    ]);
    const tools = [...targets]
      .map((target) => {
        const definitions = components.filter(
          (c) => c.category.startsWith("mcp-") && c.targets.includes(target),
        );
        const ids = new Set(definitions.map((c) => c.id));
        const wire = views.filter((e) =>
          e.components.some((id) => ids.has(id)),
        );
        const candidate = views.filter((e) =>
          e.candidates.some((id) => ids.has(id)),
        );
        const observed = calls.filter(
          (i) =>
            i.target === target &&
            i.kind === "mcp-call" &&
            i.status !== "reference",
        );
        return {
          target,
          name:
            definitions.find((c) => c.name)?.name ??
            observed.find((i) => i.name)?.name ??
            null,
          group:
            definitions.find((c) => c.group)?.group ??
            observed.find((i) => i.group)?.group ??
            null,
          since,
          until: now,
          exposedRequests: wire.length,
          candidateRequests: candidate.length,
          exposedSessions: new Set(
            wire.map((e) => requestById.get(e.request)!.session),
          ).size,
          observedCalls: observed.length,
          completed: observed.filter((i) => i.status === "completed").length,
          failed: observed.filter((i) => i.status === "failed").length,
          references: calls.filter(
            (i) => i.target === target && i.kind === "script-reference",
          ).length,
          lastUse: observed.length
            ? Math.max(...observed.map((i) => i.time))
            : null,
          usage: observed.length
            ? ("observed use" as const)
            : ("no observed use" as const),
          visibility: "incomplete" as const,
          newlyObserved:
            since === null || !definitions.some((c) => c.firstSeen < since),
          automaticUnused: false as const,
        };
      })
      .sort(
        (a, b) =>
          a.observedCalls - b.observedCalls || a.target.localeCompare(b.target),
      );
    const groups = new Set(
      [...components.map((c) => c.group), ...calls.map((i) => i.group)].filter(
        (g): g is string => g !== null,
      ),
    );
    const servers = [...groups].map((group) => {
      const ids = new Set(
        components
          .filter((c) => c.group === group && c.owner === "context")
          .map((c) => c.id),
      );
      const wire = views.filter((e) => e.components.some((id) => ids.has(id)));
      const candidate = views.filter((e) =>
        e.candidates.some((id) => ids.has(id)),
      );
      const used = calls.filter(
        (i) =>
          i.group === group &&
          ["mcp-call", "mcp-resource", "mcp-prompt"].includes(i.kind),
      );
      return {
        group,
        owners: [
          ...new Set(
            controls.flatMap((s) =>
              s.servers.filter((c) => c.group === group).map((c) => c.owner),
            ),
          ),
        ],
        exposedRequests: wire.length,
        candidateRequests: candidate.length,
        exposedSessions: new Set(
          wire.map((e) => requestById.get(e.request)!.session),
        ).size,
        observedCalls: used.length,
        completed: used.filter((i) => i.status === "completed").length,
        failed: used.filter((i) => i.status === "failed").length,
        lastUse: used.length ? Math.max(...used.map((i) => i.time)) : null,
        visibility: "incomplete" as const,
      };
    });
    const guidance: ContextCheckResult["guidance"] = [];
    const add = (
      kind: string,
      owner: string,
      message: string,
      change = false,
    ) =>
      guidance.push({
        kind,
        owner,
        message,
        action: change ? "change" : "advisory",
      });
    for (const state of controls) {
      const control = (key: string) =>
        state.controls.find((c) => c.key === key);
      for (const file of state.files) {
        if (
          profile.harness === "claude" &&
          /(?:^|[/\\])CLAUDE(?:\.local)?\.md$/.test(file.file) &&
          file.lines > 200
        )
          add(
            "instruction-budget",
            file.file,
            `${file.lines} current lines exceeds Claude's documented under-200-line target. Scope applicable instructions by path or move detailed workflows to on-demand skills; preserve required instructions. Current size does not prove historical exposure or cost.`,
            state.resolved,
          );
      }
      if (profile.harness === "codex") {
        const bytes = state.files.reduce((n, f) => n + f.bytes, 0);
        const cap = control("project_doc_max_bytes");
        if (cap && typeof cap.value === "number" && bytes > cap.value)
          add(
            "instruction-budget",
            cap.owner,
            `${bytes} current project instruction bytes exceeds the ${cap.value}-byte loading budget. Review nested scoping or raise project_doc_max_bytes to preserve required guidance; omitted content is not billed exposure.`,
            state.resolved,
          );
      }
      if (
        state.servers.length ||
        components.some((c) => c.category.startsWith("mcp-"))
      ) {
        const owner =
          profile.harness === "claude"
            ? "/mcp; ENABLE_TOOL_SEARCH; per-server alwaysLoad"
            : `${profile.settingsFile}:mcp_servers.<id>.enabled/omit_tools_from; apps.<id>.omit_tools_from`;
        add(
          "mcp-scope",
          owner,
          profile.harness === "claude"
            ? "Review no-observed-use servers with /mcp. ENABLE_TOOL_SEARCH and alwaysLoad control loading, subject to model/provider, HIPAA, experimental-beta policy, ToolSearch availability and gateway support. Permission deny restricts calls and does not establish removal of listings/schemas."
            : "Review server enabled and enabled_tools lists; omit_tools_from selects direct/deferred/code_mode exposure and combines with app omissions. disabled_tools blocks calls and does not prove listing removal. No retired tool-search switch is recommended.",
        );
      }
      const duplicates = new Map<string, ContextState["files"]>();
      for (const file of state.files.filter((f) => f.bytes > 0))
        duplicates.set(file.fingerprint, [
          ...(duplicates.get(file.fingerprint) ?? []),
          file,
        ]);
      for (const files of duplicates.values())
        if (files.length > 1)
          add(
            "duplicate-instructions",
            files.map((f) => f.file).join(", "),
            "Identical current instruction files occur in the selected chain. Review scope and precedence before consolidating; identical content does not establish redundant historical exposure or removable dollars.",
          );
      const search = control("ENABLE_TOOL_SEARCH");
      const relevantRequests = requests.filter((r) => r.cwd === state.cwd);
      const supported =
        relevantRequests.length > 0 &&
        relevantRequests.every((r) => {
          const model =
            /^claude-(opus|sonnet|haiku)-(\d+)(?:-(\d{1,2})(?:-|$))?/.exec(
              r.model ?? "",
            );
          if (!model || !r.version) return false;
          const generation = Number(model[2]) + Number(model[3] ?? 0) / 10;
          const provider = profile.provider ?? r.provider;
          if (
            provider === "azure" ||
            provider === "custom" ||
            provider === null
          )
            return false;
          const vertex = provider === "google-vertex-anthropic";
          return generation >= (vertex || model[1] === "haiku" ? 4.5 : 4);
        });
      if (components.some((c) => c.category.startsWith("mcp-")) && !supported)
        add(
          "tool-search-prerequisites",
          search?.owner ?? profile.settingsFile,
          "Model/provider support for deferral is unresolved or unavailable. Current Claude docs require Opus/Sonnet 4+ or Haiku 4.5+, and 4.5+ on Google Agent Platform; Foundry loads upfront. HIPAA and experimental-beta policy, denied/omitted ToolSearch and gateway compatibility can also prevent search. Do not enable deferral without verifying these prerequisites.",
        );
      if (
        profile.harness === "claude" &&
        supported &&
        (search?.value === "false" || state.servers.some((s) => s.alwaysLoad))
      )
        add(
          "deferred-loading",
          search?.owner ?? state.servers.find((s) => s.alwaysLoad)!.owner,
          "Upfront loading is configured for a compatible recorded model/provider. Compare full replacement discovery/schema requests before restoring deferral; a small frequently used toolset can cost less upfront. alwaysLoad=false requires Claude 2.1.287+; explicit policy, HIPAA, denied/omitted ToolSearch, gateways or request fallbacks can still prevent search. Verify current applicability; this is conditional guidance.",
        );
      for (const key of profile.harness === "claude"
        ? ["bashOutputMaxChars", "MAX_MCP_OUTPUT_TOKENS"]
        : ["tool_output_token_limit"]) {
        const cap = control(key);
        if (
          cap &&
          (cap.value !== null ||
            components.some((c) =>
              ["mcp-result", "tool-result"].includes(c.category),
            ))
        )
          add(
            "output-budget",
            cap.owner,
            `${key}=${cap.value ?? "unknown"}. Compare inline context with subsequent reads before changing the budget. Claude per-tool anthropic/maxResultSizeChars and successful-text persistence can supersede the MCP token cap. Output truncation is not proof of saved net work.`,
          );
      }
    }
    for (const c of components.filter((c) =>
      ["mcp-result", "tool-result"].includes(c.category),
    )) {
      if (c.characters === null) continue;
      const owners = controls.filter(
        (s) => s.cwd === null || requests.some((r) => r.cwd === s.cwd),
      );
      if (
        profile.harness === "claude" &&
        c.category === "mcp-result" &&
        c.characters > 50000
      )
        add(
          "output-overflow",
          c.source ?? c.locations[0]?.file ?? profile.settingsFile,
          `${c.characters} observed result characters exceeds Claude's default 50,000-character persistence threshold. A tool-specific anthropic/maxResultSizeChars limit or persisted-file preview may explain it; this is not a budget violation or evidence that all text was inline. Replacement reads and dollars remain unknown.`,
        );
      if (c.provenance === "observed-command-output-not-wire-proof")
        for (const state of owners) {
          const cap = state.controls.find(
            (c) => c.key === "bashOutputMaxChars",
          );
          if (
            cap &&
            typeof cap.value === "number" &&
            c.characters > Math.max(4000, Math.min(128000, cap.value))
          )
            add(
              "output-overflow",
              cap.owner,
              `${c.characters} observed output characters exceeds the configured Bash inline budget. Tool identity, truncation, and later reads must be linked before changing it; no lowest-cap recommendation or net dollars are inferred.`,
            );
        }
    }
    // The documented warning concerns raw MCP result tokens, not guessed tokens
    // from character counts or repeated context charges on later requests.
    if (profile.harness === "claude")
      for (const c of components.filter(
        (c) =>
          c.category === "mcp-result" &&
          c.recordedTokens !== null &&
          c.recordedTokens > 10000,
      ))
        add(
          "output-overflow",
          c.locations[0]?.file ?? profile.settingsFile,
          "Recorded MCP result exceeds Claude's documented 10,000-token warning. Inspect pagination and replacement reads; net dollars remain unknown.",
        );
    const changes = guidance.filter((g) => g.action === "change");
    const categories = [...new Set(attribution.map((c) => c.category))].map(
      (category) => ({
        category,
        revisions: attribution.filter((c) => c.category === category).length,
        wireRequests: views.filter((e) =>
          e.components.some(
            (id) => componentById.get(id)?.category === category,
          ),
        ).length,
        candidateRequests: views.filter((e) =>
          e.candidates.some(
            (id) => componentById.get(id)?.category === category,
          ),
        ).length,
      }),
    );
    return [
      {
        check: "loaded-context" as const,
        bucket: "all" as const,
        profile: profile.id,
        historyProfiles: [...new Set(requests.flatMap((r) => r.profiles))],
        evidence: "guidance" as const,
        action: changes.length ? ("change" as const) : ("advisory" as const),
        current: changes.length
          ? "documented instruction budget exceeded"
          : null,
        recommended: changes.length
          ? "review instruction scope and documented loading budget"
          : null,
        owner: changes.length
          ? [...new Set(changes.map((g) => g.owner))].join(", ")
          : profile.settingsFile,
        reason: changes.length
          ? "Current effective instruction sources exceed a documented provider budget; dollars unknown."
          : "Attribution and conditional guidance; complete exposure/call visibility or current removable net cost is not established.",
        since,
        until: now,
        windowBasis:
          options.since === undefined
            ? "available selected history; no component-setting transition established; current files are separate evidence"
            : "explicit --since; current files are separate evidence",
        baseline:
          "recorded/current profile context, holding other settings and work fixed; independent of skills, TTL, window, idle and speed checks",
        requests: requests.length,
        savingsUsd: null,
        monthlySavingsUsd: null,
        breakEven: null,
        attribution,
        categories,
        tools,
        servers,
        guidance,
        controls,
        sharedSkills: components
          .filter((c) => c.owner === "skills")
          .map((c) => ({
            component: c.id,
            group: c.group,
            owner: "skills" as const,
          })),
        usageEstimates: [],
        assumptions: [
          "wire-only marginal attribution reuses calibrated component sizes and full-request cache/context-tier repricing; allocation sensitivity is not a guaranteed bound",
          "discovery listings, loaded schemas, server instructions, tool results and instruction sources are separate categories; hashes never supply token sizes",
          "no automatic unused classification: supported native histories do not guarantee complete direct/nested call and exposure visibility throughout the window; newly observed and rare use remain advisory",
          "actual nested completion events count execution; script references alone never count use or success",
          "gross overhead is not removable net savings; no summed independent component/check savings",
          "skill listings and bodies belong to the skills check; shared plugin context is charged only to its context component, with group links for skills",
        ],
        uncertainty: [
          "native history does not guarantee every loaded file, app/plugin/managed instruction, schema, hook or output is recorded",
          "cache invalidation, search calls, catalog backfill and extra reads prevent a net-dollar recommendation without a linked counterfactual",
          ...controls.filter((c) => !c.resolved).map((c) => c.reason),
        ],
        sources: SOURCES,
      },
    ];
  });
}
