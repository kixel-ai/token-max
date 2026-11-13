//! T3's idle-timer guards for past gaps, rebuilt from its retained event log.
//!
//! Each V2 event holds a whole snapshot of one entity, so an entity's state at a time is its
//! latest event by then. T3 prunes events older than about a day; after the boundary every
//! event survives, so a state there is known once the entity has a retained event by then,
//! or has none at all, when its projection row still holds. An entity created after the
//! boundary did not exist before its first event.

use std::sync::Arc;

use rusqlite::Transaction;
use rusqlite::types::ValueRef;

use super::{Unavailable, VALUE_BOUND, instant, integer_column, text_column};
use crate::collections::{HashMap, HashSet};
use crate::model::{Millis, T3Guard, T3Guards};

/// T3 keeps at least this much of its event log (`RETAINED_EVENT_HISTORY`).
const RETAINED: Millis = 24 * 60 * 60_000;

const PRUNED: &str = "T3 pruned the event holding a lifecycle guard's state during this gap";
const OVERSIZED: &str = "a T3 event holding a lifecycle guard exceeds the 1 MiB read bound";
const UNSUPPORTED: &str =
    "T3 recorded an event this adapter does not support before or during this gap";

/// Event types whose payload is a whole thread.
const THREAD: [&str; 23] = [
    "thread.created",
    "thread.archived",
    "thread.unarchived",
    "thread.deleted",
    "thread.settled",
    "thread.unsettled",
    "thread.snoozed",
    "thread.unsnoozed",
    "thread.pinned",
    "thread.auto-settle-set",
    "thread.unpinned",
    "thread.pin-reordered",
    "thread.active-reordered",
    "thread.tags-updated",
    "thread.visited",
    "thread.marked-unread",
    "thread.metadata-updated",
    "thread.pull-request-synced",
    "thread.runtime-mode-updated",
    "thread.interaction-mode-updated",
    "thread.model-selection-updated",
    "thread.provider-switched",
    "thread.guest-share-updated",
];
/// Event types whose payload is a whole guard entity other than the thread.
const ENTITIES: [(&str, Kind); 8] = [
    ("run.created", Kind::Run),
    ("run.updated", Kind::Run),
    ("provider-thread.updated", Kind::ProviderThread),
    ("provider-turn.updated", Kind::Turn),
    ("runtime-request.updated", Kind::Request),
    ("context-transfer.created", Kind::Transfer),
    ("context-transfer.updated", Kind::Transfer),
    ("turn-item.updated", Kind::Item),
];
/// The pinned revision's other event types, none of which changes a guard.
const IGNORED: [&str; 15] = [
    "thread.pull-request-link-updated",
    "run.background-work-cancelled",
    "run-attempt.created",
    "run-attempt.updated",
    "node.updated",
    "subagent.updated",
    "provider-session.attached",
    "provider-session.updated",
    "provider-session.detached",
    "message.updated",
    "plan.updated",
    "checkpoint-scope.created",
    "checkpoint.captured",
    "checkpoint.rollback-requested",
    "context-handoff.updated",
];
/// Turn items that hold background work.
const BACKGROUND: [&str; 3] = ["command_execution", "dynamic_tool", "subagent"];

/// The snapshot keys the guards read.
const KEYS: &str = "('id','status','type','ordinal','createdAt','requestedAt','startedAt',
 'completedAt','providerThreadId','providerInstanceId','driver','runId','targetThreadId',
 'appThreadId','deletedAt','archivedAt','settledAt','settledOverride','snoozedUntil','lineage',
 'activeProviderThreadId','tokenUsage','input','firstRunOrdinal','nativeThreadRef',
 'providerSessionId','pendingBackgroundTasks')";
/// Each key's value as an identifier, status, time, count or flag; nested values are reduced
/// in SQLite, so no text, tool input or task description leaves it.
const VALUE: &str = "CASE j.key
 WHEN 'lineage' THEN CASE WHEN j.type='object' THEN
   COALESCE(json_extract(j.value,'$.relationshipToParent'),'')!='subagent'
   AND COALESCE(json_type(j.value,'$.sideChat'),'')!='true' ELSE 0 END
 WHEN 'tokenUsage' THEN CASE WHEN j.type='object' THEN json_extract(j.value,'$.updatedAt') END
 WHEN 'input' THEN CASE WHEN j.type='object' THEN
   COALESCE(json_type(j.value,'$.persistent'),'')='true' ELSE 0 END
 WHEN 'pendingBackgroundTasks' THEN CASE WHEN j.type='array' THEN
   (SELECT count(*) FROM json_each(j.value) k
    WHERE CASE WHEN k.type='object' THEN length(json_extract(k.value,'$.taskId'))>0 END) END
 WHEN 'nativeThreadRef' THEN j.type='null'
 ELSE j.atom END";

/// One thread's retained events after the boundary, one row per snapshot key. Payloads past
/// the value bound stay unread, and only guard entities' payloads are parsed.
pub(super) fn events_query() -> String {
    let parsed = THREAD
        .iter()
        .chain(ENTITIES.iter().map(|(t, _)| t))
        .map(|t| format!("'{t}'"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "SELECT e.sequence, e.occurred_at, e.event_type,
 e.event_type IN ({parsed}) AND octet_length(e.payload_json)<=?3, j.key, {VALUE}
 FROM orchestration_events e
 LEFT JOIN json_each(CASE WHEN e.event_type IN ({parsed}) AND octet_length(e.payload_json)<=?3
   THEN e.payload_json END) j ON j.key IN {KEYS}
 WHERE e.application_event_version=2 AND e.aggregate_kind='thread' AND e.stream_id=?1
   AND e.sequence>?2
 ORDER BY e.sequence"
    )
}

/// One thread's projection rows that can stand in for an entity with no retained event:
/// the thread, its runs, provider threads and turns, and whatever is still pending.
pub(super) fn projection_query() -> String {
    let sources = [
        (
            Kind::Thread,
            "orchestration_v2_projection_threads",
            "thread_id",
            "p.thread_id=?1",
        ),
        (
            Kind::Run,
            "orchestration_v2_projection_runs",
            "run_id",
            "p.thread_id=?1",
        ),
        (
            Kind::ProviderThread,
            "orchestration_v2_projection_provider_threads",
            "provider_thread_id",
            "p.thread_id=?1",
        ),
        (
            Kind::Turn,
            "orchestration_v2_projection_provider_turns",
            "provider_turn_id",
            "p.thread_id=?1",
        ),
        (
            Kind::Request,
            "orchestration_v2_projection_runtime_requests",
            "runtime_request_id",
            "p.thread_id=?1 AND p.status='pending'",
        ),
        (
            Kind::Transfer,
            "orchestration_v2_projection_context_transfers",
            "context_transfer_id",
            "p.target_thread_id=?1 AND p.status='pending' AND p.type='merge_back'",
        ),
        (
            Kind::Item,
            "orchestration_v2_projection_turn_items",
            "turn_item_id",
            "p.thread_id=?1 AND p.type IN ('command_execution','dynamic_tool','subagent')
   AND p.status IN ('pending','running','waiting')",
        ),
    ];
    let parts: Vec<String> = sources
        .iter()
        .map(|(kind, table, id, filter)| {
            format!(
                "SELECT {kind}, p.{id}, octet_length(p.payload_json)<=?2, j.key, {VALUE}
 FROM {table} p
 LEFT JOIN json_each(CASE WHEN octet_length(p.payload_json)<=?2 THEN p.payload_json END) j
   ON j.key IN {KEYS}
 WHERE {filter}",
                kind = *kind as u8
            )
        })
        .collect();
    format!("{} ORDER BY 1, 2", parts.join("\n UNION ALL "))
}

/// Where T3's event log becomes complete.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Boundary {
    /// T3 keeps every event after this sequence.
    sequence: i64,
    /// Every event from this time on is kept; None when T3 never pruned.
    pub(super) since: Option<Millis>,
}

const BOUNDARY: &str = "SELECT last_sequence, updated_at FROM orchestration_v2_projection_metadata
 WHERE projection_name='event-log-pruned-through'";

/// T3's prune boundary. A pass prunes through the mark an earlier pass left at least
/// `RETAINED` before, and writes this row with its own time, so every pruned event occurred
/// before `updated_at - RETAINED`.
pub(super) fn boundary(tx: &Transaction<'_>) -> Result<Boundary, Unavailable> {
    let row = tx.query_row(BOUNDARY, [], |r| {
        Ok((integer_column(r, 0), instant(text_column(r, 1))))
    });
    match row {
        Ok((Some(sequence), Some(at))) => Ok(Boundary {
            sequence,
            since: Some(at - RETAINED),
        }),
        Ok(_) => Err(Unavailable::Schema(
            "malformed event-log-pruned-through metadata".into(),
        )),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Boundary::default()),
        Err(e) => Err(e.into()),
    }
}

/// A joined provider turn whose run may arm T3's idle timer.
pub(super) struct Candidate<'a> {
    pub(super) thread: &'a str,
    pub(super) run: &'a str,
    /// When T3 last recorded the turn's usage, which must stay its provider thread's latest.
    pub(super) usage_at: Option<Millis>,
    /// Whether the run's user message exists and is not `/compact`.
    pub(super) compactable: bool,
}

/// Rebuild the guards after each candidate turn's run, keyed by provider turn. Events after
/// the native snapshot `bound` are not read into any state.
pub(super) fn rebuild(
    tx: &Transaction<'_>,
    boundary: Boundary,
    candidates: &HashMap<&str, Candidate<'_>>,
    bound: Millis,
) -> Result<HashMap<String, Arc<T3Guards>>, Unavailable> {
    let mut threads: HashMap<&str, Vec<(&str, &Candidate<'_>)>> = HashMap::default();
    for (turn, c) in candidates {
        threads.entry(c.thread).or_default().push((turn, c));
    }
    let mut events = tx.prepare(&events_query())?;
    let mut projection = tx.prepare(&projection_query())?;
    let mut out = HashMap::default();
    for (thread, list) in threads {
        let mut log = Log::new(thread, boundary);
        let mut rows = events.query(rusqlite::params![thread, boundary.sequence, VALUE_BOUND])?;
        let mut read: Vec<Event> = Vec::new();
        let mut last = None;
        while let Some(row) = rows.next()? {
            let sequence: i64 = row.get(0)?;
            if last != Some(sequence) {
                last = Some(sequence);
                let event_type = text_column(row, 2).unwrap_or_default();
                read.push(Event {
                    at: instant(text_column(row, 1)),
                    kind: classify(&event_type),
                    snapshot: (integer_column(row, 3) == Some(1)).then(Snapshot::default),
                });
            }
            if let (Some(snapshot), Some(key)) = (
                read.last_mut().and_then(|e| e.snapshot.as_mut()),
                text_column(row, 4),
            ) {
                snapshot.set(&key, row.get_ref(5)?);
            }
        }
        let mut rows = projection.query(rusqlite::params![thread, VALUE_BOUND])?;
        let mut current: Vec<(Kind, String, Option<Snapshot>)> = Vec::new();
        while let Some(row) = rows.next()? {
            let (Some(kind), Some(id)) = (
                integer_column(row, 0).and_then(Kind::from_code),
                text_column(row, 1),
            ) else {
                continue;
            };
            if current
                .last()
                .is_none_or(|(k, i, _)| (*k, i.as_str()) != (kind, &id))
            {
                let readable = integer_column(row, 2) == Some(1);
                current.push((kind, id, readable.then(Snapshot::default)));
            }
            if let (Some(snapshot), Some(key)) = (
                current.last_mut().and_then(|c| c.2.as_mut()),
                text_column(row, 3),
            ) {
                snapshot.set(&key, row.get_ref(4)?);
            }
        }
        log.load(&read, current);
        out.extend(log.sweep(&read, &list, bound));
    }
    Ok(out)
}

/// The guard entities T3's idle predicate reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    /// The thread's own flags: archived, deleted, settled, snoozed and listed.
    Thread,
    Run,
    ProviderThread,
    Turn,
    Request,
    Transfer,
    Item,
    /// The thread's active provider thread, which thread and provider-thread events both set.
    Active,
}

impl Kind {
    fn from_code(code: i64) -> Option<Self> {
        [
            Self::Thread,
            Self::Run,
            Self::ProviderThread,
            Self::Turn,
            Self::Request,
            Self::Transfer,
            Self::Item,
        ]
        .into_iter()
        .find(|k| *k as i64 == code)
    }
}

/// An event's entity kind: Some(None) for a type no guard reads, None for an unsupported one.
fn classify(event_type: &str) -> Option<Option<Kind>> {
    if THREAD.contains(&event_type) {
        Some(Some(Kind::Thread))
    } else if let Some((_, kind)) = ENTITIES.iter().find(|(t, _)| *t == event_type) {
        Some(Some(*kind))
    } else {
        IGNORED.contains(&event_type).then_some(None)
    }
}

/// One retained event.
struct Event {
    at: Option<Millis>,
    kind: Option<Option<Kind>>,
    /// Its fields, or None when the payload exceeds the value bound or no guard reads it.
    snapshot: Option<Snapshot>,
}

/// The allowlisted fields of one entity snapshot.
#[derive(Clone, Debug, Default)]
struct Snapshot {
    id: Option<String>,
    status: Option<String>,
    kind: Option<String>,
    ordinal: Option<i64>,
    created: Option<Millis>,
    requested: Option<Millis>,
    started: bool,
    started_at: Option<Millis>,
    completed: bool,
    provider_thread: Option<String>,
    instance: Option<String>,
    driver: Option<String>,
    run: Option<String>,
    target: Option<String>,
    app_thread: Option<String>,
    /// Archived, deleted or settled.
    closed: bool,
    snoozed_until: Option<Millis>,
    /// Neither a subagent nor a side chat.
    listed: bool,
    active_provider_thread: Option<String>,
    usage_at: Option<Millis>,
    persistent: bool,
    tasks: i64,
    first_run_null: bool,
    native_null: bool,
    session_null: bool,
}

impl Snapshot {
    fn set(&mut self, key: &str, value: ValueRef<'_>) {
        let text = || match value {
            ValueRef::Text(t) => std::str::from_utf8(t).ok().map(str::to_owned),
            _ => None,
        };
        let time = || instant(text());
        let null = matches!(value, ValueRef::Null);
        let flag = matches!(value, ValueRef::Integer(1));
        let integer = match value {
            ValueRef::Integer(n) => Some(n),
            _ => None,
        };
        match key {
            "id" => self.id = text(),
            "status" => self.status = text(),
            "type" => self.kind = text(),
            "ordinal" => self.ordinal = integer,
            "createdAt" => self.created = time(),
            "requestedAt" => self.requested = time(),
            "startedAt" => (self.started, self.started_at) = (!null, time()),
            "completedAt" => self.completed = !null,
            "providerThreadId" => self.provider_thread = text(),
            "providerInstanceId" => self.instance = text(),
            "driver" => self.driver = text(),
            "runId" => self.run = text(),
            "targetThreadId" => self.target = text(),
            "appThreadId" => self.app_thread = text(),
            "deletedAt" | "archivedAt" | "settledAt" => self.closed |= !null,
            "settledOverride" => self.closed |= text().as_deref() == Some("settled"),
            "snoozedUntil" => self.snoozed_until = time(),
            "lineage" => self.listed = flag,
            "activeProviderThreadId" => self.active_provider_thread = text(),
            "tokenUsage" => self.usage_at = time(),
            "input" => self.persistent = flag,
            "firstRunOrdinal" => self.first_run_null = null,
            "nativeThreadRef" => self.native_null = flag,
            "providerSessionId" => self.session_null = null,
            "pendingBackgroundTasks" => self.tasks = integer.unwrap_or(0),
            _ => {}
        }
    }

    /// When the entity came to exist, as far as its snapshot says.
    fn created(&self, kind: Kind) -> Option<Millis> {
        match kind {
            Kind::Run => self.requested,
            Kind::Turn | Kind::Item => self.started_at,
            _ => self.created,
        }
    }

    /// A provider thread queued for a run that has not started, which leaves the thread's
    /// active provider thread alone.
    fn placeholder(&self) -> bool {
        self.status.as_deref() == Some("not_loaded")
            && self.first_run_null
            && self.native_null
            && self.session_null
    }

    /// A run that is preparing, starting or running.
    fn running(&self) -> bool {
        matches!(
            self.status.as_deref(),
            Some("preparing" | "starting" | "running")
        )
    }

    /// Background work still pending: a non-persistent background item that is active.
    fn working(&self) -> bool {
        matches!(
            self.status.as_deref(),
            Some("pending" | "running" | "waiting")
        ) && !(self.kind.as_deref() == Some("dynamic_tool") && self.persistent)
    }
}

/// An entity's state at the sweep's time.
#[derive(Clone, Debug)]
enum State {
    Known(Snapshot),
    Absent,
    Unknown(&'static str),
}

struct Entity {
    /// Its first known snapshot, for fields that never change, such as a run's ordinal.
    first: Snapshot,
    state: State,
}

/// Whether something blocks the timer, or why that is unknown.
type Tri = Result<bool, &'static str>;

fn or(a: Tri, b: Tri) -> Tri {
    match (a, b) {
        (Ok(true), _) | (_, Ok(true)) => Ok(true),
        (Err(e), _) | (_, Err(e)) => Err(e),
        _ => Ok(false),
    }
}

fn and(a: Tri, b: Tri) -> Tri {
    match (a, b) {
        (Ok(false), _) | (_, Ok(false)) => Ok(false),
        (Err(e), _) | (_, Err(e)) => Err(e),
        _ => Ok(true),
    }
}

static ABSENT: State = State::Absent;

/// One thread's guard entities as a sweep over its events reaches each time.
struct Log<'a> {
    thread: &'a str,
    boundary: Boundary,
    /// Each kind's entities by id.
    entities: HashMap<Kind, HashMap<String, Entity>>,
    /// Background items that are pending or unknown, the only ones that can block.
    live: HashSet<String>,
    /// Kinds an unreadable event may have changed, so any of their entities may be wrong.
    poisoned: HashMap<Kind, &'static str>,
    /// An event this adapter cannot apply, which may have changed anything after it.
    unsupported: bool,
}

impl<'a> Log<'a> {
    fn new(thread: &'a str, boundary: Boundary) -> Self {
        Self {
            thread,
            boundary,
            entities: HashMap::default(),
            live: HashSet::default(),
            poisoned: HashMap::default(),
            unsupported: false,
        }
    }

    /// The entities a readable event of `kind` sets.
    fn targets(&self, kind: Kind, s: &Snapshot) -> Vec<(Kind, String)> {
        let thread = || self.thread.to_string();
        match kind {
            Kind::Thread => vec![(Kind::Thread, thread()), (Kind::Active, thread())],
            Kind::Item if !BACKGROUND.contains(&s.kind.as_deref().unwrap_or_default()) => vec![],
            Kind::Transfer
                if s.kind.as_deref() != Some("merge_back")
                    || s.target.as_deref() != Some(self.thread) =>
            {
                vec![]
            }
            _ => {
                let mut out: Vec<_> = s.id.iter().map(|id| (kind, id.clone())).collect();
                if kind == Kind::ProviderThread
                    && s.app_thread.as_deref() == Some(self.thread)
                    && !s.placeholder()
                {
                    out.push((Kind::Active, thread()));
                }
                out
            }
        }
    }

    fn of(&self, kind: Kind) -> impl Iterator<Item = (&String, &Entity)> {
        self.entities.get(&kind).into_iter().flatten()
    }

    fn get(&self, kind: Kind, id: &str) -> Option<&Entity> {
        self.entities.get(&kind)?.get(id)
    }

    fn state(&self, kind: Kind, id: &str) -> &State {
        self.get(kind, id).map_or(&ABSENT, |e| &e.state)
    }

    /// Each entity's state before the first event: absent when created after the boundary,
    /// unknown when its retained events start later, else its projection row.
    fn load(&mut self, events: &[Event], current: Vec<(Kind, String, Option<Snapshot>)>) {
        let mut thread_created = None;
        let mut rows: Vec<((Kind, String), Option<Snapshot>)> = Vec::new();
        for (kind, id, snapshot) in current {
            if kind == Kind::Thread {
                thread_created = snapshot.as_ref().and_then(|s| s.created);
                rows.push(((Kind::Active, id.clone()), snapshot.clone()));
            }
            rows.push(((kind, id), snapshot));
        }
        for e in events {
            let (Some(Some(kind)), Some(s)) = (e.kind, &e.snapshot) else {
                continue;
            };
            if kind == Kind::Thread && thread_created.is_none() {
                thread_created = s.created;
            }
            for (kind, id) in self.targets(kind, s) {
                let created = match kind {
                    Kind::Thread | Kind::Active => thread_created,
                    _ => s.created(kind),
                };
                // Without pruning, every event is retained.
                let absent = self
                    .boundary
                    .since
                    .is_none_or(|b| created.is_some_and(|c| c >= b));
                self.entities
                    .entry(kind)
                    .or_default()
                    .entry(id)
                    .or_insert_with(|| Entity {
                        first: s.clone(),
                        state: if absent {
                            State::Absent
                        } else {
                            State::Unknown(PRUNED)
                        },
                    });
            }
        }
        for ((kind, id), snapshot) in rows {
            self.entities
                .entry(kind)
                .or_default()
                .entry(id)
                .or_insert_with(|| match snapshot {
                    Some(s) => Entity {
                        first: s.clone(),
                        state: State::Known(s),
                    },
                    None => Entity {
                        first: Snapshot::default(),
                        state: State::Unknown(OVERSIZED),
                    },
                });
        }
        let items: Vec<String> = self.of(Kind::Item).map(|(id, _)| id.clone()).collect();
        for id in items {
            self.relive(&id);
        }
    }

    /// Track whether a background item can block.
    fn relive(&mut self, id: &str) {
        let live = match self.state(Kind::Item, id) {
            State::Known(s) => s.working(),
            State::Unknown(_) => true,
            State::Absent => false,
        };
        if live {
            self.live.insert(id.to_string());
        } else {
            self.live.remove(id);
        }
    }

    fn apply(&mut self, e: &Event) {
        let Some(kind) = e.kind else {
            self.unsupported = true;
            return;
        };
        let Some(kind) = kind else {
            return;
        };
        let Some(s) = &e.snapshot else {
            // An unreadable payload names no entity, unless it is this thread's.
            let thread = self.thread.to_string();
            let mut unknown = |kind: Kind| {
                if let Some(e) = self
                    .entities
                    .get_mut(&kind)
                    .and_then(|m| m.get_mut(&thread))
                {
                    e.state = State::Unknown(OVERSIZED);
                }
            };
            match kind {
                Kind::Thread => {
                    unknown(Kind::Thread);
                    unknown(Kind::Active);
                }
                Kind::ProviderThread => {
                    unknown(Kind::Active);
                    self.poisoned.insert(kind, OVERSIZED);
                }
                _ => {
                    self.poisoned.insert(kind, OVERSIZED);
                }
            }
            return;
        };
        for (target, id) in self.targets(kind, s) {
            let Some(entity) = self.entities.get_mut(&target).and_then(|m| m.get_mut(&id)) else {
                continue;
            };
            let next = match (target, &entity.state) {
                // A provider thread re-points the active one and leaves the thread's flags.
                (Kind::Active, State::Known(prior)) if kind == Kind::ProviderThread => Snapshot {
                    active_provider_thread: s.id.clone(),
                    ..prior.clone()
                },
                (Kind::Active, _) if kind == Kind::ProviderThread => Snapshot {
                    active_provider_thread: s.id.clone(),
                    ..Snapshot::default()
                },
                // An update without usage keeps the turn's last usage.
                (Kind::Turn, State::Known(prior)) if s.usage_at.is_none() => Snapshot {
                    usage_at: prior.usage_at,
                    ..s.clone()
                },
                (Kind::Turn, State::Unknown(_)) if s.usage_at.is_none() => continue,
                _ => s.clone(),
            };
            entity.state = State::Known(next);
            if target == Kind::Item {
                self.relive(&id);
            }
        }
    }

    fn poison(&self, kind: Kind) -> Tri {
        if self.unsupported {
            return Err(UNSUPPORTED);
        }
        self.poisoned.get(&kind).map_or(Ok(false), |r| Err(*r))
    }

    /// Whether the run is completed now.
    fn completed(&self, run: &str) -> bool {
        matches!(self.state(Kind::Run, run),
            State::Known(s) if s.status.as_deref() == Some("completed"))
    }

    /// Whether a run after `run` has started, which ends its gap for good.
    fn superseded(&self, run: &str) -> bool {
        let Some(ordinal) = self.get(Kind::Run, run).and_then(|e| e.first.ordinal) else {
            return false;
        };
        self.of(Kind::Run).any(|(_, e)| {
            e.first.ordinal.is_some_and(|o| o > ordinal)
                && matches!(&e.state, State::Known(s) if s.started)
        })
    }

    /// When a snooze in effect at `at` ends, which changes the guards without an event.
    fn wake(&self, at: Millis) -> Option<Millis> {
        match self.state(Kind::Thread, self.thread) {
            State::Known(s) => s.snoozed_until.filter(|u| *u > at),
            _ => None,
        }
    }

    /// Mirror T3's `idleCompactionWindow` at `at` for the gap after `c`'s run, short of the
    /// cache expiry and context size, which the idle check takes from the native requests.
    fn guard(&self, c: &Candidate<'_>, at: Millis) -> T3Guard {
        self.blocked(c, at).map(|blocked| !blocked)
    }

    fn blocked(&self, c: &Candidate<'_>, at: Millis) -> Tri {
        let mut out = or(self.poison(Kind::Thread), Ok(!c.compactable));
        let thread = match self.state(Kind::Thread, self.thread) {
            State::Known(s) => {
                let snoozed = s.snoozed_until.is_some_and(|u| u > at);
                out = or(out, Ok(s.closed || !s.listed || snoozed));
                Some(s)
            }
            State::Absent => return Ok(true),
            State::Unknown(r) => {
                out = or(out, Err(r));
                None
            }
        };
        // The latest run that is not a never-started cancellation must be this completed one.
        let Some(entry) = self.get(Kind::Run, c.run) else {
            return Ok(true);
        };
        let run = match &entry.state {
            State::Known(s) if s.status.as_deref() == Some("completed") && s.completed => s,
            State::Known(_) | State::Absent => return Ok(true),
            State::Unknown(r) => return or(out, Err(r)),
        };
        out = or(out, self.poison(Kind::Run));
        let mut running = self.poison(Kind::Run);
        for (id, e) in self.of(Kind::Run) {
            if id == c.run {
                continue;
            }
            let later = e
                .first
                .ordinal
                .zip(entry.first.ordinal)
                .is_none_or(|(o, r)| o > r);
            match &e.state {
                State::Known(s) => {
                    if later && (s.status.as_deref() != Some("cancelled") || s.started) {
                        return Ok(true);
                    }
                    running = or(running, Ok(s.running()));
                }
                State::Absent => {}
                State::Unknown(r) => {
                    if later {
                        out = or(out, Err(r));
                    }
                    running = or(running, Err(r));
                }
            }
        }
        out = or(
            out,
            or(self.poison(Kind::Request), self.poison(Kind::Transfer)),
        );
        for (kind, e) in self
            .of(Kind::Request)
            .map(|(_, e)| (Kind::Request, e))
            .chain(self.of(Kind::Transfer).map(|(_, e)| (Kind::Transfer, e)))
        {
            out = or(
                out,
                match &e.state {
                    State::Known(s) => Ok(s.status.as_deref() == Some("pending")
                        && (kind == Kind::Request
                            || s.kind.as_deref() == Some("merge_back")
                                && s.target.as_deref() == Some(self.thread))),
                    State::Absent => Ok(false),
                    State::Unknown(r) => Err(r),
                },
            );
            if out == Ok(true) {
                return out;
            }
        }
        // The Claude session T3 would compact must still be this run's.
        let active = match self.state(Kind::Active, self.thread) {
            State::Known(s) => s.active_provider_thread.as_deref(),
            State::Absent => return Ok(true),
            State::Unknown(r) => return or(out, Err(r)),
        };
        let Some(active) = active.filter(|a| Some(*a) == run.provider_thread.as_deref()) else {
            return Ok(true);
        };
        let tasks = match self.state(Kind::ProviderThread, active) {
            State::Known(p) => {
                if let Some(thread) = thread {
                    let claude = p.driver.as_deref() == Some("claudeAgent");
                    let same = thread.instance.is_some()
                        && p.instance == thread.instance
                        && run.instance == thread.instance;
                    if !claude || !same {
                        return Ok(true);
                    }
                }
                Ok(p.tasks > 0)
            }
            State::Absent => return Ok(true),
            State::Unknown(r) => Err(*r),
        };
        let tasks = or(tasks, self.poison(Kind::ProviderThread));
        // Background work reports into the next turn, unless another run is active.
        let mut items = self.poison(Kind::Item);
        for id in &self.live {
            items = or(
                items,
                match self.state(Kind::Item, id) {
                    State::Known(s) => match s.run.as_deref().map(|r| self.state(Kind::Run, r)) {
                        Some(State::Known(r)) => Ok(r.status.as_deref() != Some("rolled_back")),
                        Some(State::Unknown(r)) => Err(*r),
                        Some(State::Absent) | None => Ok(true),
                    },
                    State::Unknown(r) => Err(*r),
                    State::Absent => Ok(false),
                },
            );
        }
        out = or(out, and(running.map(|r| !r), or(tasks, items)));
        // This run's last usage must be its provider thread's latest.
        if let Some(usage) = c.usage_at {
            out = or(out, self.poison(Kind::Turn));
            for (_, e) in self.of(Kind::Turn) {
                if e.first.provider_thread.as_deref() != Some(active) {
                    continue;
                }
                out = or(
                    out,
                    match &e.state {
                        State::Known(s) => Ok(s.usage_at.is_some_and(|u| u > usage)),
                        State::Absent => Ok(false),
                        State::Unknown(r) => Err(r),
                    },
                );
            }
        }
        out
    }

    /// Sweep the events in order, recording each candidate's guards from its run's
    /// completion until a later run starts.
    fn sweep(
        &mut self,
        events: &[Event],
        candidates: &[(&str, &Candidate<'_>)],
        bound: Millis,
    ) -> Vec<(String, Arc<T3Guards>)> {
        let mut trackers: Vec<Tracker> = candidates.iter().map(|_| Tracker::default()).collect();
        let mut by_run: HashMap<&str, Vec<usize>> = HashMap::default();
        for (i, (_, c)) in candidates.iter().enumerate() {
            by_run.entry(c.run).or_default().push(i);
        }
        let mut live: Vec<usize> = Vec::new();
        let mut i = 0;
        while i < events.len() {
            let Some(at) = events[i].at else {
                // An event with no time cannot be placed, so nothing after it is known.
                self.unsupported = true;
                i += 1;
                continue;
            };
            if at > bound {
                break;
            }
            for &t in &live {
                if let Some(wake) = trackers[t].wake.filter(|w| *w < at) {
                    trackers[t].record(self, candidates[t].1, wake);
                }
            }
            // Events committed together are read together.
            let mut armed = false;
            let mut completed = Vec::new();
            while let Some(e) = events.get(i).filter(|e| e.at == Some(at)) {
                let run = e
                    .snapshot
                    .as_ref()
                    .filter(|_| e.kind == Some(Some(Kind::Run)))
                    .filter(|s| s.status.as_deref() == Some("completed"))
                    .and_then(|s| s.id.as_deref());
                if let Some(run) = run {
                    armed = true;
                    if !self.completed(run) {
                        completed.push(run);
                    }
                }
                self.apply(e);
                i += 1;
            }
            for run in completed {
                for &t in by_run.get(run).into_iter().flatten() {
                    trackers[t] = Tracker {
                        started: true,
                        ..Tracker::default()
                    };
                    if !live.contains(&t) {
                        live.push(t);
                    }
                }
            }
            live.retain(|&t| {
                let (_, c) = candidates[t];
                trackers[t].record(self, c, at);
                if armed {
                    let guard = self.guard(c, at);
                    trackers[t].guards.arms.push((at, guard));
                }
                if self.superseded(c.run) {
                    trackers[t].span(at, Ok(false));
                    return false;
                }
                true
            });
        }
        for &t in &live {
            if let Some(wake) = trackers[t].wake {
                trackers[t].record(self, candidates[t].1, wake);
            }
        }
        trackers
            .into_iter()
            .zip(candidates)
            .filter_map(|(t, (turn, c))| {
                // A run that never completed in the retained log armed no timer, while one
                // whose completion T3 pruned stays unknown.
                let never = !t.started
                    && matches!(self.state(Kind::Run, c.run),
                        State::Known(s) if s.status.as_deref() != Some("completed"));
                (t.started || never).then(|| (turn.to_string(), Arc::new(t.guards)))
            })
            .collect()
    }
}

/// One candidate's guards as the sweep records them.
struct Tracker {
    started: bool,
    guards: T3Guards,
    /// When a snooze ends before the next event.
    wake: Option<Millis>,
}

impl Default for Tracker {
    fn default() -> Self {
        Self {
            started: false,
            guards: T3Guards {
                arms: vec![],
                spans: vec![],
            },
            wake: None,
        }
    }
}

impl Tracker {
    /// Evaluate the guards at `at`, and note when a snooze in effect then ends.
    fn record(&mut self, log: &Log<'_>, c: &Candidate<'_>, at: Millis) {
        self.span(at, log.guard(c, at));
        self.wake = log.wake(at);
    }

    fn span(&mut self, at: Millis, guard: T3Guard) {
        let spans = &mut self.guards.spans;
        if spans.last().is_some_and(|(t, _)| *t == at) {
            spans.pop();
        }
        if spans.last().is_none_or(|(_, g)| *g != guard) {
            spans.push((at, guard));
        }
    }
}
