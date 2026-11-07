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
