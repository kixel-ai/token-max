# [S] Read Claude records whose usage is zeroed but counts thinking

## Goal

`token-max doctor` and `token-max sessions` read Claude Code 2.1.322
transcripts that hold an assistant record whose `usage` reports zero input,
cache and output tokens but a nonzero `output_tokens_details.thinking_tokens`
(seen in a subagent transcript, with a nonzero
`cache_creation.ephemeral_1h_input_tokens` beside a zero
`cache_creation_input_tokens`). Today the reader fails the whole scan with
"reasoning subset exceeds output", so on a host with such a record neither
command prints anything.

## Plan

Found while validating `token-max sessions` on a real host for
`quest/m0/sessions-ssh`. Reproduce with a synthetic record of that shape
first, then decide from more examples what the record is (a placeholder
written before the final usage, or a usage Claude Code now reports
differently) and read it as such: skip a placeholder that a later record for
the same message supersedes, or price it as recorded. Keep failing loud on a
record that is malformed in a way no Claude Code version writes.
