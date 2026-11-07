import type { Scan } from "./model.js";
import { count, label, ScanError } from "./safe.js";
import { structuralSettings } from "./discovery.js";

function require(value: unknown): asserts value {
  if (!value) throw new ScanError("invalid remote scan metadata");
}
function unique(values: string[]): Set<string> {
  const set = new Set(values);
  require(set.size === values.length);
  return set;
}
function opaque(value: string): void {
  require(/^[a-f0-9]{64}$/.test(value));
}

/** Validate counts, opaque identities and graph references before remote metadata reaches checks. */
export function validateScan(scan: Scan): void {
  require(scan.hosts === undefined);
  require(
    Number.isFinite(Date.parse(scan.snapshotStartedAt)) &&
      Number.isFinite(Date.parse(scan.snapshotFinishedAt)),
  );
  if (scan.since !== null) require(Number.isFinite(scan.since));
  const profiles = unique(scan.profiles.map((p) => p.id));
  profiles.forEach(opaque);
  const selected = (values: string[]) => {
    require(values.length);
    values.forEach((p) => require(profiles.has(p)));
  };
  const settings = (value: Record<string, unknown>) => {
    const normalized = structuralSettings(value);
    for (const [key, child] of Object.entries(value))
      require(Object.hasOwn(normalized, key) && normalized[key] === child);
  };
  for (const p of scan.profiles) settings(p.settings);
  for (const s of scan.snapshots) {
    selected(s.profiles);
    for (const v of [s.bytes, s.device, s.inode, s.deferredBytes, s.lines])
      count(v);
    require(s.deferredBytes <= s.bytes);
  }
  const requests = [...scan.requests, ...scan.cacheContext.requests];
  const ids = unique(requests.map((r) => r.id));
  for (const r of requests) {
    require(new RegExp(`^${r.harness}:[a-f0-9]{64}$`).test(r.id));
    opaque(r.session);
    selected(r.profiles);
    require(r.locations.length && r.parts.length);
    require(r.endTime >= r.time);
    for (const value of [
      r.model,
      r.provider,
      r.source,
      r.trigger,
      r.client,
      r.version,
    ])
      require(value === null || label(value) === value);
    for (const l of r.locations) count(l.line);
    for (const p of r.parts) {
      for (const value of Object.values(p.usage)) count(value);
      const u = p.usage;
      require(u.input === null || (u.read ?? 0) + (u.write ?? 0) <= u.input);
      require(u.output === null || (u.reasoning ?? 0) <= u.output);
      for (const pair of [
        [u.write5m, u.write1h],
        [u.write30m, u.write12h],
      ])
        require(
          pair.some((v) => v === null) ||
            u.write === null ||
            pair[0]! + pair[1]! === u.write,
        );
    }
    if (r.serverTools)
      for (const [key, value] of Object.entries(r.serverTools)) {
        require(label(key) === key);
        count(value);
      }
  }
  const events = [...scan.events, ...scan.cacheContext.boundaries];
  unique(events.map((e) => e.id));
  for (const e of events) {
    opaque(e.id);
    opaque(e.session);
    selected(e.profiles);
    settings(e.settings);
    require(
      e.request === null || /^(claude|codex):[a-f0-9]{64}$/.test(e.request),
    );
    require(e.origins.length);
    for (const origin of e.origins) {
      opaque(origin.session);
      count(origin.location.line);
    }
    for (const value of [e.preTokens, e.postTokens, e.durationMs]) count(value);
  }
  const components = unique(scan.exposure.components.map((c) => c.id));
  components.forEach(opaque);
  const componentRefs = (values: string[]) =>
    values.forEach((id) => require(components.has(id)));
  for (const c of scan.exposure.components) {
    opaque(c.key);
    opaque(c.fingerprint);
    count(c.characters);
    count(c.recordedTokens);
    componentRefs(c.members);
  }
  unique(scan.exposure.requests.map((e) => e.request));
  for (const e of scan.exposure.requests) {
    require(ids.has(e.request));
    for (const refs of [
      e.components,
      e.candidates,
      e.removed,
      e.inherited,
      e.deferred,
    ])
      componentRefs(refs);
    count(e.completeCharacters);
  }
  for (const i of scan.exposure.invocations) {
    opaque(i.id);
    opaque(i.target);
    opaque(i.session);
    i.sessions.forEach(opaque);
    selected(i.profiles);
  }
  for (const v of scan.exposure.visibility) {
    selected(v.profiles);
    if (v.component) require(components.has(v.component));
  }
}
