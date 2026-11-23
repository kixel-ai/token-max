# [M] Move the site to max.kixel.me and rework its results

## Goal

The landing page lives at https://max.kixel.me as the maintainer's personal
project, and https://max.kixel.ai answers every path with a 301 to the same
path there, so `/install.sh`, `/install.ps1`, `/run.sh` and `/run.ps1` links
already shared keep working. The page's results section shows the
maintainer's savings both on a subscription and at API rates, each measured,
with a ranked breakdown of where they come from.

## Plan

Decided 2026-11-22 (maintainer, /quest-run), after trying the deployed page
for [the promote quest](/quest/m2/promote-site.md). The maintainer said it
"looks nice" and asked for these tweaks:

- Domain: ✅ max.kixel.me, with max.kixel.ai answering a 301 to it for every
  path (rejected: token-max.kixel.me, kixel.me/token-max). The bare kixel.me
  redirects to the maintainer's Discord and stays that way. Both zones use
  the same Cloudflare nameservers (`alexis`/`dana`); check that they are on the
  same account. Update every `max.kixel.ai` mention in the README, `docs/`,
  `site/` and the run scripts, apart from the redirect itself.
- Header: the top right holds a round avatar, the name "Luke Curley", and
  icon links. Socials: ✅ GitHub https://github.com/kixelated, X
  https://x.com/kixelated, Discord https://discord.gg/7wPdgDw (rejected:
  also linking moq.dev). Avatar placement: ✅ top right with the socials
  (rejected: beside the example heading, footer byline). Serve the GitHub
  avatar and the icons from the site itself, since `_headers` allows only the
  page's own origin.
- Under "For example, on my devices across the last 30 days", in small text:
  "Yes i do a lot of vibe coding", exactly as written.
- Remove the "Run it to see yours →" button.
- Results: ✅ two columns (rejected: one API-rate headline only). The shared
  context line above them gives tokens, requests and machines. The "On my
  subscription" column gives the percent saved, phrased as limits lasting
  longer, with no dollars. The "If I paid API rates" column gives the percent
  saved and the dollars per month, followed by "(hell no)". Today's page
  shows 7% on the subscription. At API rates it adds the 3 changes whose
  saving depends on how a profile pays, "up to ~$9,572/mo more", which comes
  to roughly 22%. Measure each column with its own doctor run rather than
  adding up the "up to" figure: the maintainer's real payment answers, then
  every profile answered as API. The terminal keeps showing the real run.
- ✅ Remove the meter and put a ranked breakdown in its place (rejected: no
  breakdown). It lists the biggest savings by setting, then one "everything
  else" row, and tags each line that saves only at API rates "API only".
  `assets/demo/site.py` fills it from the runs it already parses.
- The footnote shrinks to about two lines, for example "Replayed at public
  API prices on 3 of my machines." Keep what the figures need to stay true:
  `~` marks extrapolated figures.

Retake `og.png` once the page's top changes, as `docs/releasing.md` describes,
and deploy both hosts. The page is public: name no private repository,
customer or project.
