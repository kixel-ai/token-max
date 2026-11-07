import { createReadStream } from "node:fs";
import { mkdir, open, rename, rm, stat } from "node:fs/promises";
import { dirname } from "node:path";
import { randomUUID } from "node:crypto";
import { JSONParser } from "@streamparser/json";
import type { BillingPart, Cost, Request } from "./model.js";
import { label, object, ScanError } from "./safe.js";

const PRICE_URL = "https://models.dev/api.json";
/** Dated provider rules used to interpret catalog fields. */
export const PRICE_SOURCES = [
  "https://developers.openai.com/api/docs/guides/prompt-caching (2026-11-07)",
  "https://developers.openai.com/api/docs/pricing (2026-11-07)",
  "https://platform.claude.com/docs/en/about-claude/pricing (2026-11-07)",
];

interface Rates {
  input: number | null;
  output: number | null;
  read: number | null;
  write: number | null;
}
interface ModelPrice {
  base: Rates;
  tiers: { size: number; rates: Rates }[];
  legacy: Rates | null;
  invalid: boolean;
}
/** Catalog stores only numeric pricing metadata, keyed by exact provider and model. */
export interface Catalog {
  models: Map<string, ModelPrice>;
  source: string;
  fetchedAt: string | null;
  offline: boolean;
}
function rate(value: unknown): number | null {
  if (value === undefined || value === null) return null;
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0)
    throw new ScanError("invalid price catalog rate");
  return value;
}
function rates(value: unknown): Rates {
  const c = object(value);
  return {
    input: rate(c.input),
    output: rate(c.output),
    read: rate(c.cache_read),
    write: rate(c.cache_write),
  };
}
function modelPrice(value: unknown): ModelPrice {
  const cost = object(object(value).cost);
  const tiers: ModelPrice["tiers"] = [];
  let invalid = false;
  if (cost.tiers !== undefined) {
    if (!Array.isArray(cost.tiers))
      throw new ScanError("invalid price catalog tiers");
    for (const value of cost.tiers) {
      const entry = object(value);
      const tier = object(entry.tier);
      const size = rate(tier.size);
      if (tier.type !== "context" || size === null) {
        invalid = true;
        continue;
      }
      tiers.push({ size, rates: rates(entry) });
    }
  }
  return {
    base: rates(cost),
    tiers: tiers.sort((a, b) => a.size - b.size),
    legacy: cost.context_over_200k ? rates(cost.context_over_200k) : null,
    invalid,
  };
}

/** Parse the public catalog one model at a time instead of buffering the network response. */
export async function parseCatalog(
  chunks: AsyncIterable<Uint8Array>,
): Promise<Catalog> {
  const models = new Map<string, ModelPrice>();
  const parser = new JSONParser({ paths: ["$.*.models.*"], keepStack: false });
  parser.onValue = ({ value, key, stack }) => {
    const provider = label(stack[1]?.key);
    const model = label(key);
    if (provider && model)
      models.set(`${provider}:${model}`, modelPrice(value));
  };
  try {
    for await (const chunk of chunks) parser.write(chunk);
    if (!parser.isEnded) parser.end();
  } catch (error) {
    if (error instanceof ScanError) throw error;
    throw new ScanError("invalid public price catalog");
  }
  if (!models.size)
    throw new ScanError("public price catalog contains no models");
  return { models, source: PRICE_URL, fetchedAt: null, offline: true };
}

/** Explicit offline and fixture modes never make network requests. */
export interface PriceOptions {
  file?: string;
  cache: string;
  offline?: boolean;
}
/** Fetch only public prices, caching atomically after the complete catalog validates. */
export async function loadPrices(options: PriceOptions): Promise<Catalog> {
  if (options.file || options.offline) {
    const file = options.file ?? options.cache;
    let result: Catalog;
    try {
      result = await parseCatalog(createReadStream(file));
      result.fetchedAt = (await stat(file)).mtime.toISOString();
    } catch (error) {
      if (error instanceof ScanError) throw error;
      throw new ScanError(
        "cannot read price file; fetch prices once before offline use",
      );
    }
    result.source = file;
    return result;
  }
  const temporary = options.cache + "." + randomUUID() + ".tmp";
  try {
    const response = await fetch(PRICE_URL, { redirect: "error" });
    if (!response.ok || !response.body)
      throw new ScanError(
        "public price fetch failed; use --offline with an existing cache",
      );
    await mkdir(dirname(options.cache), { recursive: true });
    const file = await open(temporary, "wx", 0o600);
    let result: Catalog;
    async function* download() {
      for await (const chunk of response.body!) {
        await file.writeFile(chunk);
        yield chunk;
      }
    }
    try {
      result = await parseCatalog(download());
    } finally {
      await file.close();
    }
    await rename(temporary, options.cache);
    return { ...result, fetchedAt: new Date().toISOString(), offline: false };
  } catch (error) {
    if (error instanceof ScanError) throw error;
    throw new ScanError(
      "cannot fetch or cache public prices; use --offline with an existing cache",
    );
  } finally {
    await rm(temporary, { force: true }).catch(() => {});
  }
}

/** A known zero with no missing components. */
export function zeroCost(): Cost {
  return { knownUsd: 0, totalUsd: 0, unknown: [], assumptions: [] };
}
/** Keep partial known charges separate from an unknown total. */
export function addCosts(costs: Cost[]): Cost {
  return {
    knownUsd: costs.reduce((n, c) => n + c.knownUsd, 0),
    totalUsd: costs.some((c) => c.totalUsd === null)
      ? null
      : costs.reduce((n, c) => n + c.totalUsd!, 0),
    unknown: [...new Set(costs.flatMap((c) => c.unknown))],
    assumptions: [...new Set(costs.flatMap((c) => c.assumptions))],
  };
}
/** Represent missing pricing without substituting a different model. */
export function unknownCost(reason: string): Cost {
  return { knownUsd: 0, totalUsd: null, unknown: [reason], assumptions: [] };
}

function writeBilling(model: string): boolean | null {
  const match = /^gpt-(\d+)(?:\.(\d+))?(?:-|$)/.exec(model);
  if (!match) return null;
  return (
    Number(match[1]) >= 6 ||
    (Number(match[1]) === 5 && Number(match[2] ?? 0) >= 6)
  );
}

function threshold(provider: string, model: string): number | null {
  // The legacy field is named over_200k, but these models bill above 272k.
  if (
    provider === "openai" &&
    /^gpt-(?:5\.[456](?:-|$)|6(?:[.-]|$))/.test(model)
  )
    return 272000;
  if (provider === "anthropic" && /^claude-haiku-5-5(?:-|$)/.test(model))
    return 100000;
  return null;
}

function selectedRates(
  catalog: Catalog,
  r: Request,
  p: BillingPart,
): { rates: Rates; assumptions: string[] } | Cost {
  if (!r.provider || !p.model)
    return unknownCost("model/provider attribution is unknown");
  const model = catalog.models.get(`${r.provider}:${p.model}`);
  if (!model)
    return unknownCost("exact model/provider has no public catalog price");
  if (model.invalid) return unknownCost("unsupported catalog tier dimensions");
  if (p.kind === "unknown")
    return unknownCost("unknown sampling iteration semantics");
  const assumptions: string[] = [];
  let selected = model.base;
  const documented = threshold(r.provider, p.model);
  if (model.legacy || model.tiers.length) {
    if (p.usage.input === null)
      return unknownCost("context tier requires total input");
    if (model.legacy && documented === null)
      return unknownCost(
        "legacy context threshold is undocumented for this model",
      );
    if (documented !== null && model.tiers.some((t) => t.size !== documented))
      return unknownCost(
        "catalog context threshold contradicts dated provider documentation",
      );
    if (model.tiers.length)
      for (const tier of model.tiers) {
        if (p.usage.input > tier.size) selected = tier.rates;
      }
    else if (model.legacy && documented !== null && p.usage.input > documented)
      selected = model.legacy;
    if (model.legacy)
      assumptions.push(
        "legacy context_over_200k resolved at documented 272000 input tokens",
      );
  }
  let factor = 1;
  if (p.tier === null)
    return unknownCost("effective service tier is not recorded");
  if (!["standard", "default"].includes(p.tier))
    return unknownCost("effective service tier has no supported catalog rate");
  if (r.provider === "anthropic") {
    if (p.speed === "fast") {
      if (/^claude-opus-(?:5-5|5|4-8)(?:-|$)/.test(p.model)) {
        factor *= 2;
        assumptions.push(
          "documented fast price is twice standard across the full context window",
        );
      } else
        return unknownCost("fast speed lacks verified model/context rates");
    } else if (p.speed !== null && p.speed !== "standard")
      return unknownCost("unknown response speed");
    if (p.speed === null)
      assumptions.push("unrecorded response speed assumed standard");
    if (p.geo === "us") {
      const generation =
        /^claude-(?:opus|sonnet|haiku|fable|mythos)-(\d+)(?:-(\d+))?/.exec(
          p.model,
        );
      if (
        !generation ||
        !(
          Number(generation[1]) > 4 ||
          (Number(generation[1]) === 4 && Number(generation[2]) >= 6)
        )
      )
        return unknownCost(
          "US inference is unsupported for this model generation",
        );
      factor *= 1.1;
      assumptions.push("documented US inference multiplier 1.1");
    } else if (p.geo !== null && p.geo !== "global")
      return unknownCost("regional rate is unsupported");
    if (p.geo === null)
      assumptions.push("unrecorded geography assumed global API equivalent");
  }
  return {
    rates: {
      input: selected.input === null ? null : selected.input * factor,
      output: selected.output === null ? null : selected.output * factor,
      read: selected.read === null ? null : selected.read * factor,
      write: selected.write === null ? null : selected.write * factor,
    },
    assumptions,
  };
}

/** Price the whole request again for every counterfactual, including context-tier changes. */
export function pricePart(catalog: Catalog, r: Request, p: BillingPart): Cost {
  const selected = selectedRates(catalog, r, p);
  if (!("rates" in selected)) return selected;
  const result = zeroCost();
  result.assumptions = selected.assumptions;
  const rates = selected.rates;
  const u = p.usage;
  function charge(
    tokens: number | null,
    rate: number | null,
    component: string,
  ) {
    if (tokens === null || (tokens > 0 && rate === null))
      result.unknown.push(component);
    else result.knownUsd += (tokens * (rate ?? 0)) / 1e6;
  }
  charge(u.output, rates.output, "output tokens/rate unknown");
  charge(u.read, rates.read, "cache reads/rate unknown");
  if (r.harness === "claude") {
    const ordinary =
      u.input === null || u.read === null || u.write === null
        ? null
        : u.input - u.read - u.write;
    charge(ordinary, rates.input, "ordinary input unknown");
    const emptyWrites = u.write === 0;
    charge(
      emptyWrites ? 0 : u.write5m,
      rates.write,
      "5m write split/rate unknown",
    );
    charge(
      emptyWrites ? 0 : u.write1h,
      rates.input === null ? null : rates.input * 2,
      "1h write split/rate unknown",
    );
    if (!emptyWrites && (u.write1h ?? 0) > 0)
      result.assumptions.push(
        "1h total write price derived as 2x ordinary input from provider documentation",
      );
  } else {
    const billing = p.model ? writeBilling(p.model) : null;
    if (billing === null) {
      result.unknown.push("model cache-write billing rules unknown");
    } else if (!billing) {
      charge(
        u.input === null || u.read === null ? null : u.input - u.read,
        rates.input,
        "ordinary input unknown",
      );
    } else if (u.write === null) {
      result.unknown.push(
        "cache writes missing on a model with billed writes; uncached input is not evidence of writes",
      );
    } else {
      const ordinary =
        u.input === null || u.read === null ? null : u.input - u.read - u.write;
      charge(ordinary, rates.input, "ordinary input unknown");
      let short = u.write30m;
      let long = u.write12h;
      if (u.write === 0) {
        short = 0;
        long = 0;
      } else if (short === null && long === null) {
        short = u.write;
        long = 0;
        result.assumptions.push(
          "unsplit recorded writes assumed default 30m lifetime; 12h allocation unknown",
        );
      }
      if (r.api !== "responses" && (long ?? 0) > 0)
        result.unknown.push("12h writes require the Responses API");
      charge(
        short,
        rates.input === null ? null : rates.input * 1.25,
        "30m write split/rate unknown",
      );
      charge(
        long,
        rates.input === null ? null : rates.input * 2.5,
        "12h write split/rate unknown",
      );
      result.assumptions.push(
        "write counts replace ordinary input at total 1.25x/2.5x rates; reasoning is already in output",
      );
    }
  }
  result.totalUsd = result.unknown.length ? null : result.knownUsd;
  return result;
}

/** Price non-token server fees separately from client tools and token iterations. */
export function serverCost(r: Request): Cost {
  if (r.harness !== "claude") return zeroCost();
  if (r.serverTools === null)
    return unknownCost("server-tool fee coverage is not recorded");
  const result = zeroCost();
  for (const [name, requests] of Object.entries(r.serverTools)) {
    if (name === "web_search_requests" && r.provider === "anthropic")
      result.knownUsd += requests * 0.01;
    else if (name !== "web_fetch_requests" && requests > 0)
      result.unknown.push(
        "server-tool charge requires execution time, free allowance, tool version, or provider rate",
      );
  }
  result.totalUsd = result.unknown.length ? null : result.knownUsd;
  return result;
}

/** Price non-overlapping iterations once, adding only separately billed server fees. */
export function priceRequest(catalog: Catalog, r: Request): Cost {
  const result = addCosts([
    ...r.parts.map((p) => pricePart(catalog, r, p)),
    serverCost(r),
  ]);
  result.assumptions = [...new Set([...result.assumptions, ...r.issues])];
  if (
    r.issues.some(
      (issue) =>
        issue.includes("no verified request delta") ||
        issue.includes("usage is absent") ||
        issue.includes("no linked assistant usage"),
    )
  ) {
    result.totalUsd = null;
    result.unknown.push(...r.issues);
  }
  return result;
}
