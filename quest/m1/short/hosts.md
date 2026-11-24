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

Percents are of every host's spend together, as the total line already
computes (`Totals::monthly`). The left-out line from the
[preamble quest](/quest/m1/short/preamble.md) sums across hosts.

Rerun `assets/demo/walk.tape`.

## Required

- [Sections](/quest/m1/short/sections.md) - the per-host sections this merges
