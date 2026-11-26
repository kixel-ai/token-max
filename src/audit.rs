//! `token-max audit`: where a repository's sessions spent tokens they need not have, priced at
//! public API prices, each hotspot with `(host, file, line)` pointers for the `token-audit`
//! skill to read. It never prints or keeps transcript text: only tool names, counts, times,
//! prices, session metadata and pointers.
//!
//! Three buckets, each ranked and cut to a limit so the output fits an agent's context:
//! cache reuse a request lost against the one before it, tool results and what carrying them
//! in later requests cost, and sessions whose shape stands out, such as long tool loops.

use serde::{Deserialize, Serialize};

use crate::cache::{CODEX_CACHE_MILLISECONDS, observed_ttl, ttl_milliseconds};
use crate::collections::{HashMap, HashSet, IndexMap};
use crate::model::{
    Agent, BillingPart, EventKind, Harness, Id, Location, Millis, PartKind, Request,
    RequestEvidence, Scan, StructuralEvent, Ttl, Usd,
};
use crate::prices::{Catalog, prefix_spans, price_request, tier_name};
use crate::repository::{self, Checkout};
use crate::safe::ScanError;
use crate::sessions::Owners;

/// Tokens per byte of a tool result's text: transcripts record each request's input, not
/// each result's, so a result's size is estimated from its text.
const TOKENS_PER_BYTE: f64 = 0.25;

/// OpenAI caches a prompt from 1,024 tokens, in 128-token steps past that. A Codex prompt's
/// rendered end differs once another message follows it, so a request reuses at most its
/// predecessor's prompt less one step, rounded down to a step: on real transcripts, nine in
/// ten Codex requests that read less than the whole prompt missed exactly one step, and
/// counting those would bury every real miss.
const CODEX_CACHE_MINIMUM: i64 = 1024;
const CODEX_CACHE_STEP: i64 = 128;

/// OpenAI keeps a cached prefix 5 to 10 minutes after its last use; a gap past the shorter
/// may have outlived it.
const CODEX_CACHE_LIFETIME: Millis = 5 * 60_000;

/// How long OpenAI keeps a prefix written with extended retention.
const CODEX_LONG_LIFETIME: Millis = 12 * 60 * 60_000;

/// What each audit states it assumes, printed with its findings.
pub const ASSUMPTIONS: [&str; 7] = [
    "Dollars are public API prices for the recorded usage, not an invoice.",
    "A request's lost cache reuse is the part of its predecessor's cached prompt it did not read, up to what it wrote or sent uncached, priced at what those tokens cost above a cache read.",
    "A Claude request's cache lifetime is the one its predecessor wrote, else its own, else 5 minutes; a Codex request's is 12 hours or 30 minutes once its session wrote with that retention, else 5 minutes, the shortest OpenAI keeps a prefix.",
    "A Codex request can reuse its predecessor's prompt from 1,024 tokens, less the last 128 tokens, whose rendering changes once another message follows, rounded down to OpenAI's 128-token cache step.",
    "A tool result's tokens are estimated as a quarter of its text's bytes; images and other media count nothing.",
    "A tool result sits in later prompts right after the request that called it, until a compaction or a prompt too short to hold it; a result whose span crosses from one price to another in a later prompt has no cost of its own, though its tool's total prices each part.",
    "A Codex session's tool errors are the commands, MCP calls and file edits Codex recorded as failed; it is null when Codex recorded none finishing.",
];

/// Which sessions an audit covers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Filter {
    /// Every session.
    All,
    /// Sessions in a repository with one of these remotes, each [`repository::canonical`].
    Remotes(Vec<String>),
    /// Sessions in a checkout sharing this resolved Git common directory, for a repository
    /// with no remote; it names a directory on this host alone.
    Common(String),
}

impl Filter {
    /// The filter for the checkout containing `dir`; None when it is in no checkout.
    pub fn of(dir: &str) -> Result<Option<Filter>, ScanError> {
        Ok(repository::locate(dir)?.map(|found| {
            if found.urls.is_empty() {
                Filter::Common(found.common)
            } else {
                Filter::Remotes(
                    found
                        .urls
                        .iter()
                        .map(|u| repository::canonical(u))
                        .collect(),
                )
            }
        }))
    }
}

/// Where a finding is in a transcript: the host, as given to `--ssh` or None for this one,
/// and the file and 1-based line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pointer {
    pub host: Option<String>,
    pub file: String,
    pub line: u64,
}

impl Pointer {
    fn at(location: &Location) -> Self {
        Self {
            host: None,
            file: location.file.to_string(),
            line: location.line,
        }
    }
}

/// What caused a request to lose cache reuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// A tool call that started after the predecessor outlasted the cache.
    ToolWaitTtl,
    /// The gap since the predecessor outlasted the cache with no tool call outlasting it.
    IdleTtl,
    /// The model, effort or speed changed, or the prompt was compacted or shortened.
    ControlChange,
    /// Within the cache's lifetime and with nothing changed: the prompt's prefix differed.
    PrefixBust,
}

impl Cause {
    const ALL: [Cause; 4] = [
        Cause::ToolWaitTtl,
        Cause::IdleTtl,
        Cause::ControlChange,
        Cause::PrefixBust,
    ];
}

/// What changed before a [`Cause::ControlChange`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Model,
    Effort,
    Speed,
    Compaction,
    /// The prompt was shorter than what its predecessor cached, with no compaction recorded.
    Truncation,
}

/// A count of findings and what they cost.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tally {
    pub events: u64,
    pub tokens: i64,
    /// The priced findings' cost; a lower bound when `unpriced` is above zero.
    pub cost_usd: Usd,
    pub unpriced: u64,
}

impl Tally {
    fn add(&mut self, other: &Tally) {
        self.events += other.events;
        self.tokens += other.tokens;
        self.cost_usd += other.cost_usd;
        self.unpriced += other.unpriced;
    }

    fn count(&mut self, tokens: i64, cost: Option<Usd>) {
        self.events += 1;
        self.tokens += tokens;
        match cost {
            Some(usd) => self.cost_usd += usd,
            None => self.unpriced += 1,
        }
    }
}

/// One request that lost cache reuse against its predecessor in the session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Miss {
    pub cause: Cause,
    pub change: Option<Change>,
    /// Null when the request's prices are unknown.
    pub cost_usd: Option<Usd>,
    /// Tokens of the predecessor's cached prompt the request wrote or sent again.
    pub tokens: i64,
    /// Milliseconds from the predecessor to the request.
    pub gap_ms: Millis,
    /// The cache lifetime the gap was compared with.
    pub ttl_ms: Millis,
    /// Each tool the earlier request's response called, by name, once: those called
    /// between the two requests, or for Codex, which records a response's calls before its
    /// usage, those recorded before `previous` and after the request before it.
    pub tools: Vec<String>,
    pub harness: Harness,
    pub session: Option<String>,
    pub at: Pointer,
    pub previous: Pointer,
}

/// Lost cache reuse, in all and by cause, and the costliest requests.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheMisses {
    pub total: Tally,
    pub by_cause: IndexMap<Cause, Tally>,
    pub hotspots: Vec<Miss>,
}

/// One tool's results: how many, their estimated tokens, and what carrying them cost.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolTally {
    pub tool: String,
    pub results: u64,
    pub tokens: i64,
    /// Later requests that carried a result, summed over results.
    pub carried_requests: u64,
    /// Each result's tokens times the requests that carried it, itself included.
    pub carried_tokens: i64,
    /// Every priced carrying request's price for each result's tokens; a lower bound when
    /// `unpriced` is above zero.
    pub cost_usd: Usd,
    /// Results with a carrying request whose prices are unknown.
    pub unpriced: u64,
}

/// One tool result and what carrying it in later requests cost.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Output {
    pub tool: String,
    pub bytes: u32,
    pub tokens: i64,
    pub carried_requests: u64,
    pub carried_tokens: i64,
    /// Null when a carrying request's prices are unknown, or its span crosses from one price
    /// to another in a carrying prompt, which an estimated size cannot place.
    pub cost_usd: Option<Usd>,
    /// Whether the tool reported an error; null where the harness records none.
    pub error: Option<bool>,
    pub harness: Harness,
    pub session: Option<String>,
    /// The call, when its start was recorded.
    pub call: Option<Pointer>,
    /// The result.
    pub at: Pointer,
}

/// Tool results by tool and the ones carried the most.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutputs {
    pub by_tool: Vec<ToolTally>,
    pub hotspots: Vec<Output>,
}

/// The longest run of calls to one tool in a session, with no other tool between.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Streak {
    pub tool: String,
    pub calls: u64,
    pub at: Pointer,
}

/// One session's shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Shape {
    pub harness: Harness,
    pub session: Option<String>,
    pub parent: Option<String>,
    /// A Claude subagent or a Codex spawned thread.
    pub subagent: bool,
    pub cwd: Option<String>,
    pub requests: u64,
    /// A lower bound unless `complete`.
    pub cost_usd: Usd,
    /// False when a request could not be priced or a file had unreadable or unfinished lines.
    pub complete: bool,
    /// Unreadable lines in the session's files.
    pub skipped_lines: u64,
    /// Input tokens over every request, cached or not.
    pub input_tokens: i64,
    /// Null when a request left them unrecorded.
    pub cache_read_tokens: Option<i64>,
    /// Cache reads over input; null without input or recorded reads.
    pub cache_hit_ratio: Option<f64>,
    /// The largest prompt.
    pub peak_input_tokens: i64,
    /// The session's first prompt, before any work: its always-loaded instructions and tools.
    pub start_input_tokens: Option<i64>,
    pub compactions: u64,
    pub tool_calls: u64,
    /// Null where the harness records no tool errors.
    pub tool_errors: Option<u64>,
    pub streak: Option<Streak>,
    /// The session's first request.
    pub at: Pointer,
    /// The lines of the session's first requests in that file, to sample how it started.
    pub early_lines: Vec<u64>,
}

/// Sessions that stand out, one list per measure.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Shapes {
    pub costliest: Vec<Shape>,
    /// By input tokens not read from the cache.
    pub least_cached: Vec<Shape>,
    /// By input tokens over every request, the context the session kept carrying.
    pub most_input: Vec<Shape>,
    pub largest_start: Vec<Shape>,
    /// Subagents and spawned threads by cost.
    pub subagents: Vec<Shape>,
    pub most_tool_calls: Vec<Shape>,
    pub most_tool_errors: Vec<Shape>,
    pub longest_streaks: Vec<Shape>,
}

/// Everything the audit selected.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub sessions: u64,
    pub requests: u64,
    /// A lower bound when `incomplete` is above zero.
    pub cost_usd: Usd,
    /// Sessions whose cost is a lower bound: a request could not be priced, or a file had
    /// unreadable or unfinished lines.
    pub incomplete: u64,
}

/// One or more hosts' findings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Audit {
    pub totals: Totals,
    pub cache_misses: CacheMisses,
    pub tool_outputs: ToolOutputs,
    pub sessions: Shapes,
}

/// Keep the `limit` entries with the largest `key`, comparing its first figure, then its
/// second, and dropping entries whose figures are both zero.
fn by<T>(list: &mut Vec<T>, limit: usize, key: impl Fn(&T) -> (f64, f64)) {
    list.retain(|item| key(item) != (0.0, 0.0));
    // Stable, so ties keep each host's order, then the order hosts were selected in.
    list.sort_by(|a, b| {
        let (a, b) = (key(a), key(b));
        b.0.total_cmp(&a.0).then(b.1.total_cmp(&a.1))
    });
    list.truncate(limit);
}

/// A single-figure key.
fn one(figure: f64) -> (f64, f64) {
    (figure, 0.0)
}

impl Audit {
    /// Rank and cut every list to `limit`. Each host keeps every tool, since a tool's total
    /// adds across hosts; only the merge cuts them.
    fn rank(&mut self, limit: usize, tools: usize) {
        // Priced findings first by dollars; unpriced ones after them by tokens.
        by(&mut self.cache_misses.hotspots, limit, |m| {
            (m.cost_usd.map_or(0.0, Usd::to_f64), m.tokens as f64)
        });
        // Each host keeps every tool's total, zeros included, since totals add across hosts.
        if tools != usize::MAX {
            by(&mut self.tool_outputs.by_tool, tools, |t| {
                (t.cost_usd.to_f64(), t.carried_tokens as f64)
            });
        }
        by(&mut self.tool_outputs.hotspots, limit, |o| {
            one(o.carried_tokens as f64)
        });
        let s = &mut self.sessions;
        by(&mut s.costliest, limit, |s| one(s.cost_usd.to_f64()));
        by(&mut s.least_cached, limit, |s| {
            one(s.cache_read_tokens.map_or(0, |read| s.input_tokens - read) as f64)
        });
        by(&mut s.most_input, limit, |s| one(s.input_tokens as f64));
        by(&mut s.largest_start, limit, |s| {
            one(s.start_input_tokens.unwrap_or(0) as f64)
        });
        by(&mut s.subagents, limit, |s| {
            one(if s.subagent { s.cost_usd.to_f64() } else { 0.0 })
        });
        by(&mut s.most_tool_calls, limit, |s| one(s.tool_calls as f64));
        by(&mut s.most_tool_errors, limit, |s| {
            one(s.tool_errors.unwrap_or(0) as f64)
        });
        // A run of one call is no loop.
        by(&mut s.longest_streaks, limit, |s| {
            one(s
                .streak
                .as_ref()
                .map_or(0.0, |k| k.calls.saturating_sub(1) as f64))
        });
    }

    /// Name `host` on every pointer, for a destination's findings.
    pub fn on(&mut self, host: &str) {
        let name = |p: &mut Pointer| p.host = Some(host.to_string());
        for m in &mut self.cache_misses.hotspots {
            name(&mut m.at);
            name(&mut m.previous);
        }
        for o in &mut self.tool_outputs.hotspots {
            name(&mut o.at);
            if let Some(call) = &mut o.call {
                name(call);
            }
        }
        let s = &mut self.sessions;
        for shape in s
            .costliest
            .iter_mut()
            .chain(&mut s.least_cached)
            .chain(&mut s.most_input)
            .chain(&mut s.largest_start)
            .chain(&mut s.subagents)
            .chain(&mut s.most_tool_calls)
            .chain(&mut s.most_tool_errors)
            .chain(&mut s.longest_streaks)
        {
            name(&mut shape.at);
            if let Some(streak) = &mut shape.streak {
                name(&mut streak.at);
            }
        }
    }

    /// Whether every ranking holds at most `limit` entries, as a destination must send them;
    /// its tools are bounded by the stream's limit alone.
    pub fn within(&self, limit: usize) -> bool {
        let s = &self.sessions;
        [
            self.cache_misses.hotspots.len(),
            self.tool_outputs.hotspots.len(),
            s.costliest.len(),
            s.least_cached.len(),
            s.most_input.len(),
            s.largest_start.len(),
            s.subagents.len(),
            s.most_tool_calls.len(),
            s.most_tool_errors.len(),
            s.longest_streaks.len(),
        ]
        .iter()
        .all(|&n| n <= limit)
            && self.cache_misses.by_cause.len() <= Cause::ALL.len()
            && s.costliest
                .iter()
                .chain(&s.least_cached)
                .chain(&s.most_input)
                .chain(&s.largest_start)
                .chain(&s.subagents)
                .chain(&s.most_tool_calls)
                .chain(&s.most_tool_errors)
                .chain(&s.longest_streaks)
                .all(|shape| shape.early_lines.len() <= EARLY)
    }

    /// Several hosts' findings as one, in the order given, re-ranked and cut to `limit`.
    pub fn merge(audits: impl IntoIterator<Item = Audit>, limit: usize) -> Audit {
        let mut all = Audit::default();
        for cause in Cause::ALL {
            all.cache_misses.by_cause.insert(cause, Tally::default());
        }
        let mut tools: IndexMap<String, ToolTally> = IndexMap::default();
        for audit in audits {
            let t = &mut all.totals;
            t.sessions += audit.totals.sessions;
            t.requests += audit.totals.requests;
            t.cost_usd += audit.totals.cost_usd;
            t.incomplete += audit.totals.incomplete;
            let c = audit.cache_misses;
            all.cache_misses.total.add(&c.total);
            for (cause, tally) in c.by_cause {
                all.cache_misses
                    .by_cause
                    .entry(cause)
                    .or_default()
                    .add(&tally);
            }
            all.cache_misses.hotspots.extend(c.hotspots);
            for tool in audit.tool_outputs.by_tool {
                match tools.get_mut(&tool.tool) {
                    Some(sum) => {
                        sum.results += tool.results;
                        sum.tokens += tool.tokens;
                        sum.carried_requests += tool.carried_requests;
                        sum.carried_tokens += tool.carried_tokens;
                        sum.cost_usd += tool.cost_usd;
                        sum.unpriced += tool.unpriced;
                    }
                    None => {
                        tools.insert(tool.tool.clone(), tool);
                    }
                }
            }
            all.tool_outputs
                .hotspots
                .extend(audit.tool_outputs.hotspots);
            let (a, s) = (&mut all.sessions, audit.sessions);
            a.costliest.extend(s.costliest);
            a.least_cached.extend(s.least_cached);
            a.most_input.extend(s.most_input);
            a.largest_start.extend(s.largest_start);
            a.subagents.extend(s.subagents);
            a.most_tool_calls.extend(s.most_tool_calls);
            a.most_tool_errors.extend(s.most_tool_errors);
            a.longest_streaks.extend(s.longest_streaks);
        }
        all.tool_outputs.by_tool = tools.into_values().collect();
        all.rank(limit, limit);
        all
    }
}

/// Sessions that stand out, each listed once, and each measure's ranking as indexes into the list.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ranked {
    pub list: Vec<Shape>,
    pub costliest: Vec<usize>,
    pub least_cached: Vec<usize>,
    pub most_input: Vec<usize>,
    pub largest_start: Vec<usize>,
    pub subagents: Vec<usize>,
    pub most_tool_calls: Vec<usize>,
    pub most_tool_errors: Vec<usize>,
    pub longest_streaks: Vec<usize>,
}

impl Shapes {
    /// Each session once, by its first request, so a session that stands out on several
    /// measures is printed once.
    pub fn ranked(self) -> Ranked {
        let mut list: Vec<Shape> = vec![];
        let mut index = |ranking: Vec<Shape>| -> Vec<usize> {
            ranking
                .into_iter()
                .map(|shape| match list.iter().position(|s| s.at == shape.at) {
                    Some(at) => at,
                    None => {
                        list.push(shape);
                        list.len() - 1
                    }
                })
                .collect()
        };
        let costliest = index(self.costliest);
        let least_cached = index(self.least_cached);
        let most_input = index(self.most_input);
        let largest_start = index(self.largest_start);
        let subagents = index(self.subagents);
        let most_tool_calls = index(self.most_tool_calls);
        let most_tool_errors = index(self.most_tool_errors);
        let longest_streaks = index(self.longest_streaks);
        Ranked {
            list,
            costliest,
            least_cached,
            most_input,
            largest_start,
            subagents,
            most_tool_calls,
            most_tool_errors,
            longest_streaks,
        }
    }
}

/// How many of a session's first requests point at how it started.
const EARLY: usize = 5;

/// What one audit selects and how much of each list it keeps.
pub struct Options<'a> {
    pub filter: &'a Filter,
    pub since: Millis,
    pub limit: usize,
}

/// A request's main sampled part, which holds its prompt.
fn message(r: &Request) -> Option<&BillingPart> {
    r.parts
        .iter()
        .find(|p| p.kind == PartKind::Message)
        .or_else(|| r.parts.first())
}

/// Whether the reader could not read `r`'s record, so what it cached is unknown.
fn unreadable(r: &Request) -> bool {
    r.evidence == RequestEvidence::Unknown && r.parts.iter().all(|p| p.kind == PartKind::Unknown)
}

/// A request with recorded prompt usage, which a chain can compare.
struct Turn<'a> {
    request: &'a Request,
    part: &'a BillingPart,
    input: i64,
    /// None when the harness did not record it.
    read: Option<i64>,
    /// Each span of the prompt's price, from its start: the end in tokens and the price per
    /// token. None when unpriced.
    spans: Option<Vec<(f64, f64)>>,
    /// Whether the session replaying it owns it, rather than holding a fork's copy.
    owned: bool,
    /// Whether an unreadable record lies between it and the request before it, so no
    /// comparison reaches back.
    blind: bool,
}

impl Turn<'_> {
    /// The prompt tokens a successor may read from the cache.
    fn cached(&self) -> Option<i64> {
        match self.request.harness {
            Harness::Claude => self.part.usage.cached_prefix(),
            Harness::Codex => {
                let reusable = self.input - CODEX_CACHE_STEP;
                Some(match reusable < CODEX_CACHE_MINIMUM {
                    true => 0,
                    false => reusable - reusable % CODEX_CACHE_STEP,
                })
            }
        }
    }

    /// The price of prompt tokens `start..end`, and whether one price covers them all.
    fn price(&self, start: f64, end: f64) -> Option<(f64, bool)> {
        let spans = self.spans.as_ref()?;
        let (mut from, mut usd, mut spanned) = (0.0, 0.0, 0);
        for &(to, rate) in spans {
            let overlap = end.min(to) - start.max(from);
            if overlap > 0.0 {
                usd += overlap * rate;
                spanned += 1;
            }
            from = to;
        }
        Some((usd, spanned <= 1))
    }
}

/// A prompt's spans as ends from its start, rather than prefix_spans' lengths.
fn ends(spans: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    let mut at = 0.0;
    spans
        .into_iter()
        .map(|(tokens, rate)| {
            at += tokens;
            (at, rate)
        })
        .collect()
}

/// What one session's events say, each list in time order.
#[derive(Default)]
struct Events<'a> {
    starts: Vec<&'a StructuralEvent>,
    ends: Vec<&'a StructuralEvent>,
    compactions: Vec<&'a StructuralEvent>,
    /// Codex's records that a tool finished, which say whether it failed.
    done: Vec<&'a StructuralEvent>,
    /// The events the session owns, rather than holds as a fork's copy, by ID.
    owned: HashSet<&'a Id>,
    /// Each call's start and its first result, by the call's ID.
    calls: HashMap<&'a Id, (Option<&'a StructuralEvent>, Option<&'a StructuralEvent>)>,
}

impl<'a> Events<'a> {
    fn sort(&mut self) {
        self.starts.sort_by_key(|e| e.time);
        self.ends.sort_by_key(|e| e.time);
        self.compactions.sort_by_key(|e| e.time);
        for &start in &self.starts {
            if let Some(id) = &start.tool_id {
                self.calls.entry(id).or_default().0.get_or_insert(start);
            }
        }
        for &end in &self.ends {
            if let Some(id) = &end.tool_id {
                self.calls.entry(id).or_default().1.get_or_insert(end);
            }
        }
    }

    /// The calls started from `from` until before `to`.
    fn started(&self, from: Millis, to: Millis) -> &[&'a StructuralEvent] {
        let at = |time: Millis| self.starts.partition_point(|e| e.time < time);
        &self.starts[at(from)..at(to).max(at(from))]
    }

    /// Whether the session owns `e`, rather than holding a fork's copy.
    fn owns(&self, e: &StructuralEvent) -> bool {
        self.owned.contains(&e.id)
    }

    /// The start of the call `end` answers, and the tool's name; None when unrecorded.
    fn call(&self, end: &'a StructuralEvent) -> (Option<&'a StructuralEvent>, Option<&'a str>) {
        let start = end.tool_id.as_ref().and_then(|id| self.calls.get(id)?.0);
        let tool = end
            .tool
            .as_deref()
            .or_else(|| start.and_then(|s| s.tool.as_deref()));
        (start, tool)
    }
}

/// Whether a session is in scope, from its directory and recorded repository.
struct Scope<'a> {
    filter: &'a Filter,
    checkouts: HashMap<String, Option<Checkout>>,
}

impl Scope<'_> {
    fn covers(&mut self, cwd: Option<&str>, recorded: Option<&str>) -> Result<bool, ScanError> {
        let remotes = match self.filter {
            Filter::All => return Ok(true),
            Filter::Remotes(remotes) => Some(remotes),
            Filter::Common(_) => None,
        };
        if let (Some(remotes), Some(url)) = (remotes, recorded)
            && remotes.contains(&repository::canonical(url))
        {
            return Ok(true);
        }
        let Some(cwd) = cwd else { return Ok(false) };
        if !self.checkouts.contains_key(cwd) {
            let found = repository::locate(cwd)?;
            self.checkouts.insert(cwd.to_string(), found);
        }
        let Some(found) = &self.checkouts[cwd] else {
            return Ok(false);
        };
        Ok(match self.filter {
            Filter::All => true,
            Filter::Remotes(remotes) => found
                .urls
                .iter()
                .any(|u| remotes.contains(&repository::canonical(u))),
            Filter::Common(common) => &found.common == common,
        })
    }
}

/// The audit of one host's scan: its sessions in scope with a request since `since`.
///
/// A Codex fork copies its source's records, so each session replays every request and
/// event its files hold a copy of, while each is billed and counted once, to the session
/// that owns it, as the session export bills it.
pub fn audit(scan: &Scan, catalog: &Catalog, options: &Options) -> Result<Audit, ScanError> {
    let since = options.since;
    let mut scope = Scope {
        filter: options.filter,
        checkouts: HashMap::default(),
    };
    let owners = Owners::new(&scan.sessions);
    // Each session's requests, in the order read, which sorts below.
    let mut sessions: IndexMap<&str, Vec<&Request>> = IndexMap::default();
    for r in &scan.requests {
        for key in owners.holding(r) {
            sessions.entry(key).or_default().push(r);
        }
    }
    let snapshots: HashMap<&str, _> = scan
        .snapshots
        .iter()
        .map(|s| (s.file.as_str(), s))
        .collect();
    // An unreadable or unfinished line may have held usage: its unreadable lines, and
    // whether its cost is complete.
    let coverage = |key: &str| {
        let (lines, deferred) = scan
            .sessions
            .files(key)
            .iter()
            .filter_map(|file| snapshots.get(&**file))
            .fold((0, 0), |(lines, bytes), s| {
                (lines + s.skipped_lines, bytes + s.deferred_bytes)
            });
        (lines, lines == 0 && deferred == 0)
    };
    let mut in_scope = |key: &str, requests: &[&Request]| {
        let cwd = scan
            .sessions
            .cwd(key)
            .map(|c| &**c)
            .or_else(|| requests.iter().find_map(|r| r.cwd.as_deref()));
        scope.covers(cwd, scan.sessions.recorded_repository(key))
    };
    let mut covered: HashSet<&str> = HashSet::default();
    for (&key, requests) in &sessions {
        let owned = |r: &&&Request| r.time >= since && &**owners.of(r) == key;
        if requests.iter().any(|r| owned(&r)) && in_scope(key, requests)? {
            covered.insert(key);
        }
    }
    let mut audit = Audit::default();
    // A session with no readable request in the window may still have lost one to an
    // unreadable line.
    for key in scan.sessions.keys() {
        if !covered.contains(&**key) && !coverage(key).1 && in_scope(key, &[])? {
            audit.totals.incomplete += 1;
        }
    }
    sessions.retain(|key, _| covered.contains(key));
    let mut events: HashMap<&str, Events> = HashMap::default();
    for e in &scan.events {
        let held = || std::iter::once(&e.session).chain(e.origins.iter().map(|o| &o.session));
        // A Codex completion names its thread, which owns it even when an earlier session's
        // rollout also records it.
        let owner = match &e.thread {
            Some(thread) => Some(thread),
            None => owners.first(held()),
        };
        let mut seen: Vec<&str> = vec![];
        for key in held() {
            if !covered.contains(&**key) || seen.contains(&&**key) {
                continue;
            }
            seen.push(key);
            let found = events.entry(key).or_default();
            if owner == Some(key) {
                found.owned.insert(&e.id);
            }
            match e.kind {
                EventKind::ToolStart => found.starts.push(e),
                EventKind::ToolEnd => found.ends.push(e),
                EventKind::Compaction => found.compactions.push(e),
                EventKind::ToolDone => found.done.push(e),
                _ => {}
            }
        }
    }
    for cause in Cause::ALL {
        audit.cache_misses.by_cause.insert(cause, Tally::default());
    }
    let mut results: IndexMap<&Id, Carried> = IndexMap::default();
    for (key, mut requests) in sessions {
        requests.sort_by_key(|r| r.time);
        let mut found = events.remove(key).unwrap_or_default();
        found.sort();
        let found = &found;
        let (id, parent) = scan.sessions.native(key);
        let native = id.map(str::to_string);
        let mut blind = false;
        let turns: Vec<Turn> = requests
            .iter()
            .filter_map(|&r| {
                let part = message(r).filter(|_| r.evidence != RequestEvidence::Unknown);
                let turn = part.and_then(|part| {
                    Some(Turn {
                        request: r,
                        part,
                        input: part.usage.input?,
                        read: part.usage.read,
                        spans: prefix_spans(catalog, r, part).map(ends),
                        owned: &**owners.of(r) == key,
                        blind,
                    })
                });
                // An attempt that never answered, such as one retried after an API error,
                // cached nothing, so the comparison reaches past it.
                blind = match &turn {
                    Some(_) => false,
                    None => blind || unreadable(r),
                };
                turn
            })
            .collect();
        misses(&turns, found, since, &native, &mut audit.cache_misses);
        outputs(&turns, found, since, &native, &mut results);
        let owned: Vec<&Request> = requests
            .iter()
            .copied()
            .filter(|r| &**owners.of(r) == key)
            .collect();
        let named = Named {
            session: native,
            parent: parent.map(str::to_string),
            cwd: scan.sessions.cwd(key).map(|c| c.to_string()),
            // A fork's first prompt is the history it copied, not how it started.
            created: scan
                .sessions
                .created(key)
                .filter(|_| owned.len() == requests.len()),
        };
        let mut shape = shape(&owned, &turns, found, since, catalog, named);
        let (lines, complete) = coverage(key);
        shape.skipped_lines = lines;
        shape.complete &= complete;
        let t = &mut audit.totals;
        t.sessions += 1;
        t.requests += shape.requests;
        t.cost_usd += shape.cost_usd;
        t.incomplete += u64::from(!shape.complete);
        let s = &mut audit.sessions;
        for list in [
            &mut s.costliest,
            &mut s.least_cached,
            &mut s.most_input,
            &mut s.largest_start,
            &mut s.subagents,
            &mut s.most_tool_calls,
            &mut s.most_tool_errors,
            &mut s.longest_streaks,
        ] {
            list.push(shape.clone());
        }
    }
    let mut tools: IndexMap<String, ToolTally> = IndexMap::default();
    for result in results.into_values() {
        let tally = tools
            .entry(result.tool.clone())
            .or_insert_with(|| ToolTally {
                tool: result.tool.clone(),
                results: 0,
                tokens: 0,
                carried_requests: 0,
                carried_tokens: 0,
                cost_usd: Usd::ZERO,
                unpriced: 0,
            });
        let carried = result.carried as i64;
        tally.carried_requests += result.carried;
        tally.carried_tokens += result.tokens * carried;
        tally.cost_usd += Usd::from_f64(result.usd);
        tally.unpriced += u64::from(!result.priced);
        // A result whose owner is out of scope adds only its in-scope copies' carriers.
        let Some(session) = result.session else {
            continue;
        };
        tally.results += 1;
        tally.tokens += result.tokens;
        let end = result.end;
        audit.tool_outputs.hotspots.push(Output {
            tool: result.tool,
            bytes: result.bytes,
            tokens: result.tokens,
            carried_requests: result.carried,
            carried_tokens: result.tokens * carried,
            cost_usd: (result.priced && result.alone).then(|| Usd::from_f64(result.usd)),
            error: (end.harness == Harness::Claude).then_some(end.file.is_none()),
            harness: end.harness,
            session,
            call: result.call.map(|s| Pointer::at(&s.location)),
            at: Pointer::at(&end.location),
        });
    }
    audit.tool_outputs.by_tool = tools.into_values().collect();
    audit.rank(options.limit, usize::MAX);
    Ok(audit)
}

/// The speed and service tier a Codex request asked for, with each tier's aliases as one.
fn requested(r: &Request) -> (Option<&str>, Option<&str>) {
    let asked = r.requested_speed.as_ref();
    (
        asked.and_then(|a| a.tier.as_deref()).map(tier_name),
        asked.and_then(|a| a.speed.as_deref()),
    )
}

/// Each request that lost cache reuse against its predecessor.
fn misses(
    turns: &[Turn],
    events: &Events,
    since: Millis,
    session: &Option<String>,
    found: &mut CacheMisses,
) {
    let mut codex_ttl = CODEX_CACHE_LIFETIME;
    for (i, pair) in turns.windows(2).enumerate() {
        let (prev, cur) = (&pair[0], &pair[1]);
        let r = cur.request;
        // Each Codex write records how long it is kept. A request that read none of its
        // prompt rewrote it all, so its writes' lifetime replaces the one before; one that
        // read some keeps the longer for the part it read.
        let u = &prev.part.usage;
        let written = if u.write12h.unwrap_or(0) > 0 {
            CODEX_LONG_LIFETIME
        } else if u.write30m.unwrap_or(0) > 0 || u.write.unwrap_or(0) > 0 {
            CODEX_CACHE_MILLISECONDS
        } else {
            CODEX_CACHE_LIFETIME
        };
        codex_ttl = match prev.read {
            Some(read) if read > 0 => codex_ttl.max(written),
            _ => written,
        };
        if r.time < since || !cur.owned || cur.blind {
            continue;
        }
        let (Some(cached), Some(read)) = (prev.cached(), cur.read) else {
            continue;
        };
        let lost = (cached - read).max(0).min(cur.input - read);
        if lost <= 0 {
            continue;
        }
        let cost = cur
            .price(read as f64, (read + lost) as f64)
            .and_then(|(usd, _)| {
                let read = cur.spans.as_ref()?.first()?.1;
                Some(Usd::from_f64((usd - lost as f64 * read).max(0.0)))
            });
        let (p, q) = (prev.part, cur.part);
        let compacted = events
            .compactions
            .iter()
            .any(|e| e.time > prev.request.time && e.time <= r.time);
        let change = if p.model != q.model {
            Some(Change::Model)
        } else if prev.request.effort != r.effort {
            Some(Change::Effort)
        } else if p.speed != q.speed
            || (r.harness == Harness::Codex && requested(prev.request) != requested(r))
        {
            Some(Change::Speed)
        } else if compacted {
            Some(Change::Compaction)
        } else if cur.input < cached {
            Some(Change::Truncation)
        } else {
            None
        };
        let ttl = match r.harness {
            Harness::Claude => ttl_milliseconds(
                observed_ttl(p)
                    .or_else(|| observed_ttl(q))
                    .unwrap_or(Ttl::FiveMinutes),
            ),
            Harness::Codex => codex_ttl,
        };
        let began = prev.request.time;
        let gap = r.time - began;
        // The calls whose results `r` waited on: `prev`'s. Claude records a request before
        // its response's calls, Codex after them.
        let calls = match r.harness {
            Harness::Claude => events.started(began, r.time),
            Harness::Codex => {
                // After the request before `prev`, and up to `prev`, whose usage can share
                // its last call's millisecond.
                let before = i
                    .checked_sub(1)
                    .map_or(Millis::MIN, |j| turns[j].request.time);
                events.started(before + 1, began + 1)
            }
        };
        let waited = calls.iter().any(|start| {
            let end = start
                .tool_id
                .as_ref()
                .and_then(|id| events.calls.get(id)?.1);
            end.is_some_and(|end| end.time - began >= ttl)
        });
        let cause = match change {
            Some(_) => Cause::ControlChange,
            None if gap >= ttl && waited => Cause::ToolWaitTtl,
            None if gap >= ttl => Cause::IdleTtl,
            None => Cause::PrefixBust,
        };
        let mut tools: Vec<String> = vec![];
        for start in calls {
            if let Some(tool) = start.tool.as_deref()
                && !tools.iter().any(|t| t == tool)
            {
                tools.push(tool.to_string());
            }
        }
        found.total.count(lost, cost);
        found.by_cause.entry(cause).or_default().count(lost, cost);
        let (Some(at), Some(previous)) = (r.locations.first(), prev.request.locations.first())
        else {
            continue;
        };
        found.hotspots.push(Miss {
            cause,
            change,
            cost_usd: cost,
            tokens: lost,
            gap_ms: gap,
            ttl_ms: ttl,
            tools,
            harness: r.harness,
            session: session.clone(),
            at: Pointer::at(at),
            previous: Pointer::at(previous),
        });
    }
}

/// One tool result and what the requests carrying it paid for it, over every session that
/// holds a copy: a Codex fork carries its source's results in its own later prompts.
struct Carried<'a> {
    end: &'a StructuralEvent,
    tool: String,
    bytes: u32,
    tokens: i64,
    /// The call, as the owning session recorded it.
    call: Option<&'a StructuralEvent>,
    /// The owning session's ID, once it replays the result; None while only copies have.
    session: Option<Option<String>>,
    carried: u64,
    /// The priced carriers' sum, whether every carrier was priced, and whether each priced
    /// carrier holds the result at one price.
    usd: f64,
    priced: bool,
    alone: bool,
}

/// Each tool result since `since` in one session's replay, and what the requests that
/// carried it paid for it, added to `found` by result.
fn outputs<'a>(
    turns: &[Turn],
    events: &Events<'a>,
    since: Millis,
    session: &Option<String>,
    found: &mut IndexMap<&'a Id, Carried<'a>>,
) {
    // Where the next result of each calling request begins in later prompts.
    let mut placed: HashMap<usize, f64> = HashMap::default();
    for &end in &events.ends {
        let Some(bytes) = end.bytes else {
            continue;
        };
        let (start, tool) = events.call(end);
        let tokens = (f64::from(bytes) * TOKENS_PER_BYTE).round();
        // The request that called the tool. Claude records a request before the calls its
        // response makes, so it is the last before the call, or before the result. Codex
        // records a request's usage after its calls, so it is the first at or after the call.
        let calling = match start {
            Some(s) if end.harness == Harness::Codex => {
                Some(turns.partition_point(|t| t.request.time < s.time))
                    .filter(|&at| at < turns.len())
            }
            _ => {
                let called = start.map_or(end.time, |s| s.time);
                turns
                    .partition_point(|t| t.request.time <= called)
                    .checked_sub(1)
            }
        };
        // A result before the window still takes its place after its caller's prompt, so
        // the same response's later results sit after it.
        let span = calling.map(|index| {
            let caller = &turns[index];
            let output = message(caller.request)
                .and_then(|p| p.usage.output)
                .unwrap_or(0);
            let from = *placed
                .entry(index)
                .or_insert((caller.input + output) as f64);
            let to = from + tokens;
            placed.insert(index, to);
            (index, from, to)
        });
        if end.time < since {
            continue;
        }
        let result = found.entry(&end.id).or_insert_with(|| Carried {
            end,
            tool: tool.unwrap_or("unrecorded").to_string(),
            bytes,
            tokens: tokens as i64,
            call: start,
            session: None,
            carried: 0,
            usd: 0.0,
            priced: true,
            alone: true,
        });
        if events.owns(end) {
            result.tool = tool.unwrap_or("unrecorded").to_string();
            result.call = start;
            result.session = Some(session.clone());
        }
        let Some((index, from, to)) = span else {
            continue;
        };
        // Compaction replaces the history the result was in.
        let compacted = events
            .compactions
            .get(events.compactions.partition_point(|e| e.time <= end.time))
            .map_or(Millis::MAX, |e| e.time);
        for later in &turns[index + 1..] {
            if later.request.time < end.time {
                continue;
            }
            // A prompt too short to hold the result no longer does, as after it was cleared;
            // a fork's copied prompt clears it in the fork too.
            if later.request.time >= compacted || (later.input as f64) < to {
                break;
            }
            // A fork's copy is billed to its source, which carried it there.
            if !later.owned {
                continue;
            }
            result.carried += 1;
            match later.price(from, to) {
                Some((price, one)) => {
                    result.usd += price;
                    result.alone &= one;
                }
                None => result.priced = false,
            }
        }
    }
}

/// What a session's records name it.
struct Named {
    session: Option<String>,
    parent: Option<String>,
    cwd: Option<String>,
    /// When the session began, if it did on its own rather than as a fork's copy.
    created: Option<Millis>,
}

/// One session's shape since `since`.
fn shape(
    requests: &[&Request],
    turns: &[Turn],
    events: &Events,
    since: Millis,
    catalog: &Catalog,
    named: Named,
) -> Shape {
    let Named {
        session,
        parent,
        cwd,
        created,
    } = named;
    let harness = requests[0].harness;
    let first = requests
        .iter()
        .find_map(|r| r.locations.first())
        .expect("every request has a location");
    let selected: Vec<&Request> = requests
        .iter()
        .copied()
        .filter(|r| r.time >= since)
        .collect();
    let (mut cost, mut complete) = (Usd::ZERO, true);
    for r in &selected {
        let priced = price_request(catalog, r);
        cost += priced.known_usd;
        complete &= priced.total_usd.is_some();
    }
    let window: Vec<&Turn> = turns
        .iter()
        .filter(|t| t.owned && t.request.time >= since)
        .collect();
    let input: i64 = window.iter().map(|t| t.input).sum();
    // Unknown when any request left its cache reads unrecorded.
    let read: Option<i64> = window.iter().map(|t| t.read).sum();
    // A fork's copies of its source's calls, results and compactions are its source's work.
    let calls: Vec<&StructuralEvent> = events
        .started(since, Millis::MAX)
        .iter()
        .copied()
        .filter(|e| events.owns(e))
        .collect();
    let mut streak: Option<Streak> = None;
    let mut run: Option<(&str, u64, &Location)> = None;
    for call in &calls {
        let tool = call.tool.as_deref().unwrap_or("unrecorded");
        run = match run {
            Some((name, n, at)) if name == tool => Some((name, n + 1, at)),
            _ => Some((tool, 1, &call.location)),
        };
        let (name, n, at) = run.expect("set above");
        if streak.as_ref().is_none_or(|s| n > s.calls) {
            streak = Some(Streak {
                tool: name.to_string(),
                calls: n,
                at: Pointer::at(at),
            });
        }
    }
    // A failed result, as Claude flags it on the result and Codex on its record that the
    // tool finished. Unknown for a Codex session that recorded no such record.
    let finished = match harness {
        Harness::Claude => &events.ends,
        Harness::Codex => &events.done,
    };
    let finished: Vec<&StructuralEvent> = finished
        .iter()
        .copied()
        .filter(|e| e.time >= since && events.owns(e))
        .collect();
    let errors = (harness == Harness::Claude || !finished.is_empty())
        .then(|| finished.iter().filter(|e| e.file.is_none()).count() as u64);
    Shape {
        harness,
        session,
        subagent: parent.is_some() || requests.iter().any(|r| r.agent == Agent::Subagent),
        parent,
        cwd,
        requests: selected.len() as u64,
        cost_usd: cost,
        complete,
        skipped_lines: 0,
        input_tokens: input,
        cache_read_tokens: read,
        cache_hit_ratio: read
            .filter(|_| input > 0)
            .map(|read| read as f64 / input as f64),
        peak_input_tokens: window.iter().map(|t| t.input).max().unwrap_or(0),
        // Only when the session began in the window, so the first prompt is its start.
        start_input_tokens: turns
            .first()
            .filter(|t| t.owned && created.is_some_and(|c| c >= since))
            .map(|t| t.input),
        compactions: events
            .compactions
            .iter()
            .filter(|e| e.time >= since && events.owns(e))
            .count() as u64,
        tool_calls: calls.len() as u64,
        tool_errors: errors,
        streak,
        at: Pointer::at(first),
        early_lines: requests
            .iter()
            .filter_map(|r| r.locations.first())
            .filter(|l| l.file == first.file)
            .take(EARLY)
            .map(|l| l.line)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::scan;
    use crate::test_support::{Workspace, prices};
    use serde_json::{Value, json};

    const MIN: Millis = 60_000;
    /// 2026-11-01T00:00:00Z.
    const START: Millis = 1_793_491_200_000;
    const OPUS: &str = "claude-opus-5-5";

    fn at(minute: f64) -> String {
        jiff::Timestamp::from_millisecond(START + (minute * MIN as f64) as Millis)
            .unwrap()
            .to_string()
    }

    /// A main-agent request reading `read` and writing `write` 5m tokens, calling `tools`.
    fn assistant(
        minute: f64,
        id: &str,
        model: &str,
        read: i64,
        write: i64,
        tools: &[(&str, &str)],
    ) -> Value {
        let mut content = vec![json!({"type": "text", "text": "SYNTHETIC_PRIVATE_REPLY"})];
        for (name, call) in tools {
            content.push(json!({"type": "tool_use", "id": call, "name": name, "input": {"command": "SYNTHETIC_PRIVATE_INPUT"}}));
        }
        json!({
            "type": "assistant",
            "sessionId": "session",
            "timestamp": at(minute),
            "message": {
                "id": id,
                "model": model,
                "content": content,
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 100,
                    "cache_read_input_tokens": read,
                    "cache_creation_input_tokens": write,
                    "cache_creation": {"ephemeral_5m_input_tokens": write, "ephemeral_1h_input_tokens": 0},
                    "service_tier": "standard",
                    "speed": "standard",
                    "inference_geo": "global",
                },
            },
        })
    }

    /// A tool result of `bytes` bytes of text.
    fn result(minute: f64, call: &str, bytes: usize, error: bool) -> Value {
        let text = format!("SYNTHETIC_PRIVATE_OUTPUT{}", "x".repeat(bytes - 24));
        json!({
            "type": "user",
            "sessionId": "session",
            "timestamp": at(minute),
            "message": {"content": [{"type": "tool_result", "tool_use_id": call, "content": [{"type": "text", "text": text}], "is_error": error}]},
        })
    }

    fn compaction(minute: f64) -> Value {
        json!({"type": "system", "subtype": "compact_boundary", "uuid": format!("boundary-{minute}"), "sessionId": "session", "timestamp": at(minute), "compactMetadata": {"trigger": "auto", "preTokens": 100_000, "postTokens": 2_000}})
    }

    fn audit_of(w: &Workspace, records: &[(&str, Vec<Value>)]) -> Audit {
        audit_since(w, records, START)
    }

    fn audit_since(w: &Workspace, records: &[(&str, Vec<Value>)], since: Millis) -> Audit {
        for (file, lines) in records {
            let path = w.home.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let text: String = lines.iter().map(|r| format!("{r}\n")).collect();
            std::fs::write(path, text).unwrap();
        }
        // As the command scans: its window, replaying every record before it.
        let mut scanning = w.scan_options();
        scanning.since = Some(since);
        scanning.reach = crate::reader::Reach::Replay;
        let scan = scan(&scanning).unwrap();
        let options = Options {
            filter: &Filter::All,
            since,
            limit: 10,
        };
        let audit = audit(&scan, &prices(), &options).unwrap();
        let text = serde_json::to_string(&audit).unwrap();
        assert!(!text.contains("SYNTHETIC_PRIVATE"), "{text}");
        audit
    }

    fn close(a: Option<Usd>, b: f64) -> bool {
        a.is_some_and(|a| (a.to_f64() - b).abs() < 1e-12)
    }

    const READ: f64 = 0.2e-6;
    const WRITE: f64 = 5e-6;

    #[test]
    fn each_lost_prefix_is_priced_once_and_named_by_its_cause() {
        let w = Workspace::new();
        let records = vec![
            assistant(0.0, "a", OPUS, 0, 100_000, &[("Bash", "wait")]),
            // The Bash call outlasts the 5m cache, so its successor rewrites the prefix.
            result(6.0, "wait", 100, false),
            assistant(6.1, "b", OPUS, 0, 100_500, &[]),
            assistant(7.0, "c", OPUS, 100_500, 200, &[]),
            // An idle gap: the prefix is rewritten whole, so read + write grows.
            assistant(20.0, "d", OPUS, 0, 100_800, &[]),
            // Within the lifetime, with nothing changed.
            assistant(20.5, "e", OPUS, 0, 100_900, &[]),
            assistant(21.0, "f", "claude-fable-5-2", 0, 101_000, &[]),
            compaction(22.0),
            // Only the overlap with what the request sent again counts.
            assistant(22.1, "g", "claude-fable-5-2", 0, 5_000, &[]),
        ];
        let audit = audit_of(&w, &[(".claude/projects/project/main.jsonl", records)]);
        let c = &audit.cache_misses;
        let causes: Vec<(Cause, Option<Change>, i64)> = c
            .hotspots
            .iter()
            .map(|m| (m.cause, m.change, m.tokens))
            .collect();
        // Ranked by dollars: Fable's rates put its rewrite first and its 5,000 tokens last.
        assert_eq!(
            causes,
            [
                (Cause::ControlChange, Some(Change::Model), 100_900),
                (Cause::PrefixBust, None, 100_800),
                (Cause::IdleTtl, None, 100_700),
                (Cause::ToolWaitTtl, None, 100_000),
                (Cause::ControlChange, Some(Change::Compaction), 5_000),
            ]
        );
        let wait = &c.hotspots[3];
        assert!(close(wait.cost_usd, 100_000.0 * (WRITE - READ)));
        assert_eq!(wait.tools, ["Bash"]);
        assert_eq!(
            (wait.gap_ms, wait.ttl_ms),
            ((6.1 * MIN as f64) as Millis, 5 * MIN)
        );
        assert_eq!((wait.previous.line, wait.at.line), (1, 3));
        assert_eq!(wait.session.as_deref(), Some("session"));
        assert_eq!(c.total.events, 5);
        assert_eq!(c.by_cause[&Cause::ControlChange].events, 2);
        assert_eq!(c.by_cause[&Cause::PrefixBust].tokens, 100_800);
        assert_eq!(c.total.unpriced, 0);
    }

    #[test]
    fn the_first_request_in_the_window_is_compared_with_the_one_before_it() {
        // Resumed the next morning, past any cache's lifetime and the window's start.
        const NIGHT: f64 = 15.0 * 60.0;
        let w = Workspace::new();
        let records = vec![
            assistant(0.0, "a", OPUS, 0, 100_000, &[]),
            assistant(NIGHT, "b", OPUS, 0, 100_500, &[]),
        ];
        let file = ".claude/projects/project/main.jsonl";
        let since = START + 14 * 60 * MIN;
        let audit = audit_since(&w, &[(file, records)], since);
        let misses = &audit.cache_misses.hotspots;
        assert_eq!(misses.len(), 1);
        assert_eq!((misses[0].previous.line, misses[0].tokens), (1, 100_000));
        assert_eq!(misses[0].cause, Cause::IdleTtl);
        let shape = &audit.sessions.costliest[0];
        // It began before the window, so its first prompt there is no start.
        assert_eq!((shape.requests, shape.start_input_tokens), (1, None));
        assert_eq!(shape.at.line, 1);

        // Codex too, whose reader keeps no usage before the scan's window.
        let w = Workspace::new();
        let records = vec![
            codex_meta("thread", None),
            codex_turn("gpt-5.4"),
            codex_billed(0.0, "a", 10_000, 0),
            codex_billed(NIGHT, "b", 10_500, 0),
        ];
        let file = ".codex/sessions/rollout.jsonl";
        let audit = audit_since(&w, &[(file, records)], since);
        let misses = &audit.cache_misses.hotspots;
        assert_eq!((misses.len(), misses[0].previous.line), (1, 3));
        assert_eq!(audit.sessions.costliest[0].start_input_tokens, None);
    }

    #[test]
    fn a_malformed_record_before_the_window_leaves_its_successor_uncompared() {
        let w = Workspace::new();
        let mut bad = assistant(1.0, "bad", OPUS, 0, 100_000, &[]);
        bad["message"]["usage"]["input_tokens"] = json!("SYNTHETIC_PRIVATE_BAD");
        let claude = vec![
            assistant(0.0, "a", OPUS, 0, 100_000, &[]),
            bad,
            assistant(10.0, "b", OPUS, 0, 100_500, &[]),
            // An attempt that never answered cached nothing, so it hides no predecessor.
            json!({"type": "api-request", "sessionId": "session", "timestamp": at(15.0), "id": "attempt", "params": {"model": OPUS, "max_tokens": 1_000}}),
            assistant(20.0, "c", OPUS, 0, 100_900, &[]),
        ];
        let mut bad = codex_billed(1.0, "bad", 10_000, 0);
        bad["payload"]["usage"]["input_tokens"] = json!("SYNTHETIC_PRIVATE_BAD");
        let codex = vec![
            codex_meta("thread", None),
            codex_turn("gpt-5.4"),
            codex_billed(0.0, "a", 10_000, 0),
            bad,
            codex_billed(10.0, "b", 10_500, 0),
        ];
        let audit = audit_since(
            &w,
            &[
                (".claude/projects/project/main.jsonl", claude),
                (".codex/sessions/rollout.jsonl", codex),
            ],
            START + 5 * MIN,
        );
        // Only Claude's second request in the window has a known predecessor.
        let misses = &audit.cache_misses.hotspots;
        assert_eq!(misses.len(), 1, "{misses:?}");
        assert_eq!((misses[0].previous.line, misses[0].at.line), (3, 5));
        // The unanswered attempt is a request too.
        assert_eq!(audit.totals.requests, 4);
    }

    fn codex_meta(id: &str, forked_from: Option<&str>) -> Value {
        json!({"type": "session_meta", "timestamp": at(0.0), "payload": {"id": id, "forked_from_id": forked_from, "model_provider": "openai", "cli_version": "0.167.0", "originator": "codex_cli_rs", "source": "cli"}})
    }

    fn codex_turn(model: &str) -> Value {
        json!({"type": "turn_context", "timestamp": at(0.0), "payload": {"model": model, "turn_id": "turn", "service_tier": "standard"}})
    }

    fn codex_billed(minute: f64, id: &str, input: i64, cached: i64) -> Value {
        json!({
            "type": "token_usage_record",
            "timestamp": at(minute),
            "payload": {
                "response_id": id,
                "turn_id": "turn",
                "usage": {"input_tokens": input, "output_tokens": 10, "cached_input_tokens": cached},
            },
        })
    }

    #[test]
    fn a_codex_fork_replays_its_copied_history_but_bills_it_to_the_source() {
        let w = Workspace::new();
        let history = vec![
            codex_turn("gpt-5.4"),
            // Codex records a response's call, then its usage, then the call's result.
            json!({"type": "response_item", "timestamp": at(0.9), "payload": {"type": "function_call", "call_id": "call", "name": "exec", "arguments": "SYNTHETIC_PRIVATE_INPUT"}}),
            codex_billed(1.0, "a", 10_000, 0),
            json!({"type": "response_item", "timestamp": at(1.6), "payload": {"type": "function_call_output", "call_id": "call", "output": format!("SYNTHETIC_PRIVATE_OUTPUT{}", "x".repeat(3_976))}}),
            codex_billed(2.0, "b", 11_200, 9_856),
        ];
        let mut source = vec![codex_meta("source", None)];
        source.extend(history.clone());
        let mut fork = vec![
            json!({"type": "session_meta", "timestamp": at(3.0), "payload": {"id": "fork", "forked_from_id": "source", "model_provider": "openai", "cli_version": "0.167.0", "originator": "codex_cli_rs", "source": "cli"}}),
        ];
        fork.extend(history);
        // The fork's own request reads none of the prompt it copied.
        fork.push(codex_billed(4.0, "c", 12_000, 0));
        let audit = audit_of(
            &w,
            &[
                // Read first, so the reader first sees the copies under the fork.
                (".codex/sessions/a-fork.jsonl", fork),
                (".codex/sessions/b-source.jsonl", source),
            ],
        );
        assert_eq!((audit.totals.sessions, audit.totals.requests), (2, 3));
        let misses = &audit.cache_misses.hotspots;
        assert_eq!(misses.len(), 1, "{misses:?}");
        assert_eq!(
            (misses[0].session.as_deref(), misses[0].tokens),
            (Some("fork"), 11_008)
        );
        let exec = &audit.tool_outputs.by_tool[0];
        // Counted once, and carried by the source's request and the fork's own.
        assert_eq!((exec.results, exec.carried_requests), (1, 2));
        let output = &audit.tool_outputs.hotspots[0];
        assert_eq!(
            (output.session.as_deref(), output.carried_requests),
            (Some("source"), 2)
        );
        // The fork made no call of its own.
        let calls: Vec<_> = audit
            .sessions
            .most_tool_calls
            .iter()
            .map(|s| (s.session.as_deref(), s.tool_calls))
            .collect();
        assert_eq!(calls, [(Some("source"), 1)]);
    }

    #[test]
    fn a_copied_prompt_too_short_to_hold_a_result_ends_it_in_the_fork() {
        let w = Workspace::new();
        let history = vec![
            codex_turn("gpt-5.4"),
            // Codex records a response's call, then its usage, then the call's result.
            json!({"type": "response_item", "timestamp": at(0.9), "payload": {"type": "function_call", "call_id": "call", "name": "exec", "arguments": "SYNTHETIC_PRIVATE_INPUT"}}),
            codex_billed(1.0, "a", 10_000, 0),
            json!({"type": "response_item", "timestamp": at(1.6), "payload": {"type": "function_call_output", "call_id": "call", "output": format!("SYNTHETIC_PRIVATE_OUTPUT{}", "x".repeat(3_976))}}),
            // Too short to hold the result, as after the history was cleared.
            codex_billed(2.0, "b", 2_000, 0),
        ];
        let mut source = vec![codex_meta("source", None)];
        source.extend(history.clone());
        let mut fork = vec![
            json!({"type": "session_meta", "timestamp": at(3.0), "payload": {"id": "fork", "forked_from_id": "source", "model_provider": "openai", "cli_version": "0.167.0", "originator": "codex_cli_rs", "source": "cli"}}),
        ];
        fork.extend(history);
        // The fork's own prompt grows past the result's place again.
        fork.push(codex_billed(4.0, "c", 12_000, 1_920));
        let audit = audit_of(
            &w,
            &[
                (".codex/sessions/a-fork.jsonl", fork),
                (".codex/sessions/b-source.jsonl", source),
            ],
        );
        let exec = &audit.tool_outputs.by_tool[0];
        assert_eq!((exec.results, exec.carried_requests), (1, 0));
    }

    #[test]
    fn a_result_is_priced_where_it_sits_until_a_prompt_cannot_hold_it() {
        let w = Workspace::new();
        let records = vec![
            assistant(0.0, "a", OPUS, 0, 10_000, &[("Read", "read")]),
            // 1,000 tokens, after the caller's 10,000 and its 100 output: 10,100 to 11,100.
            result(0.5, "read", 4_000, false),
            // Read to 10,500 and written past it, so the result spans two prices.
            assistant(1.0, "b", OPUS, 10_500, 1_200, &[]),
            assistant(2.0, "c", OPUS, 11_700, 50, &[]),
            // A model with no public price carries it too.
            assistant(2.5, "u", "claude-unpriced-9", 11_750, 0, &[]),
            compaction(3.0),
            // Long enough to hold it, but compaction replaced the history it was in.
            assistant(3.1, "d", OPUS, 0, 20_000, &[]),
        ];
        let audit = audit_of(&w, &[(".claude/projects/project/main.jsonl", records)]);
        let t = &audit.tool_outputs;
        let output = &t.hotspots[0];
        assert_eq!(
            (output.tool.as_str(), output.bytes, output.tokens),
            ("Read", 4_000, 1_000)
        );
        assert_eq!((output.carried_requests, output.carried_tokens), (3, 3_000));
        assert_eq!(
            output.cost_usd, None,
            "its first carrier splits it across two prices"
        );
        assert_eq!(output.error, Some(false));
        assert_eq!((output.call.as_ref().unwrap().line, output.at.line), (1, 2));
        let read = &t.by_tool[0];
        let usd = 400.0 * READ + 600.0 * WRITE + 1_000.0 * READ;
        // The priced carriers' charges stay, beside the unpriced one.
        assert!(close(Some(read.cost_usd), usd), "{read:?}");
        assert_eq!(
            (read.results, read.carried_requests, read.unpriced),
            (1, 3, 1)
        );
    }

    #[test]
    fn a_result_after_the_window_starts_sits_after_its_responses_earlier_results() {
        let w = Workspace::new();
        let records = vec![
            assistant(
                0.0,
                "a",
                OPUS,
                0,
                10_000,
                &[("Read", "one"), ("Read", "two")],
            ),
            // 10,100 to 11,100, before the window.
            result(0.5, "one", 4_000, false),
            // 11,100 to 12,100, in it.
            result(1.5, "two", 4_000, false),
            // Read to 11,000, so the second result sits wholly in the write.
            assistant(2.0, "b", OPUS, 11_000, 1_200, &[]),
        ];
        let file = ".claude/projects/project/main.jsonl";
        let audit = audit_since(&w, &[(file, records)], START + MIN);
        let output = &audit.tool_outputs.hotspots;
        assert_eq!(output.len(), 1, "{output:?}");
        assert_eq!((output[0].at.line, output[0].carried_requests), (3, 1));
        assert!(close(output[0].cost_usd, 1_000.0 * WRITE), "{output:?}");
    }

    #[test]
    fn codex_results_carried_uncached_pay_ordinary_input_and_a_step_short_is_no_miss() {
        let w = Workspace::new();
        let billed = |second: u32, input: i64, cached: i64| {
            json!({
                "type": "token_usage_record",
                "timestamp": at(f64::from(second) / 60.0),
                "payload": {
                    "response_id": format!("response-{second}"),
                    "turn_id": "turn",
                    "usage": {"input_tokens": input, "output_tokens": 10, "cached_input_tokens": cached},
                },
            })
        };
        let records = vec![
            json!({"type": "session_meta", "timestamp": at(0.0), "payload": {"id": "thread", "model_provider": "openai", "cli_version": "0.167.0", "originator": "codex_cli_rs", "source": "cli"}}),
            json!({"type": "turn_context", "timestamp": at(0.0), "payload": {"model": "gpt-5.4", "turn_id": "turn", "service_tier": "standard"}}),
            json!({"type": "response_item", "timestamp": at(0.5 / 60.0), "payload": {"type": "function_call", "call_id": "call", "name": "exec", "arguments": "SYNTHETIC_PRIVATE_INPUT"}}),
            billed(1, 10_000, 0),
            json!({"type": "response_item", "timestamp": at(3.0 / 60.0), "payload": {"type": "function_call_output", "call_id": "call", "output": format!("SYNTHETIC_PRIVATE_OUTPUT{}", "x".repeat(3_976))}}),
            // One 128-token step short of the whole prompt: its rendered end.
            billed(4, 11_200, 9_856),
            // Nothing read within the lifetime.
            billed(5, 12_000, 0),
        ];
        let audit = audit_of(&w, &[(".codex/sessions/rollout.jsonl", records)]);
        let misses = &audit.cache_misses.hotspots;
        assert_eq!(misses.len(), 1, "{misses:?}");
        // Up to 11,200 less a step, rounded down to a step.
        assert_eq!(
            (misses[0].cause, misses[0].tokens),
            (Cause::PrefixBust, 11_008)
        );
        assert!(close(misses[0].cost_usd, 11_008.0 * (2.5e-6 - 0.25e-6)));
        let output = &audit.tool_outputs.hotspots[0];
        assert_eq!(
            (output.tool.as_str(), output.tokens, output.carried_requests),
            ("exec", 1_000, 2)
        );
        // Past the cached prefix in both carriers, at the ordinary input price.
        assert!(close(output.cost_usd, 2.0 * 1_000.0 * 2.5e-6), "{output:?}");
        assert_eq!(output.error, None);
        let shape = &audit.sessions.costliest[0];
        assert_eq!((shape.harness, shape.tool_errors), (Harness::Codex, None));
        assert_eq!(shape.session.as_deref(), Some("thread"));
    }

    #[test]
    fn a_codex_miss_waits_on_its_predecessors_calls_and_failed_completions_are_errors() {
        let w = Workspace::new();
        let done = |minute: f64, item: Value| json!({"type": "event_msg", "timestamp": at(minute), "payload": {"type": "item_completed", "item": item}});
        let records = vec![
            codex_meta("thread", None),
            codex_turn("gpt-5.4"),
            // The call comes before the usage of the response that made it, here in the
            // same millisecond.
            json!({"type": "response_item", "timestamp": at(1.0), "payload": {"type": "function_call", "call_id": "call", "name": "exec", "arguments": "SYNTHETIC_PRIVATE_INPUT"}}),
            codex_billed(1.0, "a", 10_000, 0),
            done(
                10.0,
                json!({"type": "CommandExecution", "id": "exec-1", "exit_code": 1}),
            ),
            json!({"type": "response_item", "timestamp": at(10.0), "payload": {"type": "function_call_output", "call_id": "call", "output": "SYNTHETIC_PRIVATE_OUTPUT"}}),
            done(
                10.1,
                json!({"type": "McpToolCall", "id": "m1", "server": "s", "tool": "t", "status": "completed", "result": {"isError": true}}),
            ),
            done(
                10.2,
                json!({"type": "FileChange", "id": "f1", "status": "completed"}),
            ),
            done(
                10.3,
                json!({"type": "FileChange", "id": "f2", "status": "failed"}),
            ),
            codex_billed(10.5, "b", 11_200, 0),
        ];
        let audit = audit_of(&w, &[(".codex/sessions/rollout.jsonl", records)]);
        let miss = &audit.cache_misses.hotspots[0];
        assert_eq!(
            (miss.cause, miss.tools.as_slice()),
            (Cause::ToolWaitTtl, ["exec".to_string()].as_slice())
        );
        let shape = &audit.sessions.costliest[0];
        assert_eq!(shape.tool_errors, Some(3));
    }

    #[test]
    fn a_codex_completion_belongs_to_the_thread_it_names() {
        let w = Workspace::new();
        let failed = json!({"type": "event_msg", "timestamp": at(2.0), "payload": {"type": "item_completed", "thread_id": "child", "item": {"type": "CommandExecution", "id": "exec-1", "exit_code": 1}}});
        // The older parent's rollout also records its child's completion.
        let parent = vec![
            codex_meta("parent", None),
            codex_turn("gpt-5.4"),
            codex_billed(1.0, "a", 10_000, 0),
            failed.clone(),
        ];
        let child = vec![
            json!({"type": "session_meta", "timestamp": at(0.5), "payload": {"id": "child", "model_provider": "openai", "cli_version": "0.167.0", "originator": "codex_cli_rs", "source": "cli"}}),
            codex_turn("gpt-5.4"),
            codex_billed(1.5, "b", 10_000, 0),
            failed,
        ];
        let audit = audit_of(
            &w,
            &[
                (".codex/sessions/a-parent.jsonl", parent),
                (".codex/sessions/b-child.jsonl", child),
            ],
        );
        let errors: Vec<_> = audit
            .sessions
            .most_tool_errors
            .iter()
            .map(|s| (s.session.as_deref(), s.tool_errors))
            .collect();
        assert_eq!(errors, [(Some("child"), Some(1))]);
    }

    #[test]
    fn a_codex_prefix_written_for_30_minutes_outlives_a_10_minute_gap() {
        let w = Workspace::new();
        let billed = |minute: f64, id: &str, cached: i64, written: i64| {
            json!({
                "type": "token_usage_record",
                "timestamp": at(minute),
                "payload": {
                    "response_id": id,
                    "turn_id": "turn",
                    "usage": {"input_tokens": 10_500, "output_tokens": 10, "cached_input_tokens": cached, "cache_write_input_tokens": written},
                },
            })
        };
        let records = vec![
            json!({"type": "session_meta", "timestamp": at(0.0), "payload": {"id": "thread", "model_provider": "openai", "cli_version": "0.167.0", "originator": "codex_cli_rs", "source": "cli"}}),
            json!({"type": "turn_context", "timestamp": at(0.0), "payload": {"model": "gpt-6.1-sol", "turn_id": "turn", "service_tier": "standard"}}),
            billed(0.0, "a", 0, 10_500),
            billed(10.0, "b", 0, 10_500),
        ];
        let audit = audit_of(&w, &[(".codex/sessions/rollout.jsonl", records)]);
        let miss = &audit.cache_misses.hotspots[0];
        assert_eq!((miss.cause, miss.ttl_ms), (Cause::PrefixBust, 30 * MIN));
    }

    #[test]
    fn a_session_shows_its_errors_longest_run_and_first_prompt() {
        let w = Workspace::new();
        let records = vec![
            assistant(0.0, "a", OPUS, 0, 30_000, &[("Bash", "b1"), ("Bash", "b2")]),
            result(0.1, "b1", 100, true),
            result(0.1, "b2", 100, false),
            assistant(0.2, "b", OPUS, 30_000, 500, &[("Bash", "b3")]),
            result(0.3, "b3", 100, false),
            assistant(0.4, "c", OPUS, 30_500, 500, &[("Read", "r")]),
            result(0.5, "r", 100, false),
            assistant(0.6, "d", OPUS, 31_000, 500, &[("Bash", "b4")]),
            result(0.7, "b4", 100, false),
            // An unreadable line may have held usage.
            json!("SYNTHETIC_PRIVATE_UNREADABLE"),
        ];
        let audit = audit_of(&w, &[(".claude/projects/project/main.jsonl", records)]);
        let s = &audit.sessions;
        let shape = &s.longest_streaks[0];
        let streak = shape.streak.as_ref().unwrap();
        assert_eq!(
            (streak.tool.as_str(), streak.calls, streak.at.line),
            ("Bash", 3, 1)
        );
        assert_eq!((shape.tool_calls, shape.tool_errors), (5, Some(1)));
        assert_eq!(shape.start_input_tokens, Some(30_000));
        assert_eq!(shape.requests, 4);
        assert_eq!((shape.complete, shape.skipped_lines), (false, 1));
        assert_eq!(audit.totals.incomplete, 1);
        assert_eq!(shape.early_lines, [1, 4, 6, 8]);
        assert_eq!(s.most_tool_errors.len(), 1);
        // A main session is no subagent.
        assert!(s.subagents.is_empty());
        assert_eq!(audit.totals.sessions, 1);
    }

    #[test]
    fn a_repository_matches_by_remote_in_any_form_or_by_its_shared_git_directory() {
        let w = Workspace::new();
        let dir = |path: &str| {
            let path = w.home.join(path);
            std::fs::create_dir_all(&path).unwrap();
            crate::discovery::realpath(&path.to_string_lossy()).unwrap()
        };
        let remote = dir("remote");
        std::fs::create_dir_all(w.home.join("remote/.git")).unwrap();
        std::fs::write(
            w.home.join("remote/.git/config"),
            "[remote \"origin\"]\n\turl = git@GitHub.com:o/r.git\n",
        )
        .unwrap();
        let sub = dir("remote/sub");
        let local = dir("local");
        std::fs::create_dir_all(w.home.join("local/.git/worktrees/wt")).unwrap();
        std::fs::write(w.home.join("local/.git/config"), "[core]\n").unwrap();
        std::fs::write(w.home.join("local/.git/worktrees/wt/commondir"), "../..\n").unwrap();
        let linked = dir("wt");
        std::fs::write(
            w.home.join("wt/.git"),
            format!(
                "gitdir: {}\n",
                w.home.join("local/.git/worktrees/wt").display()
            ),
        )
        .unwrap();
        let elsewhere = dir("elsewhere");

        // A token in the query never reaches the key that is printed and sent.
        assert_eq!(
            repository::canonical("https://github.com/o/r.git?token=SECRET#x"),
            // slash ok: a remote's key, not a file path
            "github.com/o/r"
        );
        // Only the authority's userinfo goes, never an `@` in the path.
        for (given, key) in [
            (
                "https://user:t@ken@git.example/team@dept/r.git",
                "git.example/team@dept/r",
            ),
            ("git@git.example:team@dept/r.git", "git.example/team@dept/r"),
        ] {
            assert_eq!(repository::canonical(given), key, "{given}");
        }
        let https = Filter::Remotes(vec![repository::canonical("https://github.com/o/r/")]);
        assert_eq!(Filter::of(&sub).unwrap(), Some(https.clone()));
        let mut scope = Scope {
            filter: &https,
            checkouts: HashMap::default(),
        };
        assert!(scope.covers(Some(&remote), None).unwrap());
        assert!(!scope.covers(Some(&elsewhere), None).unwrap());
        assert!(!scope.covers(None, None).unwrap());
        // Codex's recorded remote counts even where the directory is gone.
        assert!(
            scope
                .covers(Some("/gone"), Some("ssh://git@github.com:22/o/r.git"))
                .unwrap()
        );

        let common = Filter::of(&linked).unwrap().unwrap();
        assert!(matches!(common, Filter::Common(_)), "{common:?}");
        let mut scope = Scope {
            filter: &common,
            checkouts: HashMap::default(),
        };
        assert!(scope.covers(Some(&local), None).unwrap());
        assert!(!scope.covers(Some(&remote), None).unwrap());
        assert_eq!(Filter::of(&elsewhere).unwrap(), None);
    }

    fn shape(cost: f64, calls: u64) -> Shape {
        Shape {
            harness: Harness::Claude,
            session: None,
            parent: None,
            subagent: false,
            cwd: None,
            requests: 1,
            cost_usd: Usd::from_f64(cost),
            complete: true,
            skipped_lines: 0,
            input_tokens: 0,
            cache_read_tokens: Some(0),
            cache_hit_ratio: None,
            peak_input_tokens: 0,
            start_input_tokens: None,
            compactions: 0,
            tool_calls: calls,
            tool_errors: None,
            streak: None,
            at: Pointer {
                host: None,
                file: "f".into(),
                line: (cost * 10.0) as u64,
            },
            early_lines: vec![],
        }
    }

    #[test]
    fn hosts_merge_into_one_ranking_with_tools_summed_and_every_pointer_named() {
        let tool = |usd: f64| ToolTally {
            tool: "Bash".into(),
            results: 1,
            tokens: 10,
            carried_requests: 1,
            carried_tokens: 10,
            cost_usd: Usd::from_f64(usd),
            unpriced: 0,
        };
        let host = |costs: &[f64]| {
            let mut audit = Audit::default();
            audit.totals.sessions = costs.len() as u64;
            audit.sessions.costliest = costs.iter().map(|&c| shape(c, 0)).collect();
            audit.tool_outputs.by_tool = vec![tool(costs[0])];
            audit
        };
        let here = host(&[3.0, 1.0]);
        let mut there = host(&[2.0, 0.5]);
        there.on("devbox");
        assert!(there.within(2) && !there.within(1));
        let merged = Audit::merge([here, there], 3);
        let ranked: Vec<(f64, Option<&str>)> = merged
            .sessions
            .costliest
            .iter()
            .map(|s| (s.cost_usd.to_f64(), s.at.host.as_deref()))
            .collect();
        assert_eq!(ranked, [(3.0, None), (2.0, Some("devbox")), (1.0, None)]);
        assert_eq!(merged.totals.sessions, 4);
        assert_eq!(merged.tool_outputs.by_tool.len(), 1);
        assert_eq!(merged.tool_outputs.by_tool[0].cost_usd.to_f64(), 5.0);
        // Every cause is listed, found or not.
        assert_eq!(merged.cache_misses.by_cause.len(), 4);
        // A measure nobody scored on lists nobody.
        assert!(merged.sessions.most_tool_calls.is_empty());
        // Printed, a session several measures rank is listed once.
        let mut sessions = merged.sessions;
        sessions.most_input = vec![sessions.costliest[1].clone(), shape(0.1, 0)];
        let ranked = sessions.ranked();
        assert_eq!(ranked.list.len(), 4);
        assert_eq!(
            (ranked.costliest, ranked.most_input),
            (vec![0, 1, 2], vec![1, 3])
        );
    }
}
