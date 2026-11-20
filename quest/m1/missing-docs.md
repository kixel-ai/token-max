# [M] Gate missing docs on the library's exports

## Goal

Every item `src/lib.rs` exports has a one-line doc comment, and `#![warn(missing_docs)]` in `src/lib.rs` keeps new ones from landing, since CI's clippy runs with `-D warnings`.

## Plan

The m1 docs audit documented every exported item, but not the 673 struct fields and 165 enum variants that `pub mod` exports. 716 of them are in `src/model.rs`.
Only `src/main.rs` and `tests/` use the library, through a few paths. The lint can't skip fields and variants.

Recommended: make the modules `pub(crate)` and export only what `main.rs` and the tests use. Then document what is still exported, and add the lint.
The alternative is documenting every field and variant. That costs about 840 lines of comments that mostly restate the field's name.
