import type {
  IdleControl,
  Profile,
  SpeedControl,
  WindowControl,
} from "./model.js";
import { object, ScanError } from "./safe.js";

/** Merge settings environments in precedence order, preserving each effective key's owner. */
export function scopedEnvironment(options: {
  scopes: { config: Record<string, unknown>; file: string }[];
  env: Record<string, unknown>;
}): { values: Record<string, unknown>; owners: Record<string, string> } {
  const values: Record<string, unknown> = Object.create(null);
  const owners: Record<string, string> = Object.create(null);
  for (const { config, file } of options.scopes)
    for (const [key, value] of Object.entries(object(config.env))) {
      values[key] = value;
      owners[key] = `${file}:env.${key}`;
    }
  for (const [key, value] of Object.entries(options.env)) {
    values[key] = value;
    owners[key] = key;
  }
  return { values, owners };
}

/** Resolve native idle settings in precedence order, retaining invalid environment values as unknown. */
export function idleControls(options: {
  scopes: { config: Record<string, unknown>; file: string }[];
  env: Record<string, unknown>;
}): IdleControl {
  const first = options.scopes[0]?.file ?? "settings.json";
  let value: boolean | null = true;
  let owner = `${first}:idleCompaction`;
  let autoCompact: boolean | null = true;
  let threshold: number | null = 200000;
  let thresholdOwner = "CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS (default)";
  const { values: env, owners } = scopedEnvironment(options);
  for (const { config, file } of options.scopes) {
    for (const key of ["idleCompaction", "autoCompactEnabled"] as const) {
      if (!Object.hasOwn(config, key)) continue;
      if (typeof config[key] !== "boolean")
        throw new ScanError(`invalid ${key}`, { file, line: 1 });
      if (key === "idleCompaction") {
        value = config[key];
        owner = `${file}:${key}`;
      } else autoCompact = config[key];
    }
  }
  if (env.DISABLE_AUTO_COMPACT === "1") autoCompact = false;
  const key = "CLAUDE_CODE_IDLE_COMPACT_MIN_TOKENS";
  if (env[key] !== undefined) {
    thresholdOwner = owners[key]!;
    const text = String(env[key]);
    const n = Number(text);
    threshold =
      /^\d+$/.test(text) && Number.isSafeInteger(n) && n > 0
        ? Math.max(100000, n)
        : null;
  }
  return {
    value,
    owner,
    threshold,
    thresholdOwner,
    autoCompact,
    resolved: threshold !== null,
    reason:
      threshold === null
        ? "idle threshold environment value is invalid or unsupported"
        : "current native files and active-profile environment; unrecorded launch overrides remain outside observed ownership",
  };
}

/** Documented Claude settings and environment window bounds. */
export const CLAUDE_WINDOW_BOUNDS = {
  minimum: 100000,
  maximum: 1000000,
} as const;

/** Resolve observed current files and environment, preserving each control's owner. */
export function compactionControls(options: {
  harness: Profile["harness"];
  user: Record<string, unknown>;
  managed: Record<string, unknown>;
  env: Record<string, unknown>;
  file: string;
  managedFile: string;
  scopes?: { config: Record<string, unknown>; file: string }[];
}): Profile["compaction"] {
  const { harness, user, managed, file, managedFile } = options;
  const scopes = options.scopes ?? [
    { config: user, file },
    { config: managed, file: managedFile },
  ];
  const { values: env, owners } = scopedEnvironment({
    scopes,
    env: options.env,
  });
  const models: Record<string, WindowControl> = Object.create(null);
  if (harness === "codex") {
    const value = user.model_auto_compact_token_limit;
    if (
      value !== undefined &&
      (!Number.isSafeInteger(value) || Number(value) <= 0)
    )
      throw new ScanError("invalid compaction token limit", { file, line: 1 });
    const scope = user.model_auto_compact_token_limit_scope;
    const ambiguous = Object.keys(object(user.profiles)).length > 0;
    const supported = scope === undefined || scope === "total";
    return {
      default: {
        value: typeof value === "number" ? value : null,
        owner: `${file}:model_auto_compact_token_limit`,
        applicable: typeof value === "number" && supported && !ambiguous,
        ...(!supported
          ? {
              comparisonUnknown:
                "effective Codex threshold scope is unsupported",
            }
          : {}),
        reason: !supported
          ? "body-after-prefix or unknown threshold scope is not a total-context window"
          : ambiguous
            ? "named Codex profiles exist; the active profile/CLI override is not established"
            : value === undefined
              ? "tuned model default is not established from current config"
              : "current explicit total-context limit; launch overrides are not recorded by config",
      },
      models,
    };
  }
  const envKey = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";
  let override: number | null = null;
  if (env[envKey] !== undefined) {
    const match = /^([0-9]+(?:\.[0-9]+)?)([kKmM]?)$/.exec(String(env[envKey]));
    if (match) {
      const value =
        Number(match[1]) *
        (match[2]?.toLowerCase() === "m" ? 1e6 : match[2] ? 1e3 : 1);
      if (Number.isFinite(value) && value > 0)
        override = Math.max(
          CLAUDE_WINDOW_BOUNDS.minimum,
          Math.min(CLAUDE_WINDOW_BOUNDS.maximum, Math.floor(value)),
        );
    }
  }
  const envOwner = (key: string) => owners[key] ?? `${file}:env.${key}`;
  // The caller supplies the effective environment and its explicitly owned keys.
  function resolve(model: string | null): WindowControl {
    let value: unknown;
    let owner = model
      ? `${file}:modelSettings.${model}.autoCompactWindow`
      : `${file}:autoCompactWindow`;
    for (const { config, file: source } of scopes) {
      const row = model ? object(object(config.modelSettings)[model]) : {};
      if (Object.hasOwn(config, "autoCompactWindow")) {
        value = config.autoCompactWindow;
        owner = `${source}:autoCompactWindow`;
      }
      if (Object.hasOwn(row, "autoCompactWindow")) {
        value = row.autoCompactWindow;
        owner = `${source}:modelSettings.${model}.autoCompactWindow`;
      }
    }
    if (
      value !== undefined &&
      value !== "auto" &&
      (typeof value !== "number" ||
        !Number.isSafeInteger(value) ||
        value < CLAUDE_WINDOW_BOUNDS.minimum ||
        value > CLAUDE_WINDOW_BOUNDS.maximum)
    )
      throw new ScanError("invalid auto-compact window", {
        file: owner.startsWith(managedFile + ":") ? managedFile : file,
        line: 1,
      });
    if (override !== null) {
      value = override;
      owner = envOwner(envKey);
    }
    const suffix = override !== null && /[kKmM]$/.test(String(env[envKey]));
    const disabled =
      env.DISABLE_AUTO_COMPACT === "1" ||
      [...scopes]
        .reverse()
        .find((s) => Object.hasOwn(s.config, "autoCompactEnabled"))?.config
        .autoCompactEnabled === false;
    return {
      value: typeof value === "number" ? value : null,
      owner,
      applicable: typeof value === "number" && !disabled,
      minimumClaudePatch: suffix ? 304 : 288,
      reason: disabled
        ? "automatic compaction is disabled; changing its window alone does not enable it"
        : override !== null
          ? "current environment window override (documented clamp applied)"
          : typeof value === "number"
            ? "current user/managed window; project, launch and per-agent overrides may take precedence"
            : "tuned model default is not established from current files",
    };
  }
  for (const { config, file: source } of scopes) {
    if (
      config.modelSettings !== undefined &&
      (config.modelSettings === null ||
        typeof config.modelSettings !== "object" ||
        Array.isArray(config.modelSettings))
    )
      throw new ScanError("invalid model settings", { file: source, line: 1 });
    for (const key of Object.keys(object(config.modelSettings))) {
      const model = /^[a-zA-Z0-9_.:/@<>+\[\]-]{1,160}$/.test(key) ? key : null;
      const row = object(config.modelSettings)[key];
      if (
        model === null ||
        row === null ||
        typeof row !== "object" ||
        Array.isArray(row)
      )
        throw new ScanError("invalid per-model settings", {
          file: source,
          line: 1,
        });
      models[model] = resolve(model);
    }
  }
  return { default: resolve(null), models };
}

/** Exact model settings win; known dated Claude IDs share their undated model row. */
export function windowControl(
  profile: Profile,
  model: string | null,
): WindowControl {
  const canonical = model?.replace(/-\d{8}$/, "");
  const current =
    model && Object.hasOwn(profile.compaction.models, model)
      ? profile.compaction.models[model]!
      : canonical &&
          /^claude-(opus|sonnet|haiku|fable|mythos)-\d/.test(canonical) &&
          Object.hasOwn(profile.compaction.models, canonical)
        ? profile.compaction.models[canonical]!
        : profile.compaction.default;
  const family = /^claude-(opus|sonnet|haiku|fable|mythos)-/.exec(
    model ?? "",
  )?.[1];
  const aliases = family ? [family, `${family}[1m]`, `${canonical}[1m]`] : [];
  if (
    aliases.some(
      (alias) =>
        Object.hasOwn(profile.compaction.models, alias) &&
        profile.compaction.models[alias]!.owner !== current.owner,
    )
  )
    return {
      ...current,
      value: null,
      applicable: false,
      reason:
        "model alias or [1m] setting may own the current window; effective alias selection is unresolved",
    };
  return current;
}

/** Whether a current documented Claude model supports fast speed, without resolving custom aliases. */
export function claudeFastSupport(model: string | null): boolean | null {
  if (model === null) return null;
  if (
    /^(?:opus(?:\[1m\])?|claude-opus-(?:5-5|5|4-8)(?:-\d{8})?(?:\[1m\])?)$/.test(
      model,
    )
  )
    return true;
  if (
    /^(?:(?:sonnet|haiku)(?:\[1m\])?|claude-(?:opus|sonnet|haiku)-\d+(?:-\d+)*(?:\[1m\])?)$/.test(
      model,
    )
  )
    return false;
  return null;
}

/** Resolve current speed controls using documented defaults and explicit disabling gates. */
export function speedControls(options: {
  harness: Profile["harness"];
  scopes: { config: Record<string, unknown>; file: string }[];
  env: Record<string, unknown>;
}): SpeedControl {
  const first = options.scopes[0]?.file ?? "settings";
  const effective = (
    key: string,
  ): { value: unknown; owner: string; file: string } => {
    let value: unknown;
    let file = first;
    let owner = `${first}:${key} (documented default)`;
    for (const scope of options.scopes) {
      const parts = key.split(".");
      let row: unknown = scope.config;
      for (const part of parts) row = object(row)[part];
      if (row !== undefined) {
        value = row;
        file = scope.file;
        owner = `${scope.file}:${key}`;
      }
    }
    return { value, owner, file };
  };
  const boolean = (key: string) => {
    const row = effective(key);
    if (row.value !== undefined && typeof row.value !== "boolean")
      throw new ScanError(`invalid ${key}`, {
        file: row.file,
        line: 1,
      });
    return row;
  };
  const result = (
    value: string | null,
    owner: string,
    reason: string,
  ): SpeedControl => ({
    value,
    owner,
    reason,
    resolved: value !== null,
    replacement:
      options.harness === "claude"
        ? "fastMode: false; /fast off for an active session"
        : 'service_tier = "default"; /fast off for an active session',
  });
  for (const { config, file } of options.scopes)
    if (
      options.harness === "codex" &&
      config.features !== undefined &&
      (config.features === null ||
        typeof config.features !== "object" ||
        Array.isArray(config.features))
    )
      throw new ScanError("invalid speed features", { file, line: 1 });
  const model = effective("model");
  if (options.harness === "claude") {
    const fast = boolean("fastMode");
    const opt = boolean("fastModePerSessionOptIn");
    const { values: env, owners } = scopedEnvironment(options);
    if (env.CLAUDE_CODE_DISABLE_FAST_MODE === "1")
      return result(
        "standard",
        owners.CLAUDE_CODE_DISABLE_FAST_MODE!,
        "fast mode disabled by environment",
      );
    // The managed layer is last, including an enforced false value.
    if (fast.value !== true)
      return result(
        "standard",
        fast.owner,
        "fast mode off (unset defaults off)",
      );
    if (opt.value === true)
      return result(
        "standard",
        opt.owner,
        "per-session opt-in disables the saved preference at startup; no current session opt-in observed",
      );
    if (
      [
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
      ].some((key) => env[key] === "1") ||
      (env.ANTHROPIC_BASE_URL !== undefined &&
        env.ANTHROPIC_BASE_URL !== "https://api.anthropic.com")
    )
      return result(
        null,
        fast.owner,
        "premium availability/effective ownership through this provider or gateway is unresolved",
      );
    const selected = env.ANTHROPIC_MODEL ?? model.value;
    const support = claudeFastSupport(
      typeof selected === "string" ? selected : null,
    );
    if (support === true)
      return result(
        "fast",
        fast.owner,
        "current supported Opus fast preference; account availability and unrecorded launch/session overrides are unknown",
      );
    if (support === false)
      return result(
        "standard",
        env.ANTHROPIC_MODEL !== undefined
          ? owners.ANTHROPIC_MODEL!
          : model.owner,
        "current model does not support fast mode; saved preference is inactive",
      );
    return result(
      null,
      fast.owner,
      "current model alias or fast-mode support is unresolved",
    );
  }
  const tier = effective("service_tier");
  if (tier.value !== undefined && typeof tier.value !== "string")
    throw new ScanError("invalid service_tier", { file: tier.file, line: 1 });
  const fast = boolean("features.fast_mode");
  const ultra = boolean("features.ultrafast_mode");
  const value = tier.value;
  if (value === undefined || value === "default" || value === "standard")
    return result(
      "standard",
      tier.owner,
      "no premium speed preference; unrecorded API project/session overrides remain unknown",
    );
  if (value === "fast" || value === "priority" || value === "ultrafast") {
    const gate = value === "ultrafast" ? ultra : fast;
    if (gate.value === false)
      return result(
        "standard",
        gate.owner,
        "saved premium tier is disabled by its feature gate",
      );
    const provider = effective("model_provider");
    if (provider.value !== undefined && provider.value !== "openai")
      return result(
        null,
        tier.owner,
        "custom provider speed support and ownership are unresolved",
      );
    const supported =
      typeof model.value === "string" &&
      (value === "ultrafast"
        ? /^gpt-(?:6-astra|6\.1-sol)(?:-\d{4}-\d{2}-\d{2})?$/.test(model.value)
        : /^gpt-(?:6-(?:sol|astra|luna)|6\.1-(?:sol|terra)|5\.6(?:-(?:sol|terra|luna))?|5\.[45]|5\.4-mini)(?:-\d{4}-\d{2}-\d{2})?$/.test(
            model.value,
          ));
    return result(
      supported ? String(value) : null,
      tier.owner,
      supported
        ? "current premium preference; independent feature gates default on in current Codex; account access and unrecorded session overrides are unknown"
        : "saved premium preference has unresolved current model support",
    );
  }
  return result(
    null,
    tier.owner,
    "auto, flex, scale or unrecognized tier is not an established standard/premium speed control",
  );
}
