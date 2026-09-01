//! The comparison behind `glpv check`: the locally merged configuration
//! against the server's `merged_yaml`, and the jobs the server says would be
//! created against the local rules evaluation. Pure functions over the graph
//! and the lint API's response, so the integration tests can drive them
//! without a server.

use std::collections::{BTreeMap, HashMap};

use glpv_core::model::{AllowFailure, Job, Outcome, Pipeline, When};
use glpv_yaml::FileId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Top-level keys that carry no meaning once includes are expanded.
const DROP_TOP_LEVEL: &[&str] = &["include"];
/// Stages the server injects into `stages:` (glpv keeps them implicit).
const IMPLICIT_STAGES: &[&str] = &[".pre", ".post"];

/// The lint API's response (`POST /projects/:id/ci/lint`).
#[derive(Debug, Default, Deserialize)]
pub struct Oracle {
    #[serde(default)]
    pub valid: Option<bool>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub merged_yaml: Option<String>,
    /// Present with `include_jobs=true`: the jobs a dry-run pipeline on the
    /// ref would create, after rules.
    #[serde(default)]
    pub jobs: Option<Vec<OracleJob>>,
    /// Error envelopes (`{"message": …}` / `{"error": …}`).
    #[serde(default)]
    pub message: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
}

/// A job of a pipeline the server actually created, as the Jobs and Bridges
/// APIs describe it (`GET /projects/:id/pipelines/:id/jobs` and `/bridges`).
/// Both are readable with the job token of any job in that pipeline, so a
/// comparison against them needs no other credential.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineJob {
    pub name: String,
    #[serde(default)]
    pub stage: String,
    /// Only set for jobs whose `when` is not the default.
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub allow_failure: bool,
    #[serde(default)]
    pub status: String,
}

/// The oracle's view of a real pipeline: names, stages, `allow_failure` and
/// the `when` values the API reports; no scripts (the API has none).
pub fn pipeline_jobs_to_oracle<'a>(
    jobs: impl IntoIterator<Item = &'a PipelineJob>,
) -> Vec<OracleJob> {
    jobs.into_iter()
        .map(|j| OracleJob {
            name: j.name.clone(),
            stage: j.stage.clone(),
            script: Vec::new(),
            before_script: Vec::new(),
            after_script: Vec::new(),
            when: j.when.clone().unwrap_or_default(),
            allow_failure: j.allow_failure,
        })
        .collect()
}

/// Which fields `compare_jobs_with` holds the two sides to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompareFields {
    /// `script`, `before_script`, `after_script` (the lint API has them).
    pub scripts: bool,
    /// `when` — compared only for jobs where the server reports one.
    pub when: bool,
}

impl CompareFields {
    /// Everything the lint API returns.
    pub const ALL: CompareFields = CompareFields {
        scripts: true,
        when: true,
    };
    /// What the Jobs API of a real pipeline can vouch for.
    pub const PIPELINE: CompareFields = CompareFields {
        scripts: false,
        when: true,
    };
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OracleJob {
    pub name: String,
    #[serde(default)]
    pub stage: String,
    #[serde(default)]
    pub script: Vec<String>,
    #[serde(default)]
    pub before_script: Vec<String>,
    #[serde(default)]
    pub after_script: Vec<String>,
    #[serde(default)]
    pub when: String,
    #[serde(default)]
    pub allow_failure: bool,
}

/// Parse a YAML document (Psych semantics) into JSON.
pub fn yaml_to_json(text: &str) -> anyhow::Result<Value> {
    let (mut docs, _) = glpv_yaml::parse(FileId(0), text).map_err(|e| anyhow::anyhow!("{e}"))?;
    let root = docs
        .drain(..)
        .next()
        .and_then(|d| d.root)
        .ok_or_else(|| anyhow::anyhow!("the document is empty"))?;
    Ok(glpv_core::util::node_to_json(&root))
}

/// Sort object keys recursively so key order never counts as a difference.
pub fn normalize(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let sorted: BTreeMap<String, Value> =
                m.into_iter().map(|(k, v)| (k, normalize(v))).collect();
            let mut out = Map::with_capacity(sorted.len());
            for (k, v) in sorted {
                out.insert(k, v);
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.into_iter().map(normalize).collect()),
        other => other,
    }
}

/// Normalise a merged configuration for comparison: drop the expanded
/// `include:`, the implicit `.pre`/`.post` stages, and key order.
pub fn normalize_root(mut v: Value) -> Value {
    if let Value::Object(m) = &mut v {
        for k in DROP_TOP_LEVEL {
            m.remove(*k);
        }
        if let Some(Value::Array(stages)) = m.get_mut("stages") {
            stages.retain(|s| !matches!(s.as_str(), Some(x) if IMPLICIT_STAGES.contains(&x)));
        }
    }
    normalize(v)
}

/// Unified diff of two normalised configurations, or `None` when identical.
pub fn unified_diff(local: &Value, server: &Value) -> Option<String> {
    let a = serde_json::to_string_pretty(local).unwrap_or_default() + "\n";
    let b = serde_json::to_string_pretty(server).unwrap_or_default() + "\n";
    if a == b {
        return None;
    }
    let diff = similar::TextDiff::from_lines(&a, &b);
    Some(
        diff.unified_diff()
            .context_radius(3)
            .header("glpv (local)", "gitlab (lint API)")
            .to_string(),
    )
}

/// A job as glpv resolved and evaluated it, in the oracle's terms.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalJob {
    pub name: String,
    pub stage: String,
    pub when: String,
    pub allow_failure: bool,
    pub script: Vec<String>,
    pub before_script: Vec<String>,
    pub after_script: Vec<String>,
    pub outcome: Outcome,
}

fn when_text(w: When) -> String {
    serde_json::to_value(w)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "on_success".to_string())
}

/// GitLab flattens nested script arrays into one list of lines.
pub fn flatten_lines(v: Option<&Value>) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Null => {}
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::String(s) => out.push(s.clone()),
            other => out.push(other.to_string()),
        }
    }
    if let Some(v) = v {
        walk(v, &mut out);
    }
    out
}

/// Every job of a pipeline with its effective `when` / `allow_failure` (the
/// matched rule's, else the job's) and its script lines from the merged YAML.
pub fn local_jobs(p: &Pipeline) -> Vec<LocalJob> {
    let mut payload: HashMap<String, &Job> = HashMap::new();
    for j in &p.jobs {
        let base = j.base_name.clone().unwrap_or_else(|| j.name.clone());
        payload.entry(base).or_insert(j);
    }
    let mut docs: HashMap<String, Value> = HashMap::new();
    p.jobs
        .iter()
        .map(|j| {
            let base = j.base_name.clone().unwrap_or_else(|| j.name.clone());
            let src = payload.get(&base).copied().unwrap_or(j);
            let doc = docs
                .entry(base.clone())
                .or_insert_with(|| yaml_to_json(&src.merged_yaml).unwrap_or(Value::Null))
                .clone();
            let ev = j.evaluations.first();
            let outcome = ev.map(|e| e.outcome).unwrap_or(Outcome::Unknown);
            let matched = ev.and_then(|e| e.trace.iter().find(|t| t.result == "matched"));
            let clause = matched.and_then(|t| src.rules.rules.get(t.index));
            let when = match outcome {
                Outcome::Manual => "manual".to_string(),
                Outcome::Delayed => "delayed".to_string(),
                _ => when_text(matched.and_then(|t| t.when).unwrap_or(j.when)),
            };
            let allow_failure =
                clause
                    .and_then(|c| c.allow_failure)
                    .unwrap_or(match &j.allow_failure {
                        AllowFailure::Bool(b) => *b,
                        AllowFailure::ExitCodes(_) => true,
                    });
            LocalJob {
                name: j.name.clone(),
                stage: j.stage.clone(),
                when,
                allow_failure,
                script: flatten_lines(doc.get("script")),
                before_script: flatten_lines(doc.get("before_script")),
                after_script: flatten_lines(doc.get("after_script")),
                outcome,
            }
        })
        .collect()
}

#[derive(Debug, Default, PartialEq)]
pub struct JobReport {
    /// glpv expects these to run; the server would not create them.
    pub missing_on_server: Vec<String>,
    /// The server would create these; glpv says skipped/blocked (or does not know them).
    pub unexpected_on_server: Vec<String>,
    /// glpv could not decide (unknown variables); not counted as a difference.
    pub undecided: Vec<String>,
    /// (job, field, local, server)
    pub field_mismatches: Vec<(String, String, String, String)>,
    pub compared: usize,
}

impl JobReport {
    pub fn is_clean(&self) -> bool {
        self.missing_on_server.is_empty()
            && self.unexpected_on_server.is_empty()
            && self.field_mismatches.is_empty()
    }
}

pub fn compare_jobs(local: &[LocalJob], server: &[OracleJob]) -> JobReport {
    compare_jobs_with(local, server, CompareFields::ALL)
}

pub fn compare_jobs_with(
    local: &[LocalJob],
    server: &[OracleJob],
    fields: CompareFields,
) -> JobReport {
    let mut report = JobReport::default();
    let by_name: HashMap<&str, &OracleJob> = server.iter().map(|j| (j.name.as_str(), j)).collect();
    let mut seen: Vec<&str> = Vec::new();
    for l in local {
        let s = by_name.get(l.name.as_str()).copied();
        match l.outcome {
            Outcome::Unknown => report.undecided.push(l.name.clone()),
            Outcome::Runs | Outcome::Manual | Outcome::Delayed => match s {
                None => report.missing_on_server.push(l.name.clone()),
                Some(s) => {
                    seen.push(s.name.as_str());
                    report.compared += 1;
                    let mut field = |name: &str, a: String, b: String| {
                        if a != b {
                            report
                                .field_mismatches
                                .push((l.name.clone(), name.to_string(), a, b));
                        }
                    };
                    field("stage", l.stage.clone(), s.stage.clone());
                    if fields.when && !s.when.is_empty() {
                        field("when", l.when.clone(), s.when.clone());
                    }
                    field(
                        "allow_failure",
                        l.allow_failure.to_string(),
                        s.allow_failure.to_string(),
                    );
                    if fields.scripts {
                        field("script", l.script.join("\n"), s.script.join("\n"));
                        field(
                            "before_script",
                            l.before_script.join("\n"),
                            s.before_script.join("\n"),
                        );
                        field(
                            "after_script",
                            l.after_script.join("\n"),
                            s.after_script.join("\n"),
                        );
                    }
                }
            },
            Outcome::Skipped | Outcome::Blocked => {
                if s.is_some() {
                    report.unexpected_on_server.push(l.name.clone());
                    seen.push(l.name.as_str());
                }
            }
        }
    }
    let known: Vec<&str> = local.iter().map(|l| l.name.as_str()).collect();
    for s in server {
        if !known.contains(&s.name.as_str()) {
            report.unexpected_on_server.push(s.name.clone());
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalisation_ignores_order_include_and_implicit_stages() {
        let local =
            yaml_to_json("stages: [build, test]\nb: 2\na: {y: 1, x: [1, {q: 1, p: 2}]}\n").unwrap();
        let server = yaml_to_json(
            "---\ninclude: [{local: x.yml}]\na:\n  x:\n  - 1\n  - p: 2\n    q: 1\n  y: 1\nb: 2\nstages:\n- \".pre\"\n- build\n- test\n- \".post\"\n",
        )
        .unwrap();
        assert_eq!(normalize_root(local), normalize_root(server));
        assert!(
            unified_diff(
                &normalize_root(yaml_to_json("a: 1").unwrap()),
                &normalize_root(yaml_to_json("a: 2").unwrap())
            )
            .is_some()
        );
    }

    #[test]
    fn script_lines_flatten_like_gitlab() {
        let v = yaml_to_json("script:\n  - a\n  - [b, [c]]\n  - 3\n").unwrap();
        assert_eq!(flatten_lines(v.get("script")), vec!["a", "b", "c", "3"]);
        assert!(flatten_lines(None).is_empty());
    }

    fn lj(name: &str, outcome: Outcome) -> LocalJob {
        LocalJob {
            name: name.into(),
            stage: "test".into(),
            when: "on_success".into(),
            allow_failure: false,
            script: vec!["make".into()],
            before_script: vec![],
            after_script: vec![],
            outcome,
        }
    }
    fn oj(name: &str) -> OracleJob {
        OracleJob {
            name: name.into(),
            stage: "test".into(),
            script: vec!["make".into()],
            before_script: vec![],
            after_script: vec![],
            when: "on_success".into(),
            allow_failure: false,
        }
    }

    #[test]
    fn job_comparison_reports_presence_and_fields() {
        let local = vec![
            lj("a", Outcome::Runs),
            lj("b", Outcome::Skipped),
            lj("c", Outcome::Unknown),
            lj("d", Outcome::Runs),
            LocalJob {
                when: "manual".into(),
                ..lj("e", Outcome::Manual)
            },
        ];
        let server = vec![oj("a"), oj("b"), oj("e"), oj("zzz")];
        let r = compare_jobs(&local, &server);
        assert_eq!(r.compared, 2);
        assert_eq!(r.missing_on_server, vec!["d"]);
        assert_eq!(r.unexpected_on_server, vec!["b", "zzz"]);
        assert_eq!(r.undecided, vec!["c"]);
        assert_eq!(r.field_mismatches.len(), 1);
        assert_eq!(r.field_mismatches[0].0, "e");
        assert_eq!(r.field_mismatches[0].1, "when");
        assert!(!r.is_clean());
        assert!(compare_jobs(&[lj("a", Outcome::Runs)], &[oj("a")]).is_clean());
    }
}
