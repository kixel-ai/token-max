# [M] Ask how each SSH destination's Claude profiles pay

## Goal

`token-max doctor --ssh` asks how each Claude profile on every selected host
pays, not only this host's, so a destination's main-agent prompt cache TTL is
priced against the one default that applies instead of "saves up to".

## Plan

Found 2026-11-22 while measuring max.kixel.me's results: the page's API-rate
run could answer only nixos's profiles; desktop's and fedora's main-agent TTL
changes stayed "up to". Billing picks only that change's default, so "up to"
equals the API answer and the page is right, but a person running doctor
across hosts cannot resolve those lines.

`ask_billing` in `src/doctor.rs` runs before the scan from this host's
discovery (`src/cli.rs`, `doctor`). A destination would send its profile roots
before scanning, the coordinator ask about them with each labeled by host, and
send the answers back with the job, as `ssh::serve` already receives `since`
and `defaults`. Then `assets/demo/site.py` can answer every profile and the
results section can drop its "up to" note in `docs/releasing.md`.
