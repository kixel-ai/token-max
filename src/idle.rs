//! Idle compaction and keepalive checks. Placeholder until the port of src/idle.ts lands.

use indexmap::IndexMap;

use crate::model::{
    CompactionCharge, CompactionCheck, IdleCheck, Millis, Scan, TtlCheck, UsageEstimate,
};
use crate::prices::Catalog;

/// Inputs to the idle checks, which run after the TTL and window decisions.
pub struct IdleOptions<'a> {
    pub scan: &'a Scan,
    pub catalog: &'a Catalog,
    pub summaries: &'a [CompactionCharge],
    pub estimates: &'a IndexMap<String, UsageEstimate>,
    pub ttls: &'a [&'a TtlCheck],
    pub windows: &'a [&'a CompactionCheck],
    pub since: Option<Millis>,
    pub now: Millis,
}

/// Compare idle compaction on and off for each Claude profile.
pub fn idle_checks(_options: &IdleOptions) -> Vec<IdleCheck> {
    Vec::new()
}
