//! Shared renderer helpers.

use glpv_core::model::{Graph, Job, JobId, RulesMode, When};

/// How a trigger edge should be drawn, derived from its bridge job's rules.
pub struct EdgeStyle {
    /// Dotted/dashed: the trigger only fires under conditions (or manually).
    pub conditional: bool,
    /// Short label: strategy plus the gating condition, e.g.
    /// `depend · if $CI_COMMIT_BRANCH == "main"` or `manual`.
    pub label: String,
}

pub fn bridge_job<'g>(graph: &'g Graph, from_job: &JobId) -> Option<&'g Job> {
    graph
        .pipelines
        .iter()
        .flat_map(|p| &p.jobs)
        .find(|j| &j.id == from_job)
}

pub fn trigger_edge_style(graph: &Graph, from_job: &JobId, strategy: Option<&str>) -> EdgeStyle {
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = strategy {
        parts.push(s.to_string());
    }
    let mut conditional = false;
    if let Some(job) = bridge_job(graph, from_job) {
        let manual = job.rules.mode == RulesMode::Manual || job.when == When::Manual;
        match job.rules.mode {
            RulesMode::Manual => {
                conditional = true;
                parts.push("manual".to_string());
            }
            RulesMode::Conditional | RulesMode::Legacy => {
                conditional = true;
                match first_condition(job) {
                    Some(c) => parts.push(c),
                    None => parts.push("conditional".to_string()),
                }
            }
            RulesMode::Never => {
                conditional = true;
                parts.push("never".to_string());
            }
            _ if manual => {
                conditional = true;
                parts.push("manual".to_string());
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        parts.push("trigger".to_string());
    }
    EdgeStyle {
        conditional,
        label: parts.join(" · "),
    }
}

fn first_condition(job: &Job) -> Option<String> {
    for clause in &job.rules.rules {
        if let Some(cond) = &clause.r#if {
            return Some(format!("if {}", truncate(cond, 36)));
        }
        if clause.changes.is_some() {
            return Some("if changes".to_string());
        }
        if clause.exists.is_some() {
            return Some("if exists".to_string());
        }
        if clause.legacy.is_some() {
            return Some("only/except".to_string());
        }
    }
    None
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n).collect();
        format!("{cut}…")
    }
}
