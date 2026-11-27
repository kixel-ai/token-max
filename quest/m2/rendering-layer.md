# [M] One spend type and one styling layer for the report and the TUI

## Goal

The report, the walk and the TUI hold a host's spend in one type and style
text through one layer, with no copied mechanism. Output changes only where
the Plan says.

## Plan

The m2 code audit found these copies:

- Spend is held three ways. `totals::Spent`, `report::Spend` and
  `totals::Totals` each carry usd, margin, floor and partial with their own
  merge. `Listing::join` re-sums spend and savings through `Spend::merge` and
  `Head::merge`, though the `totals` it is passed already holds both. Embed a
  `Spent` in the other two and build `join`'s figures from `totals`. The
  wire format changes, which `wire::IDENTITY` already guards.
- `report::window_start` is restated in `details`, and
  `Out` is built three times (`Out::new`, `Listing::with_list`, `closing`).
- `report::Out::spans` and `doctor::Style::spans` are the same
  escape-then-wrap. Six call sites convert `(String, &str)` spans to
  `(&str, &str)` first. Write one `terminal` helper that takes
  `&[(impl AsRef<str>, &str)]`.
- `tui.rs` parses SGR sequences in three places (`ansi_cells`, `wrap_ansi`,
  `trim_ansi`), and its `RESET`, `BOLD` and color constants copy
  `terminal.rs`'s. Text is wrapped in SGR codes four ways: `App::style`,
  `cli::piggy_progress`'s `s`, `doctor::Style::raw` and
  `terminal::wrap_spans`. Write one SGR-token iterator on `terminal`'s
  constants. The TUI styles text before it measures it, so `wrap_spans`
  stays a separate caller.
- Progress bars are drawn six times in `tui.rs`, and twice in
  `cli::piggy_progress`. Use one `bar(ratio, width, color)`.
- `cli::piggy_progress` writes its own counts and savings text, instead of
  the plain progress line's `figures`/`tallies` and `App::savings_text`. It
  says "N reqs" where the plain line says "N requests". Recommended: reuse
  the plain line's text, which changes the panel's wording.
- `tui::write_by_hand` reimplements `report::closing`'s STEPS BY HAND in a
  narrower layout. Keep it if the TUI's layout is deliberate.
