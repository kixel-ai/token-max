# [L] One report across SSH hosts

## Goal

With `--ssh`, `doctor` prints one report, not one per host: its sections
span hosts, each summing every host's members, and the walk asks a section
once. Splitting a section, or `--details`, shows each host's members, and
edits name each file as `host:path`.

## Plan

Settled in the [epic](/quest/m1/short/README.md): one merged report
(rejected: a shorter report per host).

A destination sends its report as preformatted text (`Outcome::Report` in
`src/wire.rs`), so merging needs the lines as structured data on the wire.
A destination runs the same token-max version, so the wire change needs no
compatibility path. Each destination still reads, checks and writes its own
files; this host only sends back which members were accepted, as it does
now. That replaces "a group only gathers one host's changes" in
`docs/reference.md`.

A split section's answers go back to each host member by member, so a
host writes exactly the members accepted for it. `vibes`' skipped-change
cards apply the section's total across hosts, as within one host.

The report after the walk (`Outcome::Reviewed`'s text, printed before the
confirm) merges the same way, keeping each section's accepted and declined members
apart, as the post-walk report already lists them, so its accepted total
counts only what was accepted; validation and writing stay on each host.

Percents are of every host's spend together, as the total line already
computes (`Totals::monthly`). The report's left-out line
(`left_out` in `src/report.rs`) sums its counts across hosts.

Rerun `assets/demo/walk.tape`.

## Required

- [Sections](/quest/m1/short/sections.md) - the per-host sections this merges
