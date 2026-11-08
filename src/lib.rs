//! token-max replays local Claude Code and Codex transcripts against public API prices.

pub mod controls;
pub mod discovery;
pub mod model;
pub mod safe;
pub mod terminal;

#[cfg(test)]
mod test_support;
