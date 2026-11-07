import { scan, type ScanOptions } from "./reader.js";
import { helperArtifact } from "./artifact.js";
import { destination, remoteScan } from "./ssh.js";
import { display, identity, ScanError } from "./safe.js";
import type { Location, Request, Scan, StructuralEvent } from "./model.js";
import type { Component, Exposure, Invocation } from "./exposure.js";

/** One independent execution context, consumed when it joins the coordinator scan. */
export interface HostScan {
  scan: Scan;
  id: string;
  transport: "local" | "ssh";
  destination: string | null;
  platform: string;
  runtime: string;
}

function union<T>(a: T[], b: T[]): T[] {
  return [...new Set([...a, ...b])];
}
function signature(value: object, exclude: string[]): string {
  return JSON.stringify(
    Object.fromEntries(
      Object.entries(value)
        .filter(([key]) => !exclude.includes(key))
        .sort(([a], [b]) => a.localeCompare(b)),
    ),
  );
}
function owners(value: unknown, host: string): void {
  if (value === null || typeof value !== "object") return;
  for (const [key, child] of Object.entries(value)) {
    if ((key === "owner" || key.endsWith("Owner")) && typeof child === "string")
      Reflect.set(value, key, `${display(host)}: ${child}`);
    else owners(child, host);
  }
}

/** Namespace host-local histories while merging only proven copied billing identities. */
export function combineHosts(inputs: HostScan[]): Scan {
  const requests = new Map<string, Request>();
  const events = new Map<string, StructuralEvent>();
  const preRequests = new Map<string, Request>();
  const preEvents = new Map<string, StructuralEvent>();
  const components = new Map<string, Component>();
  const exposures = new Map<string, Exposure>();
  const invocations = new Map<string, Invocation>();
  const result: Scan = {
    hosts: [],
    snapshotStartedAt: inputs[0]!.scan.snapshotStartedAt,
    snapshotFinishedAt: inputs.at(-1)!.scan.snapshotFinishedAt,
    since: inputs[0]!.scan.since,
    profiles: [],
    roots: [],
    snapshots: [],
    requests: [],
    events: [],
    exposure: { components: [], requests: [], invocations: [], visibility: [] },
    cacheContext: { requests: [], boundaries: [] },
    coverage: {
      requests: 0,
      sourceKnown: 0,
      tierKnown: 0,
      missingUsage: 0,
      deferredFiles: 0,
      issues: [],
    },
  };
  for (const input of inputs) {
    const { scan: data, id: host, ...metadata } = input;
    if (data.since !== result.since)
      throw new ScanError("host scan windows disagree");
    result.hosts!.push({
      ...metadata,
      id: host,
      snapshotStartedAt: data.snapshotStartedAt,
      snapshotFinishedAt: data.snapshotFinishedAt,
      coverage: data.coverage,
    });
    const namespace = (value: string) => identity(`${host}\0${value}`);
    const location = (value: Location) => {
      value.host = host;
    };
    const profiles = (values: string[]) => values.map(namespace);
    for (const p of data.profiles) {
      p.id = namespace(p.id);
      p.host = host;
      owners(p, host);
    }
    result.profiles.push(...data.profiles);
    result.roots.push(...data.roots.map((r) => ({ ...r, host })));
    result.snapshots.push(
      ...data.snapshots.map((s) => ({
        ...s,
        host,
        profiles: profiles(s.profiles),
      })),
    );
    const ids = new Map<string, string>();
    for (const r of [...data.requests, ...data.cacheContext.requests])
      ids.set(r.id, r.identityBasis === "recorded" ? r.id : namespace(r.id));
    for (const [values, target] of [
      [data.requests, requests],
      [data.cacheContext.requests, preRequests],
    ] as const)
      for (const r of values) {
        r.id = ids.get(r.id)!;
        r.session = namespace(r.session);
        r.profiles = profiles(r.profiles);
        r.locations.forEach(location);
        r.origins = r.locations.map((l) => ({
          host,
          session: r.session,
          profiles: [...r.profiles],
          location: l,
        }));
        const prior = target.get(r.id);
        if (!prior && (target === requests ? preRequests : requests).has(r.id))
          throw new ScanError(
            "copied request timestamps disagree across the selected window; no report produced",
            r.locations[0],
          );
        if (!prior) {
          target.set(r.id, r);
          continue;
        }
        const excluded = [
          "session",
          "profiles",
          "locations",
          "origins",
          "issues",
        ];
        if (signature(prior, excluded) !== signature(r, excluded))
          throw new ScanError(
            `SSH copied request metadata conflicts on ${display(host)}; no report produced`,
            r.locations[0],
          );
        prior.profiles = union(prior.profiles, r.profiles);
        prior.locations.push(...r.locations);
        prior.origins!.push(...r.origins);
        prior.issues = union(prior.issues, r.issues);
      }
    for (const [values, target] of [
      [data.events, events],
      [data.cacheContext.boundaries, preEvents],
    ] as const)
      for (const e of values) {
        e.id = e.identityBasis === "recorded" ? e.id : namespace(e.id);
        e.session = namespace(e.session);
        e.profiles = profiles(e.profiles);
        if (e.request) e.request = ids.get(e.request) ?? e.request;
        location(e.location);
        for (const origin of e.origins) {
          origin.session = namespace(origin.session);
          location(origin.location);
        }
        const prior = target.get(e.id);
        if (!prior) {
          target.set(e.id, e);
          continue;
        }
        if (
          signature(prior, ["session", "profiles", "location", "origins"]) !==
          signature(e, ["session", "profiles", "location", "origins"])
        )
          throw new ScanError(
            `copied structural metadata conflicts on ${display(host)}; no report produced`,
            e.location,
          );
        prior.profiles = union(prior.profiles, e.profiles);
        prior.origins.push(...e.origins);
      }
    for (const c of data.exposure.components) {
      c.locations.forEach(location);
      const prior = components.get(c.id);
      if (!prior) {
        components.set(c.id, c);
        continue;
      }
      if (
        signature(prior, ["locations", "firstSeen", "members", "targets"]) !==
        signature(c, ["locations", "firstSeen", "members", "targets"])
      )
        throw new ScanError(
          "copied component metadata conflicts; no report produced",
          c.locations[0],
        );
      prior.locations.push(...c.locations);
      prior.firstSeen = Math.min(prior.firstSeen, c.firstSeen);
      prior.members = union(prior.members, c.members);
      prior.targets = union(prior.targets, c.targets);
    }
    for (const e of data.exposure.requests) {
      e.request = ids.get(e.request)!;
      const prior = exposures.get(e.request);
      if (!prior) {
        exposures.set(e.request, e);
        continue;
      }
      if (
        signature(prior, ["reasons", "removed", "inherited"]) !==
        signature(e, ["reasons", "removed", "inherited"])
      ) {
        prior.candidates = union(
          [...prior.candidates, ...prior.components],
          [...e.candidates, ...e.components],
        );
        prior.components = [];
        prior.coverage = "unknown";
        prior.linkage = "unknown";
        prior.completeCharacters = null;
        prior.wireModel = null;
        prior.references = null;
        prior.deferred = union(prior.deferred, e.deferred);
        prior.reasons.push(
          "copied histories disagree on exposure across hosts",
        );
      }
      prior.removed = union(prior.removed, e.removed);
      prior.inherited = union(prior.inherited, e.inherited);
      prior.reasons = union(prior.reasons, e.reasons);
    }
    for (const i of data.exposure.invocations) {
      i.id = i.identityBasis === "recorded" ? i.id : namespace(i.id);
      i.session = namespace(i.session);
      i.sessions = i.sessions.map(namespace);
      i.profiles = profiles(i.profiles);
      i.locations.forEach(location);
      const prior = invocations.get(i.id);
      if (!prior) {
        invocations.set(i.id, i);
        continue;
      }
      if (
        signature(prior, ["session", "sessions", "profiles", "locations"]) !==
        signature(i, ["session", "sessions", "profiles", "locations"])
      )
        throw new ScanError(
          "copied invocation metadata conflicts; no report produced",
          i.locations[0],
        );
      prior.sessions = union(prior.sessions, i.sessions);
      prior.profiles = union(prior.profiles, i.profiles);
      prior.locations.push(...i.locations);
    }
    result.exposure.visibility.push(
      ...data.exposure.visibility.map((v) => ({
        ...v,
        profiles: profiles(v.profiles),
      })),
    );
    result.coverage.issues = union(
      result.coverage.issues,
      data.coverage.issues,
    );
  }
  const order = <T extends { time: number }>(values: Iterable<T>) =>
    [...values].sort((a, b) => a.time - b.time);
  result.requests = order(requests.values());
  result.events = order(events.values());
  result.cacheContext = {
    requests: order(preRequests.values()),
    boundaries: order(preEvents.values()),
  };
  result.exposure.components = [...components.values()];
  result.exposure.requests = [...exposures.values()];
  result.exposure.invocations = [...invocations.values()];
  result.coverage.requests = result.requests.length;
  result.coverage.sourceKnown = result.requests.filter(
    (r) => r.source !== null || r.trigger !== null,
  ).length;
  result.coverage.tierKnown = result.requests.filter((r) =>
    r.parts.every((p) => p.tier !== null),
  ).length;
  result.coverage.missingUsage = result.requests.filter((r) =>
    r.parts.some((p) => p.usage.input === null || p.usage.output === null),
  ).length;
  result.coverage.deferredFiles = result.snapshots.filter(
    (s) => s.deferredBytes > 0,
  ).length;
  if (
    result.requests.some((r) => new Set(r.origins?.map((o) => o.host)).size > 1)
  )
    result.coverage.issues.push(
      "proven copied requests count once; historical profile attribution is ambiguous; cache replay retains the first selected host origin and never joins histories by path",
    );
  return result;
}

/** Audit selected hosts atomically, then run the existing analyzer once with one catalog. */
export async function audit(
  options: ScanOptions & { ssh: string[]; includeLocal: boolean },
): Promise<Scan> {
  const hosts = [...new Set(options.ssh.map(destination))];
  const artifact = await helperArtifact();
  const inputs: HostScan[] = [];
  if (options.includeLocal)
    inputs.push({
      scan: await scan(options),
      id: "local",
      transport: "local",
      destination: null,
      platform: process.platform,
      runtime: process.versions.bun
        ? `bun ${process.versions.bun}`
        : `node ${process.versions.node}`,
    });
  for (const host of hosts) {
    const remote = await remoteScan({
      host,
      artifact,
      ...(options.signal ? { signal: options.signal } : {}),
      ...(options.since === undefined ? {} : { since: options.since }),
    });
    inputs.push({
      scan: remote.scan,
      id: `ssh:${host}`,
      transport: "ssh",
      destination: host,
      platform: remote.runtime.platform,
      runtime: `${remote.runtime.kind} ${remote.runtime.version}`,
    });
  }
  return combineHosts(inputs);
}
