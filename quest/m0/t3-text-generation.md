# [S] Report T3's text-generation model

## Goal

Each T3 instance gets one report line for `textGenerationModelSelection`,
the model T3 uses for thread titles, branch names, PR text and other
generated text: the model it resolves to, its cost where native records
exist, and plainly "unmeasured" where T3's ephemeral sessions leave none,
naming the setting that changes it.

## Plan

On the maintainer's host (T3 0.1.7) `textGenerationModelSelection` is unset
in `settings.json`, so it resolves to a provider default. Its settings panel
says source control can override it. The report today mentions only that
helper generation "can use ephemeral sessions" with unmeasured cost, and has
no line for the setting.

Settled 2026-11-09 (maintainer, /quest-plan): ✅ report the setting and its
cost (rejected: also recommend a cheaper model outright, since model selection
is outside m0). ✅ m0 (rejected: m1).
