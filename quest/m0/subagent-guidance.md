# [S] Cite Anthropic's subagent model guidance

## Goal

Each Claude subagent type's line in `token-max doctor` also cites, under
Official, Anthropic's
[model guide](https://claude.dev/blog/which-claude-model-should-you-build-with/):
subagents that search, read logs, or summarize can run on Haiku or Sonnet
at the model's default effort, code edits and judgment calls stay on the
main model, and a user agent named `Explore` with `model: haiku` keeps
exploration cheap. The lines stay evidence: no verdict and no exit code.

## Plan

Decided 2026-11-13 (maintainer, /quest-plan after reading the guide and
Theo Browne's
[Opus against Fable video](https://www.youtube.com/watch?v=XtykeVoSmP8)):
✅ cite the guide beside each subagent line (rejected: no change), in m0
(rejected: m1). The lines stay verdict-free, as decided on 2026-11-11 in
the finished subagent models quest (`quest/m0/subagent-models.md` in git
history): the parent's extra work after a weaker search can't be
attributed, and Theo argues against Haiku for any side task.

The guide's numbers worth quoting: an Opus 5.5 lead over Sonnet 5.5
workers cost about a third less than Opus alone at its default effort and
scored about the same.
