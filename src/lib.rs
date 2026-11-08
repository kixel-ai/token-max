//! token-max replays local Claude Code and Codex transcripts against public API prices.

pub mod cache;
pub mod compaction;
pub mod context;
pub mod controls;
pub mod discovery;
pub mod estimates;
pub mod exposure;
pub mod fast;
pub mod model;
pub mod prices;
pub mod reader;
pub mod report;
pub mod safe;
pub mod scenarios;
pub mod terminal;

#[cfg(test)]
mod test_support;
