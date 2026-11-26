//! token-max replays local Claude Code and Codex transcripts against public API prices.
#![warn(missing_docs)]

mod advisor;
mod allowlist;
mod audit;
mod background;
mod bash;
mod cache;
pub mod cli;
mod collections;
mod compaction;
mod context;
mod controls;
mod defaults;
mod descriptions;
mod discovery;
mod doctor;
mod effort;
mod estimates;
mod exposure;
mod fable;
mod fast;
mod idle;
mod inbound;
mod keepalive;
mod lsp;
mod mcp;
mod memory;
mod model;
mod prices;
mod reader;
mod replay;
mod report;
mod repository;
mod rows;
mod safe;
mod scenarios;
mod sessions;
mod skills;
mod ssh;
mod subagents;
mod t3;
mod tally;
mod terminal;
mod totals;
mod tui;
mod turn_end;
mod unreadable;
mod upgrade;
mod version;
mod vibes;
mod window;
mod wire;

#[cfg(test)]
mod test_support;

// Modules stay private, so the compiler reports code nothing uses. The integration tests reach
// the pipeline through these.
pub use controls::Env;
pub use discovery::DiscoveryOptions;
pub use doctor::fixes;
pub use model::{
    Action, BackgroundCheck, BackgroundFeature, CheckResult, ContextKind, Harness, Report, Usd,
    Verdict,
};
pub use prices::{PriceOptions, load_prices};
pub use reader::{Reach, ScanOptions, scan};
pub use replay::{AnalyzeOptions, analyze};
pub use report::{ReportOptions, format_report};
