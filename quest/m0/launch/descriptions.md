# [M] Say what each setting does

## Goal

Each setting and feature `token-max doctor` reports comes with one plain
sentence saying what it does, such as what the Artifact tool is and what
`enableArtifact` turns off. The walk opens each change with it, and
`--details` shows it under every row; the one-line report stays one line.

## Plan

Decided 2026-11-15 (maintainer, /quest-plan, who did not know what
"artifacts" was): ✅ the walk and `--details` (rejected: a description on
every report row, which doubles the report; the walk only).

Recommended: a skill or MCP tool reuses the description it already ships,
such as SKILL.md's `description` or the tool's own description, shortened
to one sentence; built-in tools and settings get a curated sentence checked
against the provider's documentation. A feature with neither says so rather
than guessing. These are configuration, not prompt or reply text, so
printing them is allowed.
