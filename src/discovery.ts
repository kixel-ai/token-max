import { readdir, realpath, stat, readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { parse as parseToml } from "smol-toml";
import type {
  Harness,
  Profile,
  Request,
  RootStatus,
  Snapshot,
  Ttl,
} from "./model.js";
import { identity, label, object, ScanError } from "./safe.js";
import {
  compactionControls,
  idleControls,
  scopedEnvironment,
  speedControls,
} from "./controls.js";

/** Explicit roots limit the entire scan; otherwise discover host-local profiles. */
export interface DiscoveryOptions {
  signal?: AbortSignal;
  home?: string;
  env?: NodeJS.ProcessEnv;
  claudeDirs?: string[];
  codexDirs?: string[];
  /** Override the system managed file for host adapters and synthetic tests. */
  managedSettingsFile?: string;
  /** Override Codex system files for host adapters and synthetic tests. */
  codexSystemConfigFile?: string;
  codexRequirementsFile?: string;
}

function missing(error: unknown): boolean {
  return object(error).code === "ENOENT";
}

async function settings(
  file: string,
  harness: Harness,
): Promise<Record<string, unknown>> {
  let text: string;
  try {
    text = await readFile(file, "utf8");
  } catch (error) {
    if (missing(error)) return {};
    throw new ScanError("cannot read settings", { file, line: 1 });
  }
  try {
    const value: unknown =
      harness === "claude" ? JSON.parse(text) : parseToml(text);
    if (value === null || typeof value !== "object" || Array.isArray(value))
      throw new Error();
    return object(value);
  } catch {
    throw new ScanError("invalid settings file", { file, line: 1 });
  }
}

function ttl(value: unknown): Ttl | null {
  return value === "5m" || value === "1h" ? value : null;
}

/** Documented system file; other managed delivery mechanisms remain unobserved. */
export function managedSettingsPath(
  platform: NodeJS.Platform = process.platform,
): string {
  if (platform === "darwin")
    return "/Library/Application Support/ClaudeCode/managed-settings.json";
  if (platform === "win32")
    return "C:\\Program Files\\ClaudeCode\\managed-settings.json";
  return "/etc/claude-code/managed-settings.json";
}

function controls(options: {
  config: Record<string, unknown>;
  env: NodeJS.ProcessEnv;
  file: string;
  managed: Record<string, unknown>;
  managedFile: string;
  scopes?: { config: Record<string, unknown>; file: string }[];
}): Profile["ttl"] {
  const { config, env, file, managed, managedFile } = options;
  const scopes = options.scopes ?? [
    { config, file },
    { config: managed, file: managedFile },
  ];
  const settingOwner = (key: string) =>
    ([...scopes].reverse().find((s) => Object.hasOwn(s.config, key))?.file ??
      file) +
    ":" +
    key;
  const { values, owners } = scopedEnvironment({ scopes, env });
  const envOwner = (key: string) => owners[key] ?? `${file}:env.${key}`;
  const result = {} as Profile["ttl"];
  for (const bucket of ["main", "subagent"] as const) {
    const variable =
      bucket === "main"
        ? "CLAUDE_CODE_PROMPT_CACHE_TTL"
        : "CLAUDE_CODE_SUBAGENT_PROMPT_CACHE_TTL";
    const key = bucket === "main" ? "promptCacheTtl" : "subagentPromptCacheTtl";
    let value: Ttl | null = null;
    let owner = settingOwner(key);
    let reason =
      "billing state is unknown; main default can change from included subscription usage to credits";
    if (values.DISABLE_PROMPT_CACHING === "1") {
      reason = "prompt caching is disabled";
    } else if (values.FORCE_PROMPT_CACHING_5M === "1") {
      value = "5m";
      owner = envOwner("FORCE_PROMPT_CACHING_5M");
      reason = "environment override";
    } else if (ttl(values[variable])) {
      value = ttl(values[variable]);
      owner = envOwner(variable);
      reason = "environment override";
    } else if (ttl(config[key])) {
      value = ttl(config[key]);
      reason = "current user/managed setting";
    } else if (values.ENABLE_PROMPT_CACHING_1H === "1") {
      value = "1h";
      owner = envOwner("ENABLE_PROMPT_CACHING_1H");
      reason = "environment override; agent frontmatter may take precedence";
    } else if (bucket === "subagent") {
      value = "5m";
      reason =
        "documented default; agent frontmatter and server helpers may override";
    } else if (
      values.CLAUDE_CODE_USE_BEDROCK === "1" ||
      values.CLAUDE_CODE_USE_VERTEX === "1" ||
      values.CLAUDE_CODE_USE_FOUNDRY === "1"
    ) {
      value = "5m";
      reason = "explicit cloud-provider billing default";
    }
    result[bucket] = { value, owner, reason };
  }
  return result;
}

/** Retain only settings that current or planned checks consume. */
export function structuralSettings(
  config: Record<string, unknown>,
): Record<string, string | number | boolean> {
  const result: Record<string, string | number | boolean> = {};
  for (const key of [
    "promptCacheTtl",
    "subagentPromptCacheTtl",
    "idleCompaction",
    "fastMode",
    "fastModePerSessionOptIn",
    "autoCompact",
    "autoCompactEnabled",
    "autoCompactWindow",
    "model",
    "model_provider",
    "service_tier",
    "model_auto_compact_token_limit",
    "model_auto_compact_token_limit_scope",
    "model_post_turn_compact_threshold_percent",
    "model_context_window",
    "effort",
    "realtime_active",
  ]) {
    const value = config[key];
    if (
      typeof value === "boolean" ||
      (typeof value === "number" && Number.isFinite(value))
    )
      result[key] = value;
    else if (label(value)) result[key] = label(value)!;
  }
  return result;
}

/** Resolve current project/local controls at recorded directories, preserving every owner. */
export async function discoverIdleOwners(
  options: DiscoveryOptions,
  profiles: Profile[],
  requests: Request[],
): Promise<void> {
  options.signal?.throwIfAborted();
  const env = options.env ?? process.env;
  const home = options.home ?? homedir();
  const active = await realpath(
    resolve(env.CLAUDE_CONFIG_DIR ?? join(home, ".claude")),
  ).catch(() => "");
  const managedFile = options.managedSettingsFile ?? managedSettingsPath();
  for (const profile of profiles.filter((p) => p.harness === "claude")) {
    options.signal?.throwIfAborted();
    const user = await settings(profile.settingsFile, "claude");
    const managed = await settings(managedFile, "claude");
    const localEnv = active === profile.root ? env : {};
    profile.speedDirectories = Object.create(null);
    profile.idleDirectories = Object.create(null) as NonNullable<
      Profile["idleDirectories"]
    >;
    const relevant = requests.filter(
      (r) => r.profiles.includes(profile.id) && r.agent === "main",
    );
    if (relevant.some((r) => r.cwd === null) && profile.idle) {
      profile.idle.resolved = false;
      profile.idle.reason =
        "recorded working directory is absent; current project/local ownership cannot be resolved";
    }
    for (const cwd of new Set(
      relevant.flatMap((r) => (r.cwd ? [r.cwd] : [])),
    )) {
      options.signal?.throwIfAborted();
      let root = cwd;
      for (let path = cwd; ; path = dirname(path)) {
        options.signal?.throwIfAborted();
        try {
          await stat(join(path, ".git"));
          root = path;
          break;
        } catch (error) {
          if (!missing(error) && object(error).code !== "ENOTDIR")
            throw new ScanError("cannot resolve current project ownership", {
              file: path,
              line: 0,
            });
        }
        if (dirname(path) === path) break;
      }
      const scopes = [{ config: user, file: profile.settingsFile }];
      for (const file of [
        join(root, ".claude", "settings.json"),
        join(root, ".claude", "settings.local.json"),
      ])
        scopes.push({ config: await settings(file, "claude"), file });
      scopes.push({ config: managed, file: managedFile });
      profile.speedDirectories![cwd] = speedControls({
        harness: "claude",
        scopes,
        env: localEnv,
      });
      const c = idleControls({ scopes, env: localEnv });
      const config = Object.assign({}, ...scopes.map((s) => s.config), {
        env: Object.assign({}, ...scopes.map((s) => object(s.config.env))),
      }) as Record<string, unknown>;
      c.ttl = controls({
        config,
        env: localEnv,
        file: profile.settingsFile,
        managed,
        managedFile,
        scopes,
      });
      c.compaction = compactionControls({
        harness: "claude",
        user,
        managed,
        env: localEnv,
        file: profile.settingsFile,
        managedFile,
        scopes,
      });
      try {
        await stat(cwd);
      } catch {
        profile.speedDirectories![cwd]!.resolved = false;
        profile.speedDirectories![cwd]!.value = null;
        profile.speedDirectories![cwd]!.reason =
          "recorded working directory no longer exists; current ownership unresolved";
        c.resolved = false;
        c.reason =
          "recorded working directory no longer exists; current project/local ownership is unresolved";
      }
      profile.idleDirectories[cwd] = c;
    }
  }
}

/** Discover configuration aliases and freeze every selected file's length before parsing. */
export async function discover(options: DiscoveryOptions = {}): Promise<{
  roots: RootStatus[];
  profiles: Profile[];
  snapshots: Snapshot[];
}> {
  options.signal?.throwIfAborted();
  const home = options.home ?? homedir();
  const env = options.env ?? process.env;
  const explicit = !!(options.claudeDirs?.length || options.codexDirs?.length);
  const candidates = new Map<string, RootStatus>();
  function add(harness: Harness, path: string, origin: RootStatus["origin"]) {
    const absolute = resolve(path);
    const key = harness + absolute;
    const previous = candidates.get(key);
    if (previous?.origin === "explicit") return;
    candidates.set(key, {
      harness,
      path: absolute,
      origin,
      status: explicit && origin !== "explicit" ? "excluded" : "selected",
    });
  }
  add("claude", join(home, ".claude"), "default");
  add("codex", join(home, ".codex"), "default");
  if (env.CLAUDE_CONFIG_DIR)
    add("claude", env.CLAUDE_CONFIG_DIR, "environment");
  if (env.CODEX_HOME) add("codex", env.CODEX_HOME, "environment");
  let entries;
  try {
    entries = await readdir(home, { withFileTypes: true });
  } catch {
    throw new ScanError("cannot discover home directory", {
      file: home,
      line: 0,
    });
  }
  for (const entry of entries) {
    if (!(entry.isDirectory() || entry.isSymbolicLink())) continue;
    const harness = entry.name.startsWith(".claude")
      ? "claude"
      : entry.name.startsWith(".codex")
        ? "codex"
        : null;
    if (!harness || candidates.has(harness + resolve(home, entry.name)))
      continue;
    const path = join(home, entry.name);
    try {
      if (!(await stat(path)).isDirectory()) continue;
      await stat(join(path, harness === "claude" ? "projects" : "sessions"));
      add(harness, path, "discovered");
    } catch (error) {
      if (!missing(error) && object(error).code !== "ENOTDIR") {
        add(harness, path, "discovered");
      }
    }
  }
  for (const path of options.claudeDirs ?? []) add("claude", path, "explicit");
  for (const path of options.codexDirs ?? []) add("codex", path, "explicit");
  const profiles = new Map<string, Profile>();
  const roots = [...candidates.values()].sort((a, b) =>
    a.path.localeCompare(b.path),
  );
  for (const root of roots) {
    options.signal?.throwIfAborted();
    if (root.status === "excluded") continue;
    let canonical: string;
    try {
      canonical = await realpath(root.path);
      if (!(await stat(canonical)).isDirectory()) throw new Error();
    } catch (error) {
      root.status = missing(error) ? "missing" : "inaccessible";
      if (root.origin === "default" && root.status === "missing") continue;
      throw new ScanError("selected root is missing or inaccessible", {
        file: root.path,
        line: 0,
      });
    }
    const key = root.harness + canonical;
    const previous = profiles.get(key);
    if (previous) {
      previous.aliases.push(root.path);
      continue;
    }
    const settingsFile = join(
      canonical,
      root.harness === "claude" ? "settings.json" : "config.toml",
    );
    const user = await settings(settingsFile, root.harness);
    let config = user;
    const managedFile = options.managedSettingsFile ?? managedSettingsPath();
    const managed =
      root.harness === "claude" ? await settings(managedFile, "claude") : {};
    if (root.harness === "claude") {
      config = {
        ...config,
        ...managed,
        env: { ...object(config.env), ...object(managed.env) },
      };
    }
    // Shell overrides belong only to the selected active profile, never every alias.
    const active = resolve(
      env[root.harness === "claude" ? "CLAUDE_CONFIG_DIR" : "CODEX_HOME"] ??
        join(home, root.harness === "claude" ? ".claude" : ".codex"),
    );
    const activeCanonical = await realpath(active).catch(() => active);
    const localEnv = activeCanonical === canonical ? env : {};
    const combinedEnv = { ...object(config.env), ...localEnv };
    let provider: string | null =
      root.harness === "codex" ? label(config.model_provider) : null;
    if (root.harness === "claude") {
      if (combinedEnv.CLAUDE_CODE_USE_BEDROCK === "1")
        provider = "amazon-bedrock";
      else if (combinedEnv.CLAUDE_CODE_USE_VERTEX === "1")
        provider = "google-vertex-anthropic";
      else if (combinedEnv.CLAUDE_CODE_USE_FOUNDRY === "1") provider = "azure";
      else if (
        combinedEnv.ANTHROPIC_BASE_URL &&
        combinedEnv.ANTHROPIC_BASE_URL !== "https://api.anthropic.com"
      )
        provider = "custom";
    }
    const speedScopes =
      root.harness === "claude"
        ? [
            { config: user, file: settingsFile },
            { config: managed, file: managedFile },
          ]
        : [
            {
              config: await settings(
                options.codexSystemConfigFile ?? "/etc/codex/config.toml",
                "codex",
              ),
              file: options.codexSystemConfigFile ?? "/etc/codex/config.toml",
            },
            { config: user, file: settingsFile },
          ];
    const requirementsFile =
      options.codexRequirementsFile ?? "/etc/codex/requirements.toml";
    const requirements =
      root.harness === "codex" ? await settings(requirementsFile, "codex") : {};
    const speed = speedControls({
      harness: root.harness,
      scopes: speedScopes,
      env: localEnv,
    });
    if (root.harness === "codex") {
      const baseTier = [...speedScopes]
        .reverse()
        .find((s) => Object.hasOwn(s.config, "service_tier"))
        ?.config.service_tier;
      const possibleTiers: unknown[] = [baseTier];
      for (const row of Object.values(object(user.profiles)))
        possibleTiers.push(object(row).service_tier ?? baseTier);
      for (const key of ["fast_mode", "ultrafast_mode"]) {
        const value = object(requirements.features)[key];
        if (value !== undefined && typeof value !== "boolean")
          throw new ScanError("invalid required speed feature", {
            file: requirementsFile,
            line: 1,
          });
        if (
          value === false &&
          ((key === "fast_mode" &&
            ["fast", "priority"].includes(String(speed.value))) ||
            (key === "ultrafast_mode" && speed.value === "ultrafast"))
        ) {
          Object.assign(speed, {
            value: "standard",
            owner: `${requirementsFile}:features.${key}`,
            reason: "premium tier blocked by managed requirements",
            resolved: true,
          });
        }
      }
      if (
        user.profile !== undefined ||
        Object.keys(object(user.profiles)).length
      ) {
        Object.assign(speed, {
          value: null,
          resolved: false,
          reason:
            "legacy profile selector/tables exist; current Codex removed them in 0.134.0, installed version and selected profile are unresolved",
        });
      }
      for (const entry of await readdir(canonical)) {
        if (!/^[a-zA-Z0-9_-]+\.config\.toml$/.test(entry)) continue;
        const file = join(canonical, entry);
        const config = await settings(file, "codex");
        possibleTiers.push(config.service_tier ?? baseTier);
        if (
          ["model", "model_provider", "service_tier", "features"].some((key) =>
            Object.hasOwn(config, key),
          )
        ) {
          Object.assign(speed, {
            value: null,
            resolved: false,
            reason:
              "named profile files can override speed/model/gates; --profile selection is unobserved",
            owner: `${settingsFile}; ${file} (--profile selection unresolved)`,
          });
        }
      }
      const blocked = (tier: unknown) =>
        (["fast", "priority"].includes(String(tier)) &&
          object(requirements.features).fast_mode === false) ||
        (tier === "ultrafast" &&
          object(requirements.features).ultrafast_mode === false);
      // An unobserved profile cannot bypass a hard policy block. A different,
      // allowed premium tier still keeps profile selection unresolved.
      if (
        possibleTiers.some(blocked) &&
        possibleTiers.every(
          (tier) =>
            tier === undefined ||
            ["standard", "default"].includes(String(tier)) ||
            blocked(tier),
        )
      ) {
        Object.assign(speed, {
          value: "standard",
          resolved: true,
          owner: `${requirementsFile}:features`,
          reason:
            "all locally configured premium tiers are blocked by managed requirements, including profile alternatives",
        });
      }
    }
    const histories: string[] = [];
    for (const name of root.harness === "claude"
      ? ["projects"]
      : ["sessions", "archived_sessions"]) {
      try {
        histories.push(await realpath(join(canonical, name)));
      } catch (error) {
        if (!missing(error))
          throw new ScanError("cannot inspect transcript directory", {
            file: join(canonical, name),
            line: 0,
          });
      }
    }
    profiles.set(key, {
      id: identity(key),
      harness: root.harness,
      root: canonical,
      aliases: [root.path],
      histories,
      settingsFile,
      ttl: controls({
        config,
        env: localEnv,
        file: settingsFile,
        managed,
        managedFile,
      }),
      provider,
      settings: structuralSettings(config),
      speed,
      compaction: compactionControls({
        harness: root.harness,
        user,
        managed,
        env: localEnv,
        file: settingsFile,
        managedFile,
      }),
      ...(root.harness === "claude"
        ? {
            idle: idleControls({
              scopes: [
                { config: user, file: settingsFile },
                { config: managed, file: managedFile },
              ],
              env: localEnv,
            }),
          }
        : {}),
    });
  }
  const files = new Map<string, Snapshot>();
  const visited = new Map<string, string[]>();
  async function walk(path: string, profile: Profile): Promise<void> {
    options.signal?.throwIfAborted();
    let canonical: string;
    try {
      canonical = await realpath(path);
      const info = await stat(canonical);
      const key = `${info.dev}:${info.ino}`;
      if (info.isDirectory()) {
        const traversed = visited.get(key) ?? [];
        if (traversed.includes(profile.id)) return;
        visited.set(key, [...traversed, profile.id]);
        for (const entry of (await readdir(canonical)).sort())
          await walk(join(canonical, entry), profile);
      } else if (info.isFile() && path.endsWith(".jsonl")) {
        const previous = files.get(key);
        if (previous) {
          if (!previous.profiles.includes(profile.id))
            previous.profiles.push(profile.id);
        } else
          files.set(key, {
            file: canonical,
            harness: profile.harness,
            profiles: [profile.id],
            bytes: info.size,
            device: info.dev,
            inode: info.ino,
            deferredBytes: 0,
            lines: 0,
          });
      }
    } catch (error) {
      if (error instanceof ScanError) throw error;
      throw new ScanError("cannot snapshot selected history", {
        file: path,
        line: 0,
      });
    }
  }
  for (const profile of profiles.values())
    for (const history of profile.histories) await walk(history, profile);
  return {
    roots,
    profiles: [...profiles.values()],
    snapshots: [...files.values()],
  };
}

/** Keep unresolved Codex project overrides advisory rather than asserting user defaults own a launch. */
export async function discoverCodexSpeedOwners(
  options: DiscoveryOptions,
  profiles: Profile[],
  requests: Request[],
): Promise<void> {
  for (const profile of profiles.filter((p) => p.harness === "codex")) {
    for (const cwd of new Set(
      requests
        .filter((r) => r.profiles.includes(profile.id))
        .flatMap((r) => (r.cwd ? [r.cwd] : [])),
    )) {
      for (let path = cwd; ; path = dirname(path)) {
        options.signal?.throwIfAborted();
        const file = join(path, ".codex", "config.toml");
        const config = await settings(file, "codex");
        if (
          ["model", "model_provider", "service_tier", "features"].some((key) =>
            Object.hasOwn(config, key),
          ) &&
          !profile.speedDirectories?.[cwd]
        ) {
          profile.speedDirectories ??= Object.create(null);
          profile.speedDirectories![cwd] = {
            ...profile.speed!,
            value: null,
            resolved: false,
            owner: file,
            reason:
              "current project overrides exist; trust, profile and CLI/session selection are not established",
          };
        }
        try {
          await stat(join(path, ".git"));
          break;
        } catch (error) {
          if (!missing(error) && object(error).code !== "ENOTDIR")
            throw new ScanError("cannot inspect project scope", {
              file: path,
              line: 0,
            });
        }
        if (dirname(path) === path) break;
      }
    }
  }
}

/** Resolve current context controls without treating configured tools as historical exposure. */
export async function discoverContextOwners(
  options: DiscoveryOptions,
  profiles: Profile[],
  requests: Request[],
): Promise<void> {
  const env = options.env ?? process.env;
  const home = options.home ?? homedir();
  for (const profile of profiles) {
    const claude = profile.harness === "claude";
    const active = await realpath(
      resolve(
        env[claude ? "CLAUDE_CONFIG_DIR" : "CODEX_HOME"] ??
          join(home, claude ? ".claude" : ".codex"),
      ),
    ).catch(() => "");
    const localEnv = active === profile.root ? env : {};
    const base = [
      {
        file: profile.settingsFile,
        config: await settings(profile.settingsFile, profile.harness),
      },
    ];
    const relevant = requests.filter((r) => r.profiles.includes(profile.id));
    const directories = new Set(relevant.map((r) => r.cwd));
    if (!directories.size) directories.add(null);
    profile.context = [];
    for (const cwd of directories) {
      options.signal?.throwIfAborted();
      const chain: string[] = [];
      let rootFound = false;
      let exists = false;
      if (cwd) {
        try {
          exists = (await stat(cwd)).isDirectory();
        } catch (error) {
          if (!missing(error))
            throw new ScanError("cannot inspect current instruction scope", {
              file: cwd,
              line: 0,
            });
        }
        for (let path = cwd; ; path = dirname(path)) {
          chain.unshift(path);
          try {
            await stat(join(path, ".git"));
            rootFound = true;
            break;
          } catch (error) {
            if (!missing(error) && object(error).code !== "ENOTDIR")
              throw new ScanError("cannot inspect current instruction scope", {
                file: path,
                line: 0,
              });
          }
          if (dirname(path) === path) break;
        }
      }
      const scopes = claude
        ? [...base]
        : [
            {
              file: options.codexSystemConfigFile ?? "/etc/codex/config.toml",
              config: await settings(
                options.codexSystemConfigFile ?? "/etc/codex/config.toml",
                "codex",
              ),
            },
            ...base,
          ];
      if (claude && chain.length)
        for (const file of [
          join(chain[0]!, ".claude/settings.json"),
          join(chain[0]!, ".claude/settings.local.json"),
        ])
          scopes.push({ file, config: await settings(file, "claude") });
      if (!claude)
        for (const path of chain) {
          const file = join(path, ".codex/config.toml");
          scopes.push({ file, config: await settings(file, "codex") });
        }
      if (claude) {
        const file = options.managedSettingsFile ?? managedSettingsPath();
        scopes.push({ file, config: await settings(file, "claude") });
      }
      const config = Object.assign(
        {},
        ...scopes.map((s) => s.config),
      ) as Record<string, unknown>;
      // Session, plugin and remote managed overrides are not inferred from disk.
      const resolved =
        claude &&
        exists &&
        !Object.hasOwn(config, "claudeMdExcludes") &&
        relevant
          .filter((r) => r.cwd === cwd)
          .every(
            (r) =>
              r.harness === "claude" &&
              r.agent === "main" &&
              /^2\.1\.(\d+)$/.test(r.version ?? "") &&
              Number(r.version!.split(".")[2]) >= 314 &&
              ["cli", "claude-cli"].includes(r.client ?? ""),
          );
      const state: NonNullable<Profile["context"]>[number] = {
        cwd,
        resolved,
        reason: resolved
          ? "current local native CLI instruction chain; unseen launch/remote managed overrides remain possible"
          : "current project trust, selected profile, launch/managed overrides or native client applicability is unresolved",
        controls: [],
        servers: [],
        apps: [],
        files: [],
      };
      const setting = (key: string, fallback: number | null = null) => {
        const value = config[key];
        if (
          value !== undefined &&
          (typeof value !== "number" ||
            !Number.isSafeInteger(value) ||
            value <= 0)
        )
          throw new ScanError("invalid context budget setting", {
            file: [...scopes].reverse().find((s) => key in s.config)!.file,
            line: 1,
          });
        state.controls.push({
          key,
          value: typeof value === "number" ? value : fallback,
          owner: `${[...scopes].reverse().find((s) => key in s.config)?.file ?? profile.settingsFile}:${key}`,
        });
      };
      if (claude) {
        setting("bashOutputMaxChars");
        const environment = scopedEnvironment({ scopes, env: localEnv });
        for (const key of [
          "ENABLE_TOOL_SEARCH",
          "MAX_MCP_OUTPUT_TOKENS",
          "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        ]) {
          const value = environment.values[key];
          let safe: string | number | null = null;
          if (key === "MAX_MCP_OUTPUT_TOKENS") {
            if (
              value !== undefined &&
              (typeof value !== "string" ||
                !/^[1-9]\d*$/.test(value) ||
                !Number.isSafeInteger(Number(value)))
            )
              throw new ScanError("invalid context output budget", {
                file: profile.settingsFile,
                line: 1,
              });
            safe = value === undefined ? null : Number(value);
          } else if (
            typeof value === "string" &&
            /^(true|false|0|1|auto(?::(?:100|\d{1,2}))?)$/.test(value)
          )
            safe = value;
          state.controls.push({
            key,
            value: safe,
            owner:
              environment.owners[key] ?? `${profile.settingsFile}:env.${key}`,
          });
        }
      } else {
        setting("tool_output_token_limit");
        setting("project_doc_max_bytes", 32768);
      }
      const serverScopes = claude
        ? [
            ...scopes,
            ...chain.slice(0, 1).map((path) => ({
              file: join(path, ".mcp.json"),
              config: {} as Record<string, unknown>,
            })),
          ]
        : scopes;
      if (claude && chain[0])
        serverScopes[serverScopes.length - 1]!.config = await settings(
          join(chain[0], ".mcp.json"),
          "claude",
        );
      const serverMap = new Map<
        string,
        { config: Record<string, unknown>; file: string }
      >();
      for (const scope of serverScopes) {
        const field = scope.config[claude ? "mcpServers" : "mcp_servers"];
        if (
          field !== undefined &&
          (field === null || typeof field !== "object" || Array.isArray(field))
        )
          throw new ScanError("invalid MCP server configuration", {
            file: scope.file,
            line: 1,
          });
        for (const [name, value] of Object.entries(object(field))) {
          if (
            !label(name) ||
            !value ||
            typeof value !== "object" ||
            Array.isArray(value)
          )
            throw new ScanError("invalid MCP server configuration", {
              file: scope.file,
              line: 1,
            });
          serverMap.set(name, {
            config: { ...serverMap.get(name)?.config, ...object(value) },
            file: scope.file,
          });
        }
      }
      for (const [name, server] of serverMap) {
        const c = server.config;
        for (const key of ["enabled", "alwaysLoad"])
          if (c[key] !== undefined && typeof c[key] !== "boolean")
            throw new ScanError("invalid MCP enablement setting", {
              file: server.file,
              line: 1,
            });
        for (const key of ["enabled_tools", "disabled_tools"])
          if (
            c[key] !== undefined &&
            (!Array.isArray(c[key]) ||
              (c[key] as unknown[]).some((v) => typeof v !== "string"))
          )
            throw new ScanError("invalid MCP tool enablement list", {
              file: server.file,
              line: 1,
            });
        const omitted = c.omit_tools_from;
        if (
          omitted !== undefined &&
          (!Array.isArray(omitted) ||
            omitted.some(
              (v) => !["direct", "deferred", "code_mode"].includes(String(v)),
            ))
        )
          throw new ScanError("invalid MCP omission setting", {
            file: server.file,
            line: 1,
          });
        state.servers.push({
          name,
          group: identity(`mcp-server:${name}`),
          owner: `${server.file}:${claude ? "mcpServers" : "mcp_servers"}.${name}`,
          enabled: !claude ? c.enabled !== false : null,
          alwaysLoad: typeof c.alwaysLoad === "boolean" ? c.alwaysLoad : null,
          omitted: Array.isArray(omitted) ? (omitted as string[]) : [],
          blockedTools: Array.isArray(c.disabled_tools)
            ? c.disabled_tools.length
            : 0,
        });
      }
      if (!claude) {
        const apps = new Map<
          string,
          { config: Record<string, unknown>; file: string }
        >();
        for (const scope of scopes) {
          if (
            scope.config.apps !== undefined &&
            (!scope.config.apps ||
              typeof scope.config.apps !== "object" ||
              Array.isArray(scope.config.apps))
          )
            throw new ScanError("invalid app exposure setting", {
              file: scope.file,
              line: 1,
            });
          for (const [name, value] of Object.entries(
            object(scope.config.apps),
          )) {
            if (!value || typeof value !== "object" || Array.isArray(value))
              throw new ScanError("invalid app exposure setting", {
                file: scope.file,
                line: 1,
              });
            apps.set(name, {
              config: { ...apps.get(name)?.config, ...object(value) },
              file: scope.file,
            });
          }
        }
        for (const [name, { config: app, file }] of apps) {
          if (
            !label(name) ||
            (app.enabled !== undefined && typeof app.enabled !== "boolean") ||
            (app.omit_tools_from !== undefined &&
              (!Array.isArray(app.omit_tools_from) ||
                app.omit_tools_from.some(
                  (v) =>
                    !["direct", "deferred", "code_mode"].includes(String(v)),
                )))
          )
            throw new ScanError("invalid app exposure setting", {
              file,
              line: 1,
            });
          state.apps.push({
            name,
            owner: `${file}:apps.${name}`,
            enabled: app.enabled !== false,
            omitted: Array.isArray(app.omit_tools_from)
              ? (app.omit_tools_from as string[])
              : [],
          });
        }
      }
      const paths: string[] = [];
      if (claude) {
        paths.push(join(profile.root, "CLAUDE.md"));
        // Claude's instruction hierarchy continues above the Git root.
        for (
          let path = cwd;
          path !== null;
          path = dirname(path) === path ? null : dirname(path)
        )
          paths.push(
            join(path, "CLAUDE.md"),
            join(path, "CLAUDE.local.md"),
            join(path, ".claude/CLAUDE.md"),
          );
      } else {
        // project_doc_max_bytes applies to the project chain, not the global file.
        for (const path of rootFound ? chain : cwd ? [cwd] : []) {
          const names = ["AGENTS.override.md", "AGENTS.md"];
          const fallbacks = config.project_doc_fallback_filenames;
          if (
            fallbacks !== undefined &&
            (!Array.isArray(fallbacks) ||
              fallbacks.some(
                (v) =>
                  typeof v !== "string" || v.includes("/") || v.includes("\\"),
              ))
          )
            throw new ScanError("invalid instruction fallback filenames", {
              file: profile.settingsFile,
              line: 1,
            });
          if (Array.isArray(fallbacks)) names.push(...(fallbacks as string[]));
          for (const name of names) {
            try {
              if ((await readFile(join(path, name), "utf8")).trim()) {
                paths.push(join(path, name));
                break;
              }
            } catch (error) {
              if (!missing(error))
                throw new ScanError("cannot read current instructions", {
                  file: join(path, name),
                  line: 1,
                });
            }
          }
        }
      }
      for (const file of new Set(paths)) {
        try {
          const body = await readFile(file, "utf8");
          state.files.push({
            file,
            characters: [...body].length,
            bytes: Buffer.byteLength(body),
            lines: body.length
              ? body.split("\n").length - Number(body.endsWith("\n"))
              : 0,
            fingerprint: identity(body),
          });
        } catch (error) {
          if (!missing(error))
            throw new ScanError("cannot read current instructions", {
              file,
              line: 1,
            });
        }
      }
      profile.context.push(state);
    }
  }
}
