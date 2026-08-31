//! Static `rules:` summarisation (M1). The scenario evaluator arrives in M4;
//! until then jobs are classified by shape: Always / Conditional / Manual /
//! Never / Legacy.

use glpv_yaml::{Kind, Node};
use indexmap::IndexMap;

use crate::model::{LegacyOnlyExcept, RuleClause, RulesMode, RulesSummary, Severity, When};
use crate::resolve::context::ResolveState;
use crate::util::node_to_json;

pub fn parse_when(text: &str) -> Option<When> {
    match text {
        "on_success" => Some(When::OnSuccess),
        "on_failure" => Some(When::OnFailure),
        "always" => Some(When::Always),
        "manual" => Some(When::Manual),
        "delayed" => Some(When::Delayed),
        "never" => Some(When::Never),
        _ => None,
    }
}

/// Summarise a job's (or `workflow:`'s) rules-related keys.
pub fn summarize_rules(
    st: &mut ResolveState<'_>,
    rules: Option<&Node>,
    only: Option<&Node>,
    except: Option<&Node>,
) -> RulesSummary {
    if only.is_some() || except.is_some() {
        if rules.is_some() {
            st.diag(
                Severity::Error,
                "rules.mixed-with-only-except",
                "`rules` cannot be combined with `only`/`except` in the same job",
            );
        }
        let span = only.or(except).map(|n| n.span).unwrap();
        return RulesSummary {
            mode: RulesMode::Legacy,
            rules: vec![RuleClause {
                r#if: None,
                changes: None,
                exists: None,
                when: None,
                allow_failure: None,
                variables: IndexMap::new(),
                span: span.into(),
                legacy: Some(LegacyOnlyExcept {
                    only: only.map(node_to_json),
                    except: except.map(node_to_json),
                }),
            }],
        };
    }

    let Some(rules) = rules else {
        return RulesSummary::none();
    };
    let Some(items) = rules.untag().as_seq() else {
        st.diag_at(
            Severity::Error,
            "rules.invalid",
            "`rules` must be a list",
            Some(rules.span.into()),
        );
        return RulesSummary::none();
    };

    let mut clauses = Vec::new();
    for item in items {
        match &item.untag().kind {
            Kind::Map(m) => {
                let when = m.get("when").and_then(|w| w.scalar_text()).and_then(|t| {
                    let parsed = parse_when(&t);
                    if parsed.is_none() {
                        st.diag_at(
                            Severity::Error,
                            "rules.invalid-when",
                            format!("`when: {t}` is not a valid value"),
                            Some(item.span.into()),
                        );
                    }
                    parsed
                });
                let changes = m.get("changes").map(|c| match &c.untag().kind {
                    // `changes: { paths: [...], compare_to: ... }`
                    Kind::Map(cm) => cm
                        .get("paths")
                        .and_then(|p| p.as_seq())
                        .map(|s| s.iter().filter_map(|i| i.scalar_text()).collect())
                        .unwrap_or_default(),
                    Kind::Seq(s) => s.iter().filter_map(|i| i.scalar_text()).collect(),
                    _ => c.scalar_text().into_iter().collect(),
                });
                let exists = m.get("exists").map(|e| match &e.untag().kind {
                    Kind::Map(em) => em
                        .get("paths")
                        .and_then(|p| p.as_seq())
                        .map(|s| s.iter().filter_map(|i| i.scalar_text()).collect())
                        .unwrap_or_default(),
                    Kind::Seq(s) => s.iter().filter_map(|i| i.scalar_text()).collect(),
                    _ => e.scalar_text().into_iter().collect(),
                });
                clauses.push(RuleClause {
                    r#if: m.get("if").and_then(|n| n.scalar_text()),
                    changes,
                    exists,
                    when,
                    allow_failure: m.get("allow_failure").and_then(|n| n.as_bool()),
                    variables: m
                        .get("variables")
                        .and_then(|v| v.as_map())
                        .map(|vm| {
                            vm.iter()
                                .map(|(k, e)| {
                                    (k.to_string(), e.value.scalar_text().unwrap_or_default())
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    span: item.span.into(),
                    legacy: None,
                });
            }
            _ => st.diag_at(
                Severity::Error,
                "rules.invalid",
                "each rule must be a mapping",
                Some(item.span.into()),
            ),
        }
    }

    RulesSummary {
        mode: classify(&clauses),
        rules: clauses,
    }
}

/// Shape-based classification, pre-evaluation.
fn classify(clauses: &[RuleClause]) -> RulesMode {
    if clauses.is_empty() {
        return RulesMode::None;
    }
    // Walk in order; a condition-free clause terminates the chain.
    let mut any_conditional = false;
    for c in clauses {
        let unconditional = c.r#if.is_none() && c.changes.is_none() && c.exists.is_none();
        let when = c.when.unwrap_or(When::OnSuccess);
        if unconditional {
            return match when {
                When::Never => {
                    if any_conditional {
                        RulesMode::Conditional
                    } else {
                        RulesMode::Never
                    }
                }
                When::Manual => RulesMode::Manual,
                _ => {
                    if any_conditional {
                        RulesMode::Conditional
                    } else {
                        RulesMode::Always
                    }
                }
            };
        }
        if when == When::Manual {
            // A conditional manual clause: the job may become a manual gate.
            return RulesMode::Manual;
        }
        any_conditional = true;
    }
    if clauses.iter().all(|c| c.when == Some(When::Never)) {
        RulesMode::Never
    } else {
        RulesMode::Conditional
    }
}

pub mod expr;

use crate::model::{JobEvaluation, Outcome, RuleTrace};
use crate::vars::{VarState, VarTable};
use expr::Tri;

/// `rules:exists` oracle: patterns → matched?, `None` when undecidable.
pub type ExistsChecker<'a> = &'a dyn Fn(&[String]) -> Option<bool>;

/// Context for evaluating a job's rules under one scenario.
pub struct EvalContext<'a> {
    pub vars: &'a VarTable,
    /// Evaluate `rules:exists` patterns against the repo tree; `None` = unknown.
    pub exists: Option<ExistsChecker<'a>>,
    /// Evaluate `rules:changes` patterns against a (simulated) diff;
    /// `None` = unknown.
    pub changes: Option<ExistsChecker<'a>>,
    /// Simulated ref facts for legacy only/except.
    pub source: &'a str,
    pub ref_name: &'a str,
    pub is_tag: bool,
}

fn state_text(s: &VarState) -> String {
    match s {
        VarState::Known(v) => format!("\"{v}\""),
        VarState::Unset => "unset".to_string(),
        VarState::Unknown => "unknown".to_string(),
    }
}

fn outcome_of_when(when: When) -> Outcome {
    match when {
        When::Never => Outcome::Skipped,
        When::Manual => Outcome::Manual,
        When::Delayed => Outcome::Delayed,
        _ => Outcome::Runs,
    }
}

/// Evaluate a summarised rules chain. `job_when` is the job-level `when`
/// (a matching clause without its own `when` inherits it).
pub fn evaluate_rules(
    summary: &crate::model::RulesSummary,
    ctx: &EvalContext<'_>,
    scenario_id: &str,
    job_when: When,
) -> JobEvaluation {
    use crate::model::RulesMode;

    if summary.mode == RulesMode::Legacy {
        return evaluate_legacy(summary, ctx, scenario_id, job_when);
    }
    if summary.rules.is_empty() {
        return JobEvaluation {
            scenario_id: scenario_id.to_string(),
            outcome: outcome_of_when(job_when),
            trace: Vec::new(),
            blocked_by: None,
        };
    }

    let mut trace = Vec::new();
    let mut decided: Option<Outcome> = None;
    for (index, clause) in summary.rules.iter().enumerate() {
        if decided.is_some() {
            trace.push(RuleTrace {
                index,
                result: "not_reached".to_string(),
                clause: clause_text(clause),
                when: clause.when,
                vars_used: Vec::new(),
                note: None,
            });
            continue;
        }
        let mut result = Tri::True;
        let mut vars_used = Vec::new();
        let mut note: Option<String> = None;

        if let Some(cond) = &clause.r#if {
            let r = expr::eval_if(cond, ctx.vars);
            vars_used = r
                .vars_used
                .iter()
                .map(|(n, s)| (n.clone(), state_text(s)))
                .collect();
            if !r.notes.is_empty() {
                note = Some(r.notes.join("; "));
            }
            result = and_tri(result, r.result);
        }
        if let Some(patterns) = &clause.exists {
            let e = match ctx.exists {
                Some(f) => match f(patterns) {
                    Some(b) => b.into(),
                    None => Tri::Unknown,
                },
                None => Tri::Unknown,
            };
            if e == Tri::Unknown {
                note.get_or_insert_with(|| "exists: undecidable here".to_string());
            }
            result = and_tri(result, e);
        }
        if let Some(patterns) = &clause.changes {
            let c = match ctx.changes {
                Some(f) => match f(patterns) {
                    Some(b) => b.into(),
                    None => Tri::Unknown,
                },
                None => Tri::Unknown,
            };
            if c == Tri::Unknown {
                note.get_or_insert_with(|| {
                    "changes: depends on the diff; undecidable statically".to_string()
                });
            }
            result = and_tri(result, c);
        }

        let when = clause.when.unwrap_or(job_when);
        match result {
            Tri::True => decided = Some(outcome_of_when(when)),
            Tri::Unknown => decided = Some(Outcome::Unknown),
            Tri::False => {}
        }
        trace.push(RuleTrace {
            index,
            result: match result {
                Tri::True => "matched".to_string(),
                Tri::False => "no_match".to_string(),
                Tri::Unknown => "unknown".to_string(),
            },
            clause: clause_text(clause),
            when: Some(when),
            vars_used,
            note,
        });
    }

    JobEvaluation {
        scenario_id: scenario_id.to_string(),
        // No clause matched → the job is not added to the pipeline.
        outcome: decided.unwrap_or(Outcome::Skipped),
        trace,
        blocked_by: None,
    }
}

fn and_tri(a: Tri, b: Tri) -> Tri {
    match (a, b) {
        (Tri::False, _) | (_, Tri::False) => Tri::False,
        (Tri::Unknown, _) | (_, Tri::Unknown) => Tri::Unknown,
        _ => Tri::True,
    }
}

fn clause_text(clause: &crate::model::RuleClause) -> String {
    let mut parts = Vec::new();
    if let Some(i) = &clause.r#if {
        parts.push(format!("if: {i}"));
    }
    if let Some(c) = &clause.changes {
        parts.push(format!("changes: [{}]", c.join(", ")));
    }
    if let Some(e) = &clause.exists {
        parts.push(format!("exists: [{}]", e.join(", ")));
    }
    if parts.is_empty() {
        parts.push("(always)".to_string());
    }
    parts.join(" AND ")
}

/// Legacy `only`/`except`, refs lists only; anything richer is Unknown.
fn evaluate_legacy(
    summary: &crate::model::RulesSummary,
    ctx: &EvalContext<'_>,
    scenario_id: &str,
    job_when: When,
) -> JobEvaluation {
    let legacy = summary.rules.first().and_then(|c| c.legacy.as_ref());
    let Some(legacy) = legacy else {
        return JobEvaluation {
            scenario_id: scenario_id.to_string(),
            outcome: Outcome::Unknown,
            trace: Vec::new(),
            blocked_by: None,
        };
    };

    let refs_of = |v: &serde_json::Value| -> Option<Vec<String>> {
        match v {
            serde_json::Value::Array(items) => Some(
                items
                    .iter()
                    .filter_map(|i| i.as_str().map(|s| s.to_string()))
                    .collect(),
            ),
            serde_json::Value::Object(map) => {
                // refs-only maps are decidable; other keys are not.
                if map.keys().all(|k| k == "refs") {
                    refs_of_opt(map.get("refs"))
                } else {
                    None
                }
            }
            _ => None,
        }
    };
    fn refs_of_opt(v: Option<&serde_json::Value>) -> Option<Vec<String>> {
        match v {
            None => Some(Vec::new()),
            Some(serde_json::Value::Array(items)) => Some(
                items
                    .iter()
                    .filter_map(|i| i.as_str().map(|s| s.to_string()))
                    .collect(),
            ),
            _ => None,
        }
    }

    let matches_ref = |pattern: &str| -> Option<bool> {
        Some(match pattern {
            "branches" => {
                !ctx.is_tag
                    && matches!(
                        ctx.source,
                        "push"
                            | "web"
                            | "pipeline"
                            | "parent_pipeline"
                            | "trigger"
                            | "api"
                            | "schedule"
                    )
            }
            "tags" => ctx.is_tag,
            "merge_requests" => ctx.source == "merge_request_event",
            "schedules" => ctx.source == "schedule",
            "web" => ctx.source == "web",
            "api" => ctx.source == "api",
            "triggers" => ctx.source == "trigger",
            "pipelines" => ctx.source == "pipeline",
            "pushes" => ctx.source == "push",
            "external" => ctx.source == "external",
            "chat" => ctx.source == "chat",
            p if p.starts_with('/') => {
                let body = p.trim_matches('/');
                match regex::Regex::new(body) {
                    Ok(re) => re.is_match(ctx.ref_name),
                    Err(_) => return None,
                }
            }
            p => p == ctx.ref_name,
        })
    };

    let evaluate_list = |list: Option<Vec<String>>, default: bool| -> Option<bool> {
        match list {
            None => None,
            Some(l) if l.is_empty() => Some(default),
            Some(l) => {
                let mut any = false;
                for p in &l {
                    match matches_ref(p) {
                        Some(true) => any = true,
                        Some(false) => {}
                        None => return None,
                    }
                }
                Some(any)
            }
        }
    };

    // A job with only/except unset defaults to only: [branches, tags].
    let only = match &legacy.only {
        Some(v) => refs_of(v),
        None => Some(vec!["branches".to_string(), "tags".to_string()]),
    };
    let except = match &legacy.except {
        Some(v) => refs_of(v),
        None => Some(Vec::new()),
    };

    let outcome = match (evaluate_list(only, true), evaluate_list(except, false)) {
        (Some(o), Some(e)) => {
            if o && !e {
                outcome_of_when(job_when)
            } else {
                Outcome::Skipped
            }
        }
        _ => Outcome::Unknown,
    };
    JobEvaluation {
        scenario_id: scenario_id.to_string(),
        outcome,
        trace: vec![RuleTrace {
            index: 0,
            result: match outcome {
                Outcome::Skipped => "no_match".to_string(),
                Outcome::Unknown => "unknown".to_string(),
                _ => "matched".to_string(),
            },
            clause: "legacy only/except".to_string(),
            when: Some(job_when),
            vars_used: Vec::new(),
            note: matches!(outcome, Outcome::Unknown)
                .then(|| "only/except uses conditions beyond refs; undecidable".to_string()),
        }],
        blocked_by: None,
    }
}
