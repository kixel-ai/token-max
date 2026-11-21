# [S] Gate missing docs on the library's exports

## Goal

Every item `src/lib.rs` exports has a one-line doc comment, and `#![warn(missing_docs)]` in `src/lib.rs` keeps new ones from landing, since CI's clippy runs with `-D warnings`.

## Plan

[kixel-ai/token-max#174](https://github.com/kixel-ai/token-max/pull/174) made the library's modules private and re-exports only what `src/main.rs` and `tests/` use, so the lint no longer reaches every field in `src/model.rs`.
Adding the lint to the merged code reports 48 items: 24 enum variants and 9 fields in `src/model.rs`, 7 fields in `src/cli.rs`, 4 in `src/discovery.rs`, 3 in `src/prices.rs` and 1 in `src/reader.rs`.
Document each in one plain line, then add the lint.
