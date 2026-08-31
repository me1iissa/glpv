//! The canonical rules evaluator, compiled to WebAssembly for the viewer.
//!
//! The HTML export embeds this module (base64) next to the graph JSON. The
//! page initialises it once with the graph, then re-evaluates the whole
//! graph through the same `glpv_core::rules` code the CLI uses — the
//! hand-mirrored JS evaluator remains only as a fallback when WebAssembly
//! is unavailable.
//!
//! ABI (no wasm-bindgen; plain exports so the JS glue stays ~40 lines):
//! - `glpv_alloc(len) -> ptr` / `glpv_dealloc(ptr, len)`
//! - `glpv_init(ptr, len) -> 0|1`     graph JSON in
//! - `glpv_eval(ptr, len) -> ptr|0`   sim JSON in, result JSON out
//! - `glpv_result_len() -> len`       byte length of the last eval result

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use glpv_core::model::{JobEvaluation, Outcome, RulesSummary, When};
use glpv_core::rules::{EvalContext, evaluate_rules};
use glpv_core::vars::{VarState, VarTable};

/* ================= lean graph mirror =================
 * Only the fields evaluation needs; tolerant of extra keys. */

#[derive(Deserialize)]
struct WGraph {
    pipelines: Vec<WPipeline>,
}

#[derive(Deserialize)]
struct WPipeline {
    id: String,
    kind: String,
    project: WProject,
    #[serde(default)]
    git_ref: Option<String>,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    config_path: String,
    #[serde(default)]
    variables: IndexMap<String, String>,
    #[serde(default)]
    workflow_rules: Option<RulesSummary>,
    #[serde(default)]
    jobs: Vec<WJob>,
}

#[derive(Deserialize)]
struct WProject {
    host: String,
    path: String,
}

#[derive(Deserialize)]
struct WJob {
    id: String,
    name: String,
    #[serde(default)]
    base_name: Option<String>,
    when: When,
    rules: RulesSummary,
    #[serde(default)]
    variables: IndexMap<String, String>,
}

#[derive(Deserialize)]
struct Sim {
    #[serde(default)]
    source: String,
    #[serde(default, rename = "ref")]
    git_ref: String,
    #[serde(default)]
    tag: bool,
    #[serde(default)]
    vars: Vec<(String, String)>,
    /// When set, the full trace for this job id is included in the result.
    #[serde(default)]
    trace_job: Option<String>,
    /// Simulation-wide assumption for `rules:changes` (None = undecided).
    #[serde(default)]
    assume_changes: Option<bool>,
    /// Simulation-wide assumption for `rules:exists` (None = undecided).
    #[serde(default)]
    assume_exists: Option<bool>,
}

/// Per-base outcome: `[outcome, blocked_by]`. Sibling expansions share it.
#[derive(Serialize)]
struct Out {
    /// pipeline id → base job name → [outcome, blocked_by|null]
    pipelines: HashMap<String, HashMap<String, (Outcome, Option<String>)>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace: Option<JobEvaluation>,
}

/* ================= evaluation (mirrors the viewer's semantics) ========== */

fn slugify(s: &str) -> String {
    let mut out: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    out.truncate(63);
    out.trim_matches('-').to_string()
}

struct Facts {
    source: String,
    ref_name: String,
    is_tag: bool,
}

fn facts_of(p: &WPipeline, sim: &Sim) -> Facts {
    let simulated = p.kind == "root" || p.kind == "detached";
    let default_branch = p.default_branch.as_deref().unwrap_or("main");
    let ref_name = if simulated && !sim.git_ref.is_empty() {
        sim.git_ref.clone()
    } else {
        p.git_ref
            .clone()
            .unwrap_or_else(|| default_branch.to_string())
    };
    let source = if simulated {
        sim.source.clone()
    } else if p.kind == "child" || p.kind == "dynamic_child" {
        "parent_pipeline".to_string()
    } else {
        "pipeline".to_string()
    };
    Facts {
        source,
        ref_name,
        is_tag: simulated && sim.tag,
    }
}

fn pipeline_vars(p: &WPipeline, facts: &Facts, sim: &Sim) -> VarTable {
    let mut t = VarTable::default();
    let host = &p.project.host;
    let path = &p.project.path;
    let name = path.rsplit('/').next().unwrap_or(path);
    let ns = path.rsplit_once('/').map(|(a, _)| a).unwrap_or("");
    let db = p.default_branch.as_deref().unwrap_or("main");

    t.set_known("CI", "true");
    t.set_known("GITLAB_CI", "true");
    t.set_known("CI_SERVER_HOST", host);
    t.set_known("CI_SERVER_FQDN", host);
    t.set_known("CI_SERVER_URL", format!("https://{host}"));
    t.set_known("CI_API_V4_URL", format!("https://{host}/api/v4"));
    t.set_known("CI_PROJECT_PATH", path);
    t.set_known("CI_PROJECT_NAME", name);
    t.set_known("CI_PROJECT_NAMESPACE", ns);
    t.set_known(
        "CI_PROJECT_ROOT_NAMESPACE",
        path.split('/').next().unwrap_or(path),
    );
    t.set_known("CI_PROJECT_PATH_SLUG", slugify(path));
    t.set_known("CI_PROJECT_URL", format!("https://{host}/{path}"));
    t.set_known("CI_DEFAULT_BRANCH", db);
    t.set_known("CI_CONFIG_PATH", &p.config_path);
    t.set_known("CI_PIPELINE_SOURCE", &facts.source);
    t.set_known("CI_COMMIT_REF_NAME", &facts.ref_name);
    t.set_known("CI_COMMIT_REF_SLUG", slugify(&facts.ref_name));
    if let Some(sha) = &p.sha {
        t.set_known("CI_COMMIT_SHA", sha);
        t.set_known("CI_COMMIT_SHORT_SHA", &sha[..sha.len().min(8)]);
    }
    if facts.is_tag {
        t.set_known("CI_COMMIT_TAG", &facts.ref_name);
        t.set("CI_COMMIT_BRANCH", VarState::Unset);
    } else if facts.source == "merge_request_event" {
        t.set("CI_COMMIT_TAG", VarState::Unset);
        t.set("CI_COMMIT_BRANCH", VarState::Unset);
        t.set_known("CI_MERGE_REQUEST_SOURCE_BRANCH_NAME", &facts.ref_name);
        t.set_known("CI_MERGE_REQUEST_TARGET_BRANCH_NAME", db);
    } else {
        t.set("CI_COMMIT_TAG", VarState::Unset);
        t.set_known("CI_COMMIT_BRANCH", &facts.ref_name);
    }
    for (k, v) in &p.variables {
        t.set_known(k, v);
    }
    for (k, v) in &sim.vars {
        if k.is_empty() {
            continue;
        }
        if v == "(unset)" {
            t.set(k, VarState::Unset);
        } else {
            t.set_known(k, v);
        }
    }
    t
}

fn eval_all(g: &WGraph, sim: &Sim) -> Out {
    let mut out = Out {
        pipelines: HashMap::with_capacity(g.pipelines.len()),
        trace: None,
    };
    // Traces are computed for the base of the requested job (siblings share).
    let trace_base: Option<(&str, String)> = sim.trace_job.as_deref().and_then(|id| {
        g.pipelines.iter().find_map(|p| {
            p.jobs
                .iter()
                .find(|j| j.id == id)
                .map(|j| (p.id.as_str(), base_of(j).to_string()))
        })
    });

    for p in &g.pipelines {
        let facts = facts_of(p, sim);
        let pvars = pipeline_vars(p, &facts, sim);
        let assume_changes = move |_: &[String]| sim.assume_changes;
        let assume_exists = move |_: &[String]| sim.assume_exists;
        let wf_outcome = p.workflow_rules.as_ref().map(|wf| {
            let ctx = EvalContext {
                vars: &pvars,
                exists: Some(&assume_exists),
                changes: Some(&assume_changes),
                source: &facts.source,
                ref_name: &facts.ref_name,
                is_tag: facts.is_tag,
            };
            evaluate_rules(wf, &ctx, "sim", When::OnSuccess).outcome
        });

        let mut by_base: HashMap<String, (Outcome, Option<String>)> = HashMap::new();
        for j in &p.jobs {
            let base = base_of(j);
            if by_base.contains_key(base) {
                continue;
            }
            let mut vars = pvars.clone();
            for (k, v) in &j.variables {
                vars.set_known(k, v);
            }
            for (k, v) in &sim.vars {
                if k.is_empty() {
                    continue;
                }
                if v == "(unset)" {
                    vars.set(k, VarState::Unset);
                } else {
                    vars.set_known(k, v);
                }
            }
            let ctx = EvalContext {
                vars: &vars,
                exists: Some(&assume_exists),
                changes: Some(&assume_changes),
                source: &facts.source,
                ref_name: &facts.ref_name,
                is_tag: facts.is_tag,
            };
            let mut eval = evaluate_rules(&j.rules, &ctx, "sim", j.when);
            match wf_outcome {
                Some(Outcome::Skipped) => {
                    eval.outcome = Outcome::Blocked;
                    eval.blocked_by = Some("workflow:rules".to_string());
                }
                Some(Outcome::Unknown) if eval.outcome != Outcome::Skipped => {
                    eval.outcome = Outcome::Unknown;
                    eval.blocked_by = Some("workflow:rules undecided".to_string());
                }
                _ => {}
            }
            if let Some((tp, tb)) = &trace_base
                && *tp == p.id
                && tb == base
            {
                out.trace = Some(eval.clone());
            }
            by_base.insert(base.to_string(), (eval.outcome, eval.blocked_by));
        }
        out.pipelines.insert(p.id.clone(), by_base);
    }
    out
}

fn base_of(j: &WJob) -> &str {
    j.base_name.as_deref().unwrap_or(&j.name)
}

/* ================= wasm ABI ================= */

thread_local! {
    static GRAPH: RefCell<Option<WGraph>> = const { RefCell::new(None) };
    static RESULT_LEN: Cell<usize> = const { Cell::new(0) };
}

/// # Safety
/// Caller owns the returned buffer and must pass it back to `glpv_dealloc`.
#[unsafe(no_mangle)]
pub extern "C" fn glpv_alloc(len: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(len.max(1));
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr
}

/// # Safety
/// `ptr`/`len` must come from `glpv_alloc` (or an eval result), exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glpv_dealloc(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        unsafe { drop(Vec::from_raw_parts(ptr, 0, len.max(1))) };
    }
}

/// # Safety
/// `ptr..ptr+len` must be valid UTF-8 graph JSON written via `glpv_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glpv_init(ptr: *const u8, len: usize) -> i32 {
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return 1;
    };
    match serde_json::from_str::<WGraph>(text) {
        Ok(g) => {
            GRAPH.with(|c| *c.borrow_mut() = Some(g));
            0
        }
        Err(_) => 1,
    }
}

/// # Safety
/// `ptr..ptr+len` must be valid UTF-8 sim JSON written via `glpv_alloc`.
/// Returns a result buffer (length via `glpv_result_len`) or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn glpv_eval(ptr: *const u8, len: usize) -> *mut u8 {
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return std::ptr::null_mut();
    };
    let Ok(sim) = serde_json::from_str::<Sim>(text) else {
        return std::ptr::null_mut();
    };
    GRAPH.with(|c| {
        let borrow = c.borrow();
        let Some(g) = borrow.as_ref() else {
            return std::ptr::null_mut();
        };
        let Ok(mut json) = serde_json::to_vec(&eval_all(g, &sim)) else {
            return std::ptr::null_mut();
        };
        json.shrink_to_fit();
        let out = json.as_mut_ptr();
        RESULT_LEN.with(|l| l.set(json.len()));
        std::mem::forget(json);
        out
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn glpv_result_len() -> usize {
    RESULT_LEN.with(|l| l.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The same entry points the page uses, exercised natively.
    #[test]
    fn init_and_eval_roundtrip() {
        let graph = serde_json::json!({
            "pipelines": [{
                "id": "p-1", "kind": "root",
                "project": {"host": "example.com", "path": "grp/app"},
                "git_ref": "main", "default_branch": "main", "config_path": ".gitlab-ci.yml",
                "workflow_rules": null,
                "jobs": [{
                    "id": "p-1/build", "name": "build", "when": "on_success",
                    "rules": {"mode": "conditional", "rules": [{
                        "if": "$CI_COMMIT_BRANCH == \"main\"",
                        "span": {"file": 0, "start": [1,1], "end": [1,1]}
                    }]}
                }]
            }]
        });
        let g: WGraph = serde_json::from_value(graph).unwrap();
        let sim = Sim {
            source: "push".into(),
            git_ref: String::new(),
            tag: false,
            vars: vec![],
            trace_job: Some("p-1/build".into()),
            assume_changes: None,
            assume_exists: None,
        };
        let out = eval_all(&g, &sim);
        assert_eq!(out.pipelines["p-1"]["build"].0, Outcome::Runs);
        assert!(out.trace.is_some());

        let sim2 = Sim {
            git_ref: "feature/x".into(),
            ..sim
        };
        let out2 = eval_all(&g, &sim2);
        assert_eq!(out2.pipelines["p-1"]["build"].0, Outcome::Skipped);
    }
}
