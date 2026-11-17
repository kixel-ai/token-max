# [M] Run on an SSH host without installing token-max there

## Goal

`doctor --ssh HOST` and `sessions --ssh HOST` work on a host that has no
token-max, or another version: token-max puts its own version there itself
and runs it, and the user installs nothing by hand.

## Plan

Decided 2026-11-17 (maintainer, /quest-plan): ✅ ask the host its OS and
architecture first; when they match this binary's, copy it to a versioned
cache directory there, and otherwise download the same version's release
asset there and check its published checksum (rejected: `nix run` of this
commit on the host, which needs nix there; copying same-platform hosts only,
which leaves out kixtop, macOS on arm, from Linux hosts). This reverses m0's
2026-11-08 pick of a `cargo install` on each host and its rejection of
copying the binary over SSH.

A development build with no matching release fails, for a host on another
platform, with the command to run, rather than trying another way.

Recommended:

- Keep the cached binary under the host's cache directory, keyed by version
  and target, so the next run reuses it and an old version never runs.
- Keep using a token-max already installed on the host when its version
  matches, which needs no copy.
- Probe the platform once over the same SSH connection, and decide the path
  from it before copying or downloading anything.
