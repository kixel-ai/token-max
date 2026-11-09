//! Schema version 2: aggregate metadata without history-sized evidence arrays.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

use crate::model::*;
use crate::prices::add_costs;

fn value(value: impl Serialize) -> Value {
    serde_json::to_value(value).expect("report metadata serializes")
}

/// Count distinct evidence conditions without retaining their request identifiers.
fn groups(rows: impl IntoIterator<Item = Value>) -> Value {
    let mut groups = BTreeMap::<String, (Value, u64)>::new();
    let mut count = 0u64;
    for row in rows {
        let key = row.to_string();
        groups.entry(key).or_insert((row, 0)).1 += 1;
        count += 1;
    }
    json!({"count": count, "groups": groups.into_values().map(|(mut row, count)| {
        row["count"] = json!(count);
        row
    }).collect::<Vec<_>>()})
}

fn cache(rows: &[CacheReplayEvidence]) -> Value {
    let mut result = groups(rows.iter().map(|r| {
        json!({
            "ttl": r.ttl, "state": r.state, "outcome": r.outcome, "reason": r.reason
        })
    }));
    result["predecessors"] = groups(rows.iter().flat_map(|r| &r.predecessors).map(|p| {
        json!({
            "state": p.state, "prefixEvidence": p.prefix_evidence,
            "cohortKnown": p.cohort_known, "reason": p.reason
        })
    }));
    result
}

fn estimates(rows: &[UsageEstimate]) -> Value {
    groups(rows.iter().map(|r| {
        json!({
            "evidence": r.evidence, "reason": r.reason, "assumptions": r.assumptions
        })
    }))
}

fn periods(rows: &[IdlePeriod]) -> Value {
    let mut result = groups(rows.iter().map(|p| {
        json!({
            "status": p.status, "reason": p.reason, "confirmed": p.confirmed,
            "returnTiming": p.return_timing, "outcome": p.outcome,
            "version": p.version, "client": p.client, "flagState": p.flag_state,
            "activation": p.activation, "activationBasis": p.activation_basis
        })
    }));
    result["summaryCost"] = value(add_costs(rows.iter().map(|p| &p.summary_cost)));
    result
}

fn added(rows: &[AddedCompaction]) -> Value {
    json!({
        "count": rows.len(),
        "inputTokens": range(rows.iter().map(|r| r.input_tokens)),
        "summaryTokens": range(rows.iter().map(|r| r.summary_tokens)),
        "prefixTokens": range(rows.iter().map(|r| r.prefix_tokens)),
        "summaryCost": add_costs(rows.iter().map(|r| &r.summary_cost)),
        "prefixCost": add_costs(rows.iter().map(|r| &r.prefix_cost))
    })
}

fn range(rows: impl Iterator<Item = i64> + Clone) -> Value {
    json!({"min": rows.clone().min(), "max": rows.max()})
}

fn idle(scenario: &mut IdleScenario) -> (Value, Value) {
    let periods = periods(&std::mem::take(&mut scenario.periods));
    let sizes = std::mem::take(&mut scenario.by_context);
    let context = json!({
        "count": sizes.len(),
        "tokens": range(sizes.iter().filter_map(|s| s.tokens)),
        "unknownSizes": sizes.iter().filter(|s| s.tokens.is_none()).count(),
        "periods": sizes.iter().map(|s| s.periods).sum::<u64>(),
        "deltaUsd": sizes.iter().map(|s| s.delta_usd).sum::<Option<f64>>()
    });
    (periods, context)
}

fn check(mut check: CheckResult) -> Value {
    // Strip large vectors before constructing any JSON values. The remaining typed check
    // keeps every cost, coverage, owner and sensitivity, including newly added check kinds.
    let mut replacements: Vec<(&str, Value)> = vec![];
    match &mut check {
        CheckResult::Ttl(c) => {
            replacements.push((
                "usageEstimates",
                estimates(&std::mem::take(&mut c.usage_estimates)),
            ));
            replacements.push((
                "cacheEvidence",
                cache(&std::mem::take(&mut c.cache_evidence)),
            ));
        }
        CheckResult::Compaction(c) => {
            replacements.push((
                "usageEstimates",
                estimates(&std::mem::take(&mut c.usage_estimates)),
            ));
            replacements.push((
                "cacheEvidence",
                cache(&std::mem::take(&mut c.cache_evidence)),
            ));
            let scenarios = c
                .scenarios
                .iter_mut()
                .map(|s| {
                    let added = added(&std::mem::take(&mut s.added));
                    let cache = cache(&std::mem::take(&mut s.cache_evidence));
                    let mut result = value(s);
                    result["added"] = added;
                    result["cacheEvidence"] = cache;
                    result
                })
                .collect::<Vec<_>>();
            c.scenarios.clear();
            replacements.push(("scenarios", value(scenarios)));
        }
        CheckResult::Idle(c) => {
            replacements.push((
                "usageEstimates",
                estimates(&std::mem::take(&mut c.usage_estimates)),
            ));
            for (name, scenario) in [
                ("currentSettings", &mut c.current_settings),
                ("afterRecommendations", &mut c.after_recommendations),
            ] {
                let (periods, context) = idle(scenario);
                let mut result = value(scenario);
                result["periods"] = periods;
                result["byContext"] = context;
                replacements.push((name, result));
            }
        }
        CheckResult::Speed(c) => {
            replacements.push((
                "usageEstimates",
                estimates(&std::mem::take(&mut c.usage_estimates)),
            ));
        }
        CheckResult::Background(_) | CheckResult::BashCap(_) | CheckResult::Keepalive(_) => {}
    }
    let mut result = value(check);
    for (key, replacement) in replacements {
        result[key] = replacement;
    }
    result
}

fn visibility(scan: &ExposureScan) -> Value {
    let components = scan
        .components
        .iter()
        .map(|c| (c.id.as_str(), c))
        .collect::<std::collections::HashMap<_, _>>();
    groups(scan.visibility.iter().map(|v| json!({
        "component": v.component.as_ref().map(|id| {
            let c = components[id.as_str()];
            // These names identify individual messages/calls, not settings. Keep
            // their semantic target and provenance without recreating a call catalog.
            let name = (!matches!(c.provenance.as_str(),
                "unclassified-rendered-attachment" | "observed-tool-result-not-wire-proof" | "unmapped-rendered-delta"
            )).then_some(&c.name);
            json!({"name": name, "category": c.category, "targets": c.targets, "provenance": c.provenance})
        }),
        "harness": v.harness, "client": v.client, "version": v.version, "profiles": v.profiles,
        "exposure": v.exposure, "directCalls": v.direct_calls, "nestedCalls": v.nested_calls,
        "typedSkills": v.typed_skills, "automaticUnused": v.automatic_unused, "reasons": v.reasons
    })))
}

/// Consume the full analysis, releasing raw evidence before serializing its compact shape.
pub(crate) fn report(mut report: Report) -> Value {
    let checks = std::mem::take(&mut report.checks)
        .into_iter()
        .map(check)
        .collect::<Vec<_>>();
    let compactions = std::mem::take(&mut report.compactions);
    let charges = json!({
        "evidence": groups(compactions.iter().map(|c| json!({"evidence": c.evidence, "assumptions": c.assumptions}))),
        "cost": add_costs(compactions.iter().map(|c| &c.cost)),
        "ttlCost": {
            "5m": add_costs(compactions.iter().map(|c| &c.ttl_cost.five_minutes)),
            "1h": add_costs(compactions.iter().map(|c| &c.ttl_cost.one_hour))
        }
    });
    drop(compactions);
    let calibrations = std::mem::take(&mut report.exposure.calibration).into_iter().map(|c| json!({
        "cohort": c.cohort, "samples": c.samples.len(), "tokensPerCharacter": c.tokens_per_character,
        "range": c.range, "assumptions": c.assumptions
    })).collect::<Vec<_>>();
    let scan = &mut report.scan;
    let snapshots = std::mem::take(&mut scan.snapshots);
    let files = json!({
        "count": snapshots.len(), "bytes": snapshots.iter().map(|s| s.bytes).sum::<u64>(),
        "deferredBytes": snapshots.iter().map(|s| s.deferred_bytes).sum::<u64>(),
        "skippedLines": snapshots.iter().map(|s| s.skipped_lines).sum::<u64>()
    });
    drop(snapshots);
    let requests = std::mem::take(&mut scan.requests).len();
    let events = std::mem::take(&mut scan.events).len();
    let cache = std::mem::take(&mut scan.cache_context);
    let cache_counts = json!({"requests": cache.requests.len(), "boundaries": cache.boundaries.len(), "prompts": cache.prompts.len()});
    drop(cache);
    let exposure = std::mem::take(&mut scan.exposure);
    let visibility = visibility(&exposure);
    let exposure = json!({
        "components": exposure.components.len(), "requests": exposure.requests.len(),
        "invocations": exposure.invocations.len(), "visibility": visibility,
        "toolCalls": exposure.tool_calls, "toolResults": exposure.tool_results
    });
    report.schema_version = 2;
    let mut result = value(report);
    result["checks"] = value(checks);
    result["compactions"] = charges;
    result["exposure"]["calibration"] = value(calibrations);
    let scan = &mut result["scan"];
    scan["snapshots"] = files;
    scan["requests"] = value(requests);
    scan["events"] = value(events);
    scan["cacheContext"] = cache_counts;
    scan["exposure"] = exposure;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_off_coverage_groups_by_target_instead_of_call_identity() {
        let component = |id: &str| Component {
            instruction_size: None,
            size: Size {
                characters: None,
                recorded_tokens: None,
                fingerprint: id.into(),
            },
            id: id.into(),
            key: id.into(),
            name: id.into(),
            category: Category::SkillBody,
            owner: Owner::Skills,
            group: None,
            provenance: "observed-tool-result-not-wire-proof".into(),
            locations: vec![],
            first_seen: 0,
            current: Current::Unresolved,
            scope: Scope::Component,
            members: vec![],
            targets: vec!["same-skill".into()],
        };
        let row = |id: &str| Visibility {
            harness: Harness::Claude,
            client: None,
            version: None,
            profiles: vec!["profile".into()],
            component: Some(id.into()),
            since: 0,
            until: 1,
            exposure: Visible::Partial,
            direct_calls: Observed::Unknown,
            nested_calls: Visible::Unknown,
            typed_skills: Observed::Unknown,
            automatic_unused: false,
            reasons: vec!["synthetic incomplete invocation coverage".into()],
        };
        let scan = ExposureScan {
            components: vec![component("call-a"), component("call-b")],
            visibility: vec![row("call-a"), row("call-b")],
            ..ExposureScan::default()
        };
        let compact = visibility(&scan);
        assert_eq!(compact["count"], 2);
        let groups = compact["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["count"], 2);
        assert_eq!(groups[0]["component"]["name"], Value::Null);
        assert_eq!(groups[0]["component"]["targets"], json!(["same-skill"]));
        assert_eq!(
            groups[0]["reasons"],
            json!(["synthetic incomplete invocation coverage"])
        );
    }

    #[test]
    fn visibility_groups_keep_component_identity_and_unknown_reasons() {
        let component = |id: &str, name: &str| Component {
            instruction_size: None,
            size: Size {
                characters: None,
                recorded_tokens: None,
                fingerprint: id.into(),
            },
            id: id.into(),
            key: name.into(),
            name: name.into(),
            category: Category::ToolSchema,
            owner: Owner::Context,
            group: None,
            provenance: "synthetic".into(),
            locations: vec![],
            first_seen: 0,
            current: Current::Unresolved,
            scope: Scope::Component,
            members: vec![],
            targets: vec![],
        };
        let row = |id: &str| Visibility {
            harness: Harness::Claude,
            client: None,
            version: None,
            profiles: vec!["profile".into()],
            component: Some(id.into()),
            since: 0,
            until: 1,
            exposure: Visible::Unknown,
            direct_calls: Observed::Unknown,
            nested_calls: Visible::Unknown,
            typed_skills: Observed::Unknown,
            automatic_unused: false,
            reasons: vec!["synthetic incomplete exposure".into()],
        };
        let scan = ExposureScan {
            components: vec![
                component("a", "Tool A"),
                component("b", "Tool B"),
                component("a2", "Tool A"),
            ],
            visibility: vec![row("a"), row("b"), row("a2")],
            ..ExposureScan::default()
        };
        let compact = visibility(&scan);
        assert_eq!(compact["count"], 3);
        let groups = compact["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        let a = groups
            .iter()
            .find(|g| g["component"]["name"] == "Tool A")
            .unwrap();
        assert_eq!(a["count"], 2);
        assert_eq!(a["component"]["category"], "tool-schema");
        assert_eq!(a["directCalls"], "unknown");
        assert_eq!(a["reasons"], json!(["synthetic incomplete exposure"]));
    }
}
