import { readdir, readFile, realpath, stat } from "node:fs/promises";
import { basename, join } from "node:path";
import type { Size } from "./exposure.js";
import {
  estimateTokens,
  measureSize,
  priceExposureChange,
} from "./exposure.js";
import type { CheckBase, Cost, Harness, Report, Scan } from "./model.js";
import { addCosts, unknownCost, type Catalog } from "./prices.js";
import { identity, label, object, ScanError } from "./safe.js";

const SOURCES = [
  "https://code.claude.com/docs/en/skills (2026-11-07)",
  "https://code.claude.com/docs/en/settings-reference#skilloverrides (2026-11-07)",
  "https://code.claude.com/docs/en/settings-reference#skilllistingbudgetfraction (2026-11-07)",
  "https://code.claude.com/docs/en/settings-reference#skilllistingmaxdescchars (2026-11-07)",
  "https://learn.chatgpt.com/docs/build-skills (2026-11-07)",
  "https://learn.chatgpt.com/docs/config-file/config-reference (2026-11-07)",
];
type Scope = { config: Record<string, unknown>; file: string };
type Visibility = "on" | "name-only" | "user-invocable-only" | "off";
/** Current files are ownership evidence, never a historical catalog or introduction date. */
export interface SkillState {
  resolved: boolean;
  reason: string;
  budgets: {
    key: string;
    value: number | null;
    owner: string;
    violation: boolean;
    recommended: number | null;
  }[];
  sources: {
    name: string;
    path: string;
    canonical: string;
    targets: string[];
    fingerprint: string;
    description: string | null;
    visibility: Visibility;
    owner: string;
    effective: boolean | null;
  }[];
  plugins: { group: string; owner: string; enabled: boolean }[];
}
/** One skill ranking or a current budget finding, using the common exit policy. */
export interface SkillsCheckResult extends CheckBase<string> {
  check: "skills-catalog";
  bucket: "catalog" | "skill";
  reason: string;
  target: string | null;
  name: string | null;
  revisions: string[];
  duplicates: string[];
  currentResolved: boolean;
  budgets: SkillState["budgets"];
  versions: (string | null)[];
  budgetPressure: boolean;
  introduced: "unknown" | "observed-during-window";
  firstObserved: number | null;
  exposure: {
    wireRequests: number;
    candidateRequests: number;
    sessions: number;
    wireSessions: number;
    candidateSessions: number;
    completeWindow: boolean;
  };
  use: {
    requested: number;
    calls: number;
    loads: number;
    successfulCalls: number;
    successfulLoads: number;
    references: number;
    sessions: number;
    rate: number | null;
    last: number | null;
    completeWindow: boolean;
  };
  listing: {
    characters: number;
    gross: Cost;
    monthlyGrossUsd: number | null;
    grossRangeUsd: [number, number] | null;
  };
  bodies: {
    revisions: string[];
    wireRequests: number;
    candidateRequests: number;
    gross: Cost;
  };
  marginal: {
    request: string;
    before: string;
    after: string | null;
    beforeTokens: number | null;
    afterTokens: number | null;
    net: Cost;
    rangeUsd: [number, number] | null;
  }[];
  usageEstimates: [];
}

function scalar(header: string, key: string): string | null {
  const value = new RegExp(`^${key}:\\s*([^\\n]*)$`, "m")
    .exec(header)?.[1]
    ?.trim();
  if (!value || /^[>|]/.test(value)) return null;
  if (value.startsWith('"')) {
    try {
      const parsed: unknown = JSON.parse(value);
      return typeof parsed === "string" ? parsed : null;
    } catch {
      return null;
    }
  }
  return value.startsWith("'") && value.endsWith("'")
    ? value.slice(1, -1).replaceAll("''", "'")
    : value;
}

/** Resolve supported local skill controls and fingerprints without retaining instruction text. */
export async function skillState(input: {
  harness: Harness;
  scopes: Scope[];
  roots: string[];
  env: NodeJS.ProcessEnv;
  resolved: boolean;
  reason: string;
  signal?: AbortSignal | undefined;
}): Promise<SkillState> {
  const { harness, scopes } = input;
  const state: SkillState = {
    resolved: input.resolved,
    reason: input.reason,
    budgets: [],
    sources: [],
    plugins: [],
  };
  const effective = (key: string) =>
    [...scopes].reverse().find((s) => Object.hasOwn(s.config, key));
  for (const [key, fallback, maximum] of harness === "claude"
    ? ([
        ["skillListingBudgetFraction", 0.01, 1],
        ["skillListingMaxDescChars", 1536, null],
      ] as const)
    : ([["skills.max_context_tokens", null, 10000]] as const)) {
    const owner =
      harness === "claude"
        ? effective(key)
        : [...scopes]
            .reverse()
            .find((s) =>
              Object.hasOwn(object(s.config.skills), "max_context_tokens"),
            );
    const raw =
      harness === "claude"
        ? owner?.config[key]
        : object(owner?.config.skills).max_context_tokens;
    if (raw !== undefined && (typeof raw !== "number" || !Number.isFinite(raw)))
      throw new ScanError("invalid skill budget", {
        file: owner!.file,
        line: 1,
      });
    const value = raw === undefined ? fallback : (raw as number);
    const invalid =
      value !== null &&
      (value <= 0 ||
        (key !== "skillListingBudgetFraction" &&
          !Number.isSafeInteger(value)) ||
        (maximum !== null && value > maximum));
    state.budgets.push({
      key,
      value,
      owner: `${owner?.file ?? scopes[0]!.file}:${key}`,
      violation: invalid,
      recommended: invalid ? (maximum ?? fallback) : null,
    });
  }
  if (harness === "claude") {
    const scoped = [...scopes]
      .reverse()
      .find((s) =>
        Object.hasOwn(object(s.config.env), "SLASH_COMMAND_TOOL_CHAR_BUDGET"),
      );
    const raw =
      input.env.SLASH_COMMAND_TOOL_CHAR_BUDGET ??
      object(scoped?.config.env).SLASH_COMMAND_TOOL_CHAR_BUDGET;
    if (raw !== undefined) {
      const value =
        typeof raw === "string" && /^\d+$/.test(raw) ? Number(raw) : null;
      state.budgets.push({
        key: "SLASH_COMMAND_TOOL_CHAR_BUDGET",
        value,
        owner:
          input.env.SLASH_COMMAND_TOOL_CHAR_BUDGET === undefined
            ? `${scoped!.file}:env.SLASH_COMMAND_TOOL_CHAR_BUDGET`
            : "environment:SLASH_COMMAND_TOOL_CHAR_BUDGET",
        violation: false,
        recommended: null,
      });
      if (value === null || value <= 0 || !Number.isSafeInteger(value))
        state.resolved = false;
    }
  }
  const plugins = new Map<
    string,
    { group: string; owner: string; enabled: boolean }
  >();
  for (const scope of scopes) {
    const rows = object(scope.config.skills).config;
    if (harness === "codex" && rows !== undefined) {
      if (!Array.isArray(rows))
        throw new ScanError("invalid skill configuration", {
          file: scope.file,
          line: 1,
        });
      for (const row of rows) {
        const entry = object(row);
        if (
          (typeof entry.name === "string") ===
            (typeof entry.path === "string") ||
          (entry.enabled !== undefined && typeof entry.enabled !== "boolean")
        )
          throw new ScanError("invalid skill selector", {
            file: scope.file,
            line: 1,
          });
      }
    }
    const entries =
      harness === "claude"
        ? object(scope.config.enabledPlugins)
        : object(scope.config.plugins);
    for (const [key, entry] of Object.entries(entries)) {
      const enabled = harness === "claude" ? entry : object(entry).enabled;
      if (typeof enabled === "boolean" && key !== "_default" && label(key))
        plugins.set(key, {
          group: identity(`plugin:${key.split("@")[0]}`),
          enabled,
          owner: `${scope.file}:${harness === "claude" ? "enabledPlugins" : "plugins"}.${key}${harness === "codex" ? ".enabled" : ""}`,
        });
    }
  }
  state.plugins = [...plugins.values()];
  const sourceRoots = new Map<string, number>();
  let rootIndex = 0;
  async function visit(path: string, ancestors: Set<string>) {
    input.signal?.throwIfAborted();
    let canonical: string;
    try {
      canonical = await realpath(path);
      if (!(await stat(path)).isDirectory()) return;
    } catch (error) {
      if (object(error).code === "ENOENT") return;
      throw new ScanError("cannot resolve skill source", {
        file: path,
        line: 0,
      });
    }
    if (ancestors.has(canonical)) return;
    const next = new Set([...ancestors, canonical]);
    let entries;
    try {
      entries = await readdir(path, { withFileTypes: true });
    } catch {
      throw new ScanError("cannot inspect skill source", {
        file: path,
        line: 0,
      });
    }
    if (
      entries.some(
        (e) => e.name === "SKILL.md" && (e.isFile() || e.isSymbolicLink()),
      )
    ) {
      const file = join(path, "SKILL.md");
      let content: string;
      try {
        content = await readFile(file, "utf8");
      } catch {
        throw new ScanError("cannot read skill metadata", { file, line: 1 });
      }
      const header =
        /^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)/.exec(content)?.[1] ?? "";
      const name = label(
        scalar(header, "name") ??
          (harness === "claude" ? basename(path) : null),
      );
      if (!name) return; // Valid YAML outside scalar metadata is unresolved, not an invented name.
      const description = scalar(header, "description");
      let visibility: Visibility =
        harness === "claude" &&
        scalar(header, "disable-model-invocation") === "true"
          ? "user-invocable-only"
          : "on";
      let owner =
        harness === "claude"
          ? `${file}:disable-model-invocation`
          : `${scopes[0]!.file}:skills.config`;
      for (const scope of scopes) {
        if (harness === "claude") {
          const value = object(scope.config.skillOverrides)[name];
          if (value !== undefined) {
            if (
              !["on", "off", "name-only", "user-invocable-only"].includes(
                String(value),
              )
            )
              throw new ScanError("invalid skill visibility", {
                file: scope.file,
                line: 1,
              });
            visibility = value as Visibility;
            owner = `${scope.file}:skillOverrides.${name}`;
          }
        } else {
          const rows = object(scope.config.skills).config;
          if (rows !== undefined && !Array.isArray(rows))
            throw new ScanError("invalid skill configuration", {
              file: scope.file,
              line: 1,
            });
          for (const [index, row] of (
            (rows as unknown[] | undefined) ?? []
          ).entries()) {
            const entry = object(row);
            if (
              (entry.path === file || entry.name === name) &&
              typeof entry.enabled === "boolean"
            ) {
              visibility = entry.enabled ? "on" : "off";
              owner = `${scope.file}:skills.config[${index}].enabled`;
            }
          }
        }
      }
      const actual = await realpath(file);
      sourceRoots.set(file, rootIndex);
      state.sources.push({
        name,
        path: file,
        canonical: actual,
        targets: [
          identity(`skill:${name}`),
          identity(`skill:${file}`),
          identity(`skill:${actual}`),
        ],
        fingerprint: measureSize({ text: content }).fingerprint,
        description: description === null ? null : identity(description),
        visibility,
        owner,
        effective: true,
      });
      return;
    }
    for (const entry of entries.sort((a, b) => a.name.localeCompare(b.name)))
      if (
        !entry.name.startsWith(".") &&
        (entry.isDirectory() || entry.isSymbolicLink())
      )
        await visit(join(path, entry.name), next);
  }
  const seenRoots = new Set<string>();
  for (const [index, root] of input.roots.entries()) {
    if (seenRoots.has(root)) continue;
    seenRoots.add(root);
    rootIndex = index;
    await visit(root, new Set());
  }
  if (harness === "claude") {
    // Roots arrive enterprise, personal, then project. Claude deduplicates
    // symlink targets and prefers enterprise/personal over the project copy.
    const seenPaths = new Set<string>();
    const seenNames = new Map<string, SkillState["sources"][number]>();
    for (const source of state.sources) {
      const prior = seenNames.get(source.name);
      source.effective = !seenPaths.has(source.canonical);
      if (source.effective && prior) {
        // The first two roots are enterprise and personal. Two project/nested
        // aliases need qualified-name evidence that scalar frontmatter lacks.
        if (
          sourceRoots.get(prior.path)! < sourceRoots.get(source.path)! &&
          sourceRoots.get(prior.path)! < 2
        )
          source.effective = false;
        else {
          source.effective = null;
          prior.effective = null;
        }
      }
      seenPaths.add(source.canonical);
      if (!prior) seenNames.set(source.name, source);
    }
  }
  return state;
}

/** Rank observed skills and price actual catalog substitutions through the shared pricing path. */
export function skillsChecks(input: {
  scan: Scan;
  catalog: Catalog;
  exposure: Report["exposure"];
  since: number | undefined;
  now: number;
}): SkillsCheckResult[] {
  const { scan, catalog, exposure, now } = input;
  const components = new Map(scan.exposure.components.map((c) => [c.id, c]));
  const views = new Map(scan.exposure.requests.map((e) => [e.request, e]));
  const priced = new Map(exposure.requests.map((r) => [r.request, r]));
  const result: SkillsCheckResult[] = [];
  for (const profile of scan.profiles) {
    const selected = scan.requests.filter(
      (r) =>
        r.profiles.includes(profile.id) &&
        (input.since === undefined || r.time >= input.since) &&
        r.time <= now,
    );
    const groups = new Map<string, typeof selected>();
    for (const r of selected) {
      const key = r.cwd ?? "";
      const rows = groups.get(key) ?? [];
      rows.push(r);
      groups.set(key, rows);
    }
    if (!groups.size) groups.set("", []);
    for (const [cwd, requests] of groups) {
      const state =
        (cwd ? profile.skillDirectories?.[cwd] : undefined) ?? profile.skills;
      if (!state) continue;
      const sessionIds = new Set(requests.map((r) => r.session));
      const requestStart = requests.reduce<number | null>(
        (start, r) => (start === null ? r.time : Math.min(start, r.time)),
        null,
      );
      const since =
        input.since ??
        scan.exposure.invocations.reduce<number | null>(
          (start, i) =>
            i.profiles.includes(profile.id) &&
            i.sessions.some((s) => sessionIds.has(s)) &&
            i.time <= now
              ? start === null
                ? i.time
                : Math.min(start, i.time)
              : start,
          requestStart,
        );
      const base = (): SkillsCheckResult => ({
        check: "skills-catalog",
        bucket: "skill",
        profile: profile.id,
        historyProfiles: [...new Set(requests.flatMap((r) => r.profiles))],
        evidence: "guidance",
        action: "advisory",
        current: null,
        recommended: null,
        owner: profile.settingsFile,
        since,
        until: now,
        windowBasis:
          input.since === undefined
            ? "available selected requests; current files applied to history; no settings-change history"
            : "explicit --since; current files resolved separately from historical catalogs",
        baseline:
          "recorded catalog at current profile defaults, holding all other settings and work constant; independent of other checks",
        requests: requests.length,
        savingsUsd: null,
        monthlySavingsUsd: null,
        breakEven: null,
        reason: "",
        target: null,
        name: null,
        revisions: [],
        duplicates: [],
        currentResolved: false,
        budgets: state.budgets,
        budgetPressure: false,
        versions: [...new Set(requests.map((r) => r.version))],
        introduced: "unknown",
        firstObserved: null,
        exposure: {
          wireRequests: 0,
          candidateRequests: 0,
          sessions: 0,
          wireSessions: 0,
          candidateSessions: 0,
          completeWindow: false,
        },
        use: {
          requested: 0,
          calls: 0,
          loads: 0,
          successfulCalls: 0,
          successfulLoads: 0,
          references: 0,
          sessions: 0,
          rate: null,
          last: null,
          completeWindow: false,
        },
        listing: {
          characters: 0,
          gross: unknownCost("no priced rendered listings"),
          monthlyGrossUsd: null,
          grossRangeUsd: null,
        },
        bodies: {
          revisions: [],
          wireRequests: 0,
          candidateRequests: 0,
          gross: unknownCost("no attributed body exposure"),
        },
        marginal: [],
        usageEstimates: [],
        assumptions: [
          "gross exposure is not removable waste; fixed behavior excludes unmeasured replacement work",
          "skill bodies and references remain on demand and are never removed with listing metadata",
          "shared plugin instructions/tools/results belong to the context check; plugin overhead is not counted here",
          "no summed independent savings; API-price equivalents are not subscription bills",
        ],
        uncertainty: [
          state.reason,
          "no supported client/version proves complete invocation visibility throughout the window",
          "first observation is not installation time; recent introduction cannot be ruled out",
          "shortened descriptions, omission and budget backfill require observed replacement catalogs; unknown is not zero",
        ],
        sources: SOURCES,
      });
      for (const budget of state.budgets.filter((b) => b.violation)) {
        const row = base();
        Object.assign(row, {
          bucket: "catalog",
          current: String(budget.value),
          recommended: String(budget.recommended),
          owner: budget.owner,
          currentResolved: state.resolved,
          action: state.resolved ? "change" : "advisory",
          reason: `${budget.key} violates its documented positive ${budget.key === "skillListingBudgetFraction" ? "fraction at most 1" : budget.key === "skills.max_context_tokens" ? "integer maximum of 10000" : "integer range"}; dollars unknown`,
        });
        result.push(row);
      }
      const catalogs = [
        ...new Set(
          requests.flatMap((r) => {
            const e = views.get(r.id);
            return e ? [...e.components, ...e.candidates] : [];
          }),
        ),
      ]
        .map((id) => components.get(id)!)
        .filter(
          (c) => c.category === "skill-listing" && c.scope === "aggregate",
        );
      const targets = new Set(
        catalogs.flatMap((c) =>
          c.members
            .map(
              (id) =>
                components.get(id)!.skill?.path ??
                components.get(id)!.skill?.name,
            )
            .filter((id): id is string => !!id),
        ),
      );
      const entries = [...new Set(catalogs.flatMap((c) => c.members))].map(
        (id) => components.get(id)!,
      );
      const nameTargets = new Map<string, Set<string>>();
      for (const entry of entries) {
        if (!entry.skill) continue;
        const names = nameTargets.get(entry.skill.name) ?? new Set<string>();
        names.add(entry.skill.path ?? entry.skill.name);
        nameTargets.set(entry.skill.name, names);
      }
      // Manual/hidden skills can load bodies without ever appearing in a listing.
      // An ambiguous name keeps its own unallocated usage row instead of being lost.
      for (const invocation of scan.exposure.invocations) {
        if (
          !invocation.kind.startsWith("skill-") ||
          invocation.kind === "skill-reference" ||
          !invocation.profiles.includes(profile.id) ||
          !invocation.sessions.some((s) => sessionIds.has(s)) ||
          (since !== null && invocation.time < since) ||
          invocation.time > now
        )
          continue;
        if (
          !targets.has(invocation.target) &&
          nameTargets.get(invocation.target)?.size !== 1
        )
          targets.add(invocation.target);
      }
      for (const target of targets) {
        const row = base();
        row.target = target;
        const members = entries.filter(
          (c) =>
            c.category === "skill-listing" &&
            c.scope === "component" &&
            (c.skill?.path ?? c.skill?.name) === target,
        );
        const aliases = new Set(members.flatMap((c) => c.targets));
        const names = new Set(
          members.flatMap((c) => (c.skill ? [c.skill.name] : [])),
        );
        // A path match is stronger than a colliding name. Never allocate one name's use to several paths.
        const uniqueNames = [...names].filter(
          (name) => nameTargets.get(name)?.size === 1,
        );
        const useTargets = new Set([target, ...uniqueNames]);
        const directSources = state.sources.filter(
          (s) => identity(`skill:${s.path}`) === target,
        );
        const sources = directSources.length
          ? directSources
          : state.sources.filter((s) => s.targets.includes(target));
        const pluginGroup =
          members[0]?.group ??
          scan.exposure.invocations.find(
            (i) => i.target === target && i.profiles.includes(profile.id),
          )?.group;
        const plugin = pluginGroup
          ? state.plugins.filter((p) => p.group === pluginGroup)
          : [];
        const source =
          sources.length === 1 && !pluginGroup ? sources[0] : undefined;
        row.name = source?.name ?? null;
        row.owner =
          source?.owner ??
          (plugin.length === 1
            ? plugin[0]!.owner
            : pluginGroup
              ? `${profile.settingsFile}:plugin control (/plugin for Claude; plugins.<plugin>.enabled for Codex)`
              : `${profile.settingsFile}:${profile.harness === "claude" ? "skillOverrides" : "skills.config"}`);
        row.current =
          source?.visibility ??
          (plugin.length === 1 ? String(plugin[0]!.enabled) : null);
        row.recommended =
          profile.harness === "claude"
            ? pluginGroup
              ? "review /plugin; preserve required plugin tools"
              : "consider name-only, user-invocable-only, off, or project scope"
            : "consider per-skill enabled=false or project scope; implicit invocation policy alone has no established catalog saving";
        row.currentResolved = state.resolved && source?.effective === true;
        if (source?.effective === false) {
          const winner = state.sources.find(
            (s) => s.name === source.name && s.effective === true,
          );
          row.owner = winner?.owner ?? row.owner;
          row.current = winner?.visibility ?? null;
          row.uncertainty.push(
            "historical skill source is deduplicated or shadowed by the current effective source; historical-only finding stays advisory",
          );
        }
        row.revisions = members.map((m) => m.id);
        row.firstObserved = members.length
          ? Math.min(...members.map((m) => m.firstSeen))
          : null;
        const seenSessions = new Set<string>();
        const usedSessions = new Set<string>();
        const wireSessions = new Set<string>(),
          candidateSessions = new Set<string>();
        const costs: Cost[] = [],
          bodyCosts: Cost[] = [];
        const ranges: [number, number][] = [];
        for (const r of requests) {
          const e = views.get(r.id);
          if (!e) continue;
          const exposed = [...e.components, ...e.candidates]
            .map((id) => components.get(id)!)
            .filter(
              (c) =>
                c.category === "skill-listing" &&
                c.members.some((id) => row.revisions.includes(id)),
            );
          if (exposed.length) {
            seenSessions.add(r.session);
            if (exposed.some((c) => e.components.includes(c.id))) {
              row.exposure.wireRequests++;
              wireSessions.add(r.session);
            } else {
              row.exposure.candidateRequests++;
              candidateSessions.add(r.session);
            }
          }
          for (const c of exposed) {
            row.budgetPressure ||= c.catalog?.budgetExceeded === true;
            const spans = c.members
              .map((id) => components.get(id)!)
              .filter((m) => row.revisions.includes(m.id));
            row.listing.characters += spans.reduce(
              (sum, m) => sum + (m.characters ?? 0),
              0,
            );
            for (const other of c.members.map((id) => components.get(id)!))
              if (
                !row.revisions.includes(other.id) &&
                spans.some(
                  (m) =>
                    m.skill?.name === other.skill?.name &&
                    m.skill?.description === other.skill?.description,
                )
              )
                row.duplicates.push(other.id);
            if (new Set(spans.map((m) => m.id)).size !== spans.length)
              row.duplicates.push(c.id);
            const measurement = priced
              .get(r.id)
              ?.components.find((m) => m.component === c.id);
            const calibration = exposure.calibration.find(
              (k) =>
                measurement?.tokens.basis ===
                `empirical calibration ${k.cohort}`,
            );
            const size: Size = {
              characters: spans.reduce((n, m) => n + (m.characters ?? 0), 0),
              recordedTokens: null,
              fingerprint: "",
            };
            const tokens = estimateTokens(size, calibration).estimate;
            const supported =
              e.components.includes(c.id) &&
              tokens !== null &&
              r.parts.length === 1 &&
              r.parts[0]!.usage.input !== null &&
              tokens <= r.parts[0]!.usage.input!;
            const gross = supported
              ? priceExposureChange({
                  catalog,
                  request: r,
                  removedTokens: tokens,
                  addedTokens: 0,
                })
              : null;
            costs.push(
              gross?.gross ??
                unknownCost(
                  "listing span is candidate-only or lacks a supported token calibration",
                ),
            );
            if (gross?.range) ranges.push(gross.range);
            const after = catalogs
              .filter(
                (a) =>
                  a.key === c.key &&
                  a.firstSeen > c.firstSeen &&
                  a.members.length &&
                  !a.members.some((id) => {
                    const m = components.get(id)!;
                    return (m.skill?.path ?? m.skill?.name) === target;
                  }) &&
                  c.members
                    .filter((id) => !row.revisions.includes(id))
                    .every((id) =>
                      a.members.some((next) =>
                        components
                          .get(next)!
                          .targets.some((t) =>
                            components.get(id)!.targets.includes(t),
                          ),
                      ),
                    ),
              )
              .sort((a, b) => a.firstSeen - b.firstSeen)[0];
            const beforeTokens = measurement?.tokens.estimate ?? null;
            const afterTokens = after
              ? estimateTokens(after, calibration).estimate
              : null;
            const change =
              e.components.includes(c.id) &&
              beforeTokens !== null &&
              afterTokens !== null &&
              beforeTokens <= (r.parts[0]?.usage.input ?? -1)
                ? priceExposureChange({
                    catalog,
                    request: r,
                    removedTokens: beforeTokens,
                    addedTokens: afterTokens,
                  })
                : null;
            row.marginal.push({
              request: r.id,
              before: c.id,
              after: after?.id ?? null,
              beforeTokens,
              afterTokens,
              net:
                change?.fixedBehaviorNet ??
                unknownCost(
                  "no supported observed before/after catalog substitution; budget backfill is unknown",
                ),
              rangeUsd: change?.range ?? null,
            });
          }
          for (const id of [...e.components, ...e.candidates]) {
            const c = components.get(id)!;
            if (
              c.category !== "skill-body" ||
              !c.targets.some((t) => useTargets.has(t))
            )
              continue;
            row.bodies.revisions.push(id);
            if (e.components.includes(id)) row.bodies.wireRequests++;
            else row.bodies.candidateRequests++;
            bodyCosts.push(
              priced.get(r.id)?.components.find((m) => m.component === id)
                ?.gross ?? unknownCost("body has no wire attribution"),
            );
          }
        }
        const sessions = new Set(requests.map((r) => r.session));
        const calls = scan.exposure.invocations.filter(
          (i) =>
            i.profiles.includes(profile.id) &&
            i.sessions.some((s) => sessions.has(s)) &&
            useTargets.has(i.target) &&
            (since === null || i.time >= since) &&
            i.time <= now,
        );
        for (const call of calls) {
          if (call.kind === "skill-request") row.use.requested++;
          else if (call.kind === "skill-call") {
            row.use.calls++;
            if (call.status === "completed") row.use.successfulCalls++;
          } else if (call.kind === "skill-load") {
            row.use.loads++;
            if (call.status === "completed") row.use.successfulLoads++;
          } else {
            row.use.references++;
            continue;
          }
          call.sessions
            .filter((s) => sessionIds.has(s))
            .forEach((s) => usedSessions.add(s));
          row.use.last = Math.max(row.use.last ?? -Infinity, call.time);
        }
        row.exposure.sessions = seenSessions.size;
        row.use.sessions = usedSessions.size;
        row.exposure.wireSessions = wireSessions.size;
        row.exposure.candidateSessions = candidateSessions.size;
        row.use.rate = seenSessions.size
          ? [...usedSessions].filter((s) => seenSessions.has(s)).length /
            seenSessions.size
          : null;
        row.listing.gross = costs.length ? addCosts(costs) : row.listing.gross;
        row.listing.monthlyGrossUsd =
          row.listing.gross.totalUsd !== null && since !== null && now > since
            ? (row.listing.gross.totalUsd * 30 * 86400000) / (now - since)
            : null;
        row.listing.grossRangeUsd =
          costs.length && ranges.length === costs.length
            ? [
                ranges.reduce((n, r) => n + r[0], 0),
                ranges.reduce((n, r) => n + r[1], 0),
              ]
            : null;
        row.bodies.gross = bodyCosts.length
          ? addCosts(bodyCosts)
          : row.bodies.gross;
        row.bodies.revisions = [...new Set(row.bodies.revisions)];
        row.duplicates = [...new Set(row.duplicates)];
        const observedUse = row.use.requested + row.use.calls + row.use.loads;
        row.reason = `${observedUse ? `${row.use.requested} requested, ${row.use.calls} calls, ${row.use.loads} loads` : "no observed use"} during ${seenSessions.size} exposed/candidate sessions; ${row.duplicates.length ? "duplicate rendered entries; " : ""}${row.budgetPressure ? "recorded catalog budget warning; " : ""}rank by counts, exposed-session rate and window; incomplete visibility is advisory`;
        const currentDuplicate =
          profile.harness === "codex" &&
          row.duplicates.length &&
          row.currentResolved &&
          source &&
          source.visibility === "on" &&
          members.some((m) => m.skill?.description === source.description) &&
          state.sources.some(
            (s) =>
              s.path !== source.path &&
              s.visibility === "on" &&
              s.effective === true &&
              s.name === source.name &&
              s.fingerprint === source.fingerprint &&
              [...aliases].some((a) => s.targets.includes(a)),
          );
        if (currentDuplicate) {
          row.action = "change";
          row.recommended =
            "remove duplicate skill source from catalog; preserve one effective copy";
          row.reason =
            "duplicate rendered listing matches duplicate current skill sources; provider-guidance change, net dollars unknown";
        }
        if (row.marginal.length)
          row.uncertainty.push(
            "observed replacement catalogs are conditional substitutions, not proof that this control caused the change; all replacement text is charged as ordinary input; no causal or future cache saving inferred",
          );
        if (uniqueNames.length !== names.size)
          row.uncertainty.push(
            "name collision: name-only invocations cannot be assigned to one source; path evidence only",
          );
        // Native visibility deliberately never authorizes automatic unused changes.
        // A firstSeen timestamp alone cannot remove the recent-introduction caveat.
        result.push(row);
      }
      if (!targets.size && requests.length) {
        const row = base();
        row.bucket = "catalog";
        row.reason =
          "no identifiable rendered skill entries; installed skills do not prove historical exposure or zero use/cost";
        result.push(row);
      }
    }
  }
  return result.sort(
    (a, b) =>
      (a.use.rate ?? Infinity) - (b.use.rate ?? Infinity) ||
      b.listing.gross.knownUsd - a.listing.gross.knownUsd ||
      (a.target ?? "").localeCompare(b.target ?? ""),
  );
}
