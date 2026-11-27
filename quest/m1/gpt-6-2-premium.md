# [S] Price GPT-6.2 fast and ultrafast requests

## Goal

A Codex request on `gpt-6.2-sol` or `gpt-6.2-astra` under the fast or
ultrafast service tier is priced at the OpenAI API's premium rates instead of
left unpriced, so fast-mode lines on Codex's own effort pick (`gpt-6.2-sol`,
`src/effort.rs`) report a cost.

## Plan

- ✅ Kept by the maintainer at [kixel-ai/token-max#286](https://github.com/kixel-ai/token-max/pull/286#issuecomment-7216566816)'s pick, 2026-11-27 (rejected: drop it).
- `premium_multiplier` in `src/prices.rs` lists no `gpt-6.2-*` model, so such
  a request prices as unknown today.
- The [OpenAI price table](https://developers.openai.com/api/docs/pricing),
  read 2026-11-27, prices both models' Fast at 2x Standard. Ultrafast is 3x
  through 2027-02-28 and 6x from 2027-03-01 (Pacific time), so the ultrafast
  multiplier depends on the request's time, which `premium_multiplier` does
  not take today.
- Recheck the table when starting, since it changes, and whether the 2x holds
  above 272K input as it does for GPT-6.
- Update the fast-mode paragraph in `docs/semantics.md` with it.
