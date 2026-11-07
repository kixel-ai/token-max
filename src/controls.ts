import type { Profile, WindowControl } from "./model.js";
import { object, ScanError } from "./safe.js";

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
}): Profile["compaction"] {
  const { harness, user, managed, file, managedFile } = options;
  const env = { ...object(user.env), ...object(managed.env), ...options.env };
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
  const envOwner = (key: string) =>
    Object.hasOwn(options.env, key)
      ? key
      : `${Object.hasOwn(object(managed.env), key) ? managedFile : file}:env.${key}`;
  // The caller supplies the effective environment and its explicitly owned keys.
  function resolve(model: string | null): WindowControl {
    let value: unknown;
    let owner = model
      ? `${file}:modelSettings.${model}.autoCompactWindow`
      : `${file}:autoCompactWindow`;
    for (const [config, source] of [
      [user, file],
      [managed, managedFile],
    ] as const) {
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
      user.autoCompactEnabled === false ||
      managed.autoCompactEnabled === false;
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
  for (const [config, source] of [
    [user, file],
    [managed, managedFile],
  ] as const) {
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
