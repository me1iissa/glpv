//! Build `model::Job`s from the classified, fully-merged configuration.

use std::collections::HashMap;

use glpv_yaml::{Kind, Node};
use indexmap::IndexMap;

use crate::model::{
    self, AllowFailure, Contribution, ContributionKind, Forward, Need, NeedKind, Parallel,
    PipelineId, Severity, TriggerKind, Unresolved, UnresolvedReason, When,
};
use crate::resolve::classify::Classified;
use crate::resolve::context::ResolveState;
use crate::resolve::extends::Contributions;
use crate::resolve::parallel::{expand_names, parse_parallel};
use crate::rules::{parse_when, summarize_rules};
use crate::util::{leaf_spans, node_to_json};

/// Keys a job inherits from `default:` when it does not set them itself.
const DEFAULT_KEYS: [&str; 12] = [
    "image",
    "services",
    "cache",
    "before_script",
    "after_script",
    "tags",
    "retry",
    "timeout",
    "interruptible",
    "artifacts",
    "hooks",
    "id_tokens",
];

const LEAF_SPAN_CAP: usize = 300;

pub fn build_jobs(
    st: &mut ResolveState<'_>,
    classified: &Classified,
    stages: &[String],
    extends_contribs: &Contributions,
    reference_contribs: &Contributions,
    pipeline_id: &PipelineId,
) -> Vec<model::Job> {
    struct Draft {
        base_name: String,
        node: Node,
        key_span: glpv_yaml::Span,
        parallel: Option<Parallel>,
        names: Vec<String>,
        stage: String,
        default_contrib: Option<Contribution>,
    }

    let mut drafts = Vec::new();
    for (name, entry) in &classified.jobs {
        let mut node = entry.value.untag().clone();
        let mut default_contrib = None;
        if let Some(defaults) = &classified.defaults
            && let Some(applied) = apply_defaults(&mut node, defaults)
        {
            default_contrib = Some(applied);
        }
        let parallel = node.get("parallel").and_then(|p| parse_parallel(st, p));
        let names = match &parallel {
            Some(p) => expand_names(name, p),
            None => vec![name.clone()],
        };
        let stage = node
            .get("stage")
            .and_then(|s| s.scalar_text())
            .unwrap_or_else(|| "test".to_string());
        if !stages.contains(&stage) {
            st.diag_at(
                Severity::Error,
                "stage.unknown",
                format!("job `{name}` uses stage `{stage}`, which is not in `stages`"),
                Some(entry.key_span.into()),
            );
        }
        drafts.push(Draft {
            base_name: name.clone(),
            node,
            key_span: entry.key_span,
            parallel,
            names,
            stage,
            default_contrib,
        });
    }

    // Name tables for `needs` validation.
    let mut all_names: HashMap<String, usize> = HashMap::new(); // name -> stage index
    let mut expansions: HashMap<String, Vec<String>> = HashMap::new();
    for d in &drafts {
        let idx = stages
            .iter()
            .position(|s| *s == d.stage)
            .unwrap_or(usize::MAX);
        for n in &d.names {
            all_names.insert(n.clone(), idx);
        }
        if d.names.len() > 1 || d.names.first() != Some(&d.base_name) {
            expansions.insert(d.base_name.clone(), d.names.clone());
        }
    }

    let mut jobs = Vec::new();
    for d in &drafts {
        let stage_idx = stages
            .iter()
            .position(|s| *s == d.stage)
            .unwrap_or(usize::MAX);
        let needs = parse_needs(
            st,
            &d.base_name,
            &d.node,
            stage_idx,
            &all_names,
            &expansions,
        );
        let dependencies: Vec<String> = d
            .node
            .get("dependencies")
            .and_then(|n| n.untag().as_seq())
            .map(|s| s.iter().filter_map(|i| i.scalar_text()).collect())
            .unwrap_or_default();

        let rules = summarize_rules(
            st,
            d.node.get("rules"),
            d.node.get("only"),
            d.node.get("except"),
        );

        let trigger = d
            .node
            .get("trigger")
            .and_then(|t| parse_trigger(st, &d.base_name, t));
        let has_script = ["script", "run"].iter().any(|k| d.node.get(k).is_some());
        if trigger.is_some() && has_script {
            st.diag_at(
                Severity::Error,
                "trigger.with-script",
                format!("trigger job `{}` cannot also have a script", d.base_name),
                Some(d.key_span.into()),
            );
        }
        if trigger.is_none() && !has_script {
            st.diag_at(
                Severity::Error,
                "job.missing-script",
                format!("job `{}` needs a `script`, `run` or `trigger`", d.base_name),
                Some(d.key_span.into()),
            );
        }

        let when_text = d.node.get("when").and_then(|w| w.scalar_text());
        let when = match when_text.as_deref() {
            None => When::OnSuccess,
            Some(t) => match parse_when(t) {
                Some(w) => w,
                None => {
                    st.diag_at(
                        Severity::Error,
                        "job.invalid-when",
                        format!("`when: {t}` is not a valid value"),
                        Some(d.key_span.into()),
                    );
                    When::OnSuccess
                }
            },
        };

        let allow_failure = match d.node.get("allow_failure") {
            Some(af) => parse_allow_failure(af),
            // A manual job outside `rules` is non-blocking by default.
            None if when == When::Manual && rules.rules.is_empty() => AllowFailure::Bool(true),
            None => AllowFailure::Bool(false),
        };

        let environment = d
            .node
            .get("environment")
            .and_then(|e| match &e.untag().kind {
                Kind::Map(m) => m.get("name").and_then(|n| n.scalar_text()),
                _ => e.scalar_text(),
            });
        let image = d.node.get("image").and_then(|i| match &i.untag().kind {
            Kind::Map(m) => m.get("name").and_then(|n| n.scalar_text()),
            _ => i.scalar_text(),
        });
        let is_pages = d.base_name == "pages" || d.node.get("pages").is_some();
        let variables = crate::util::yaml_vars_map(d.node.get("variables"));

        let mut contributors: Vec<Contribution> = Vec::new();
        if let Some(c) = extends_contribs.get(&d.base_name) {
            contributors.extend(c.iter().cloned());
        }
        if let Some(c) = reference_contribs.get(&d.base_name) {
            contributors.extend(c.iter().cloned());
        }
        if let Some(c) = &d.default_contrib {
            contributors.push(c.clone());
        }

        let mut spans = IndexMap::new();
        if st.opts.full_provenance {
            leaf_spans(&d.node, "", &mut spans, LEAF_SPAN_CAP);
        }

        let merged_yaml = glpv_yaml::emit_document(&d.node);

        for (i, name) in d.names.iter().enumerate() {
            // Parallel/matrix expansions share their base job's configuration;
            // the heavy payload lives only on the first expansion and siblings
            // resolve through `base_name` (a 44-way rspec matrix would
            // otherwise repeat identical YAML 44 times).
            let first = i == 0;
            jobs.push(model::Job {
                id: model::job_id(pipeline_id, name),
                name: name.clone(),
                base_name: (name != &d.base_name).then(|| d.base_name.clone()),
                stage: d.stage.clone(),
                needs: needs.clone(),
                dependencies: dependencies.clone(),
                when,
                allow_failure: allow_failure.clone(),
                rules: if first {
                    rules.clone()
                } else {
                    model::RulesSummary {
                        mode: rules.mode,
                        rules: Vec::new(),
                    }
                },
                trigger: trigger.clone(),
                parallel: d.parallel.clone(),
                is_pages,
                environment: environment.clone(),
                image: image.clone(),
                variables: if first {
                    variables.clone()
                } else {
                    Default::default()
                },
                provenance: model::Provenance {
                    defined_at: d.key_span.into(),
                    contributors: if first {
                        contributors.clone()
                    } else {
                        Vec::new()
                    },
                    leaf_spans: if first {
                        spans.clone()
                    } else {
                        Default::default()
                    },
                },
                merged_yaml: if first {
                    merged_yaml.clone()
                } else {
                    String::new()
                },
                evaluations: Vec::new(),
            });
        }
    }
    jobs
}

/// Apply `default:` keys the job doesn't set. Returns a contribution when
/// anything was inherited.
fn apply_defaults(job: &mut Node, defaults: &Node) -> Option<Contribution> {
    // inherit:default: false | true | [keys]
    let inherit = job.get("inherit").and_then(|i| i.get("default")).cloned();
    let allowed: Option<Vec<String>> = match inherit.as_ref().map(|n| &n.untag().kind) {
        Some(Kind::Scalar(_)) => match inherit.as_ref().unwrap().as_bool() {
            Some(false) => return None,
            _ => None,
        },
        Some(Kind::Seq(items)) => Some(items.iter().filter_map(|i| i.scalar_text()).collect()),
        _ => None,
    };

    let dmap = defaults.untag().as_map()?;
    let jmap = job.as_map_mut()?;
    let mut applied = false;
    for key in DEFAULT_KEYS {
        if let Some(allow) = &allowed
            && !allow.iter().any(|a| a == key)
        {
            continue;
        }
        if !jmap.contains_key(key)
            && let Some(entry) = dmap.entries.get(key)
        {
            jmap.entries.insert(key.to_string(), entry.clone());
            applied = true;
        }
    }
    applied.then(|| Contribution {
        kind: ContributionKind::Default,
        span: defaults.span.into(),
    })
}

fn parse_needs(
    st: &mut ResolveState<'_>,
    job_name: &str,
    node: &Node,
    stage_idx: usize,
    all_names: &HashMap<String, usize>,
    expansions: &HashMap<String, Vec<String>>,
) -> Vec<Need> {
    let Some(needs_node) = node.get("needs") else {
        return Vec::new();
    };
    let Some(items) = needs_node.untag().as_seq() else {
        st.diag_at(
            Severity::Error,
            "needs.invalid",
            format!("`needs` of `{job_name}` must be a list"),
            Some(needs_node.span.into()),
        );
        return Vec::new();
    };

    let mut out = Vec::new();
    for item in items {
        let span: model::Span = item.span.into();
        let (target, optional, artifacts, project, git_ref, pipeline, matrix) =
            match &item.untag().kind {
                Kind::Scalar(_) => (
                    item.scalar_text().unwrap_or_default(),
                    false,
                    true,
                    None,
                    None,
                    None,
                    None,
                ),
                Kind::Map(m) => (
                    m.get("job")
                        .and_then(|j| j.scalar_text())
                        .unwrap_or_default(),
                    m.get("optional").and_then(|o| o.as_bool()).unwrap_or(false),
                    m.get("artifacts").and_then(|a| a.as_bool()).unwrap_or(true),
                    m.get("project").and_then(|p| p.scalar_text()),
                    m.get("ref").and_then(|r| r.scalar_text()),
                    m.get("pipeline").and_then(|p| p.scalar_text()),
                    m.get("parallel").and_then(|p| p.get("matrix")).cloned(),
                ),
                _ => {
                    st.diag_at(
                        Severity::Error,
                        "needs.invalid",
                        "each `needs` entry must be a job name or a mapping",
                        Some(span),
                    );
                    continue;
                }
            };

        if let Some(project) = project {
            out.push(Need {
                job: target,
                optional,
                artifacts,
                project: Some(project),
                git_ref,
                pipeline: None,
                kind: NeedKind::CrossProjectArtifact,
                unresolved: None,
                span,
            });
            continue;
        }
        if let Some(pipeline) = pipeline {
            out.push(Need {
                job: target,
                optional,
                artifacts,
                project: None,
                git_ref: None,
                pipeline: Some(pipeline),
                kind: NeedKind::ParentPipeline,
                unresolved: None,
                span,
            });
            continue;
        }
        if target.is_empty() {
            st.diag_at(
                Severity::Error,
                "needs.invalid",
                format!("a `needs` entry of `{job_name}` has no job name"),
                Some(span),
            );
            continue;
        }

        // `needs:parallel:matrix` selects specific expansions of the target.
        let targets: Vec<String> = if let Some(matrix_node) = &matrix {
            match matrix_subset(matrix_node) {
                Some(subset) => expand_names(&target, &Parallel::Matrix(subset)),
                None => vec![target.clone()],
            }
        } else if let Some(exp) = expansions.get(&target) {
            exp.clone()
        } else {
            vec![target.clone()]
        };

        for t in targets {
            let (kind, unresolved) = match all_names.get(&t) {
                Some(target_idx) => {
                    if *target_idx > stage_idx {
                        st.diag_at(
                            Severity::Error,
                            "needs.later-stage",
                            format!(
                                "`{job_name}` needs `{t}`, which is in a later stage; \
                                 needs must point at the same or an earlier stage"
                            ),
                            Some(span),
                        );
                    }
                    (NeedKind::Normal, None)
                }
                None if optional => (NeedKind::Normal, None),
                None => {
                    st.diag_at(
                        Severity::Error,
                        "needs.undefined",
                        format!("`{job_name}` needs `{t}`, which does not exist in the pipeline"),
                        Some(span),
                    );
                    (
                        NeedKind::Unresolved,
                        Some(Unresolved {
                            reason: UnresolvedReason::InvalidConfig,
                            detail: format!("job `{t}` does not exist"),
                            span: Some(span),
                        }),
                    )
                }
            };
            out.push(Need {
                job: t,
                optional,
                artifacts,
                project: None,
                git_ref: None,
                pipeline: None,
                kind,
                unresolved,
                span,
            });
        }
    }
    out
}

fn matrix_subset(node: &Node) -> Option<Vec<IndexMap<String, Vec<String>>>> {
    let entries = node.untag().as_seq()?;
    let mut out = Vec::new();
    for entry in entries {
        let vars = entry.untag().as_map()?;
        let mut dims: IndexMap<String, Vec<String>> = IndexMap::new();
        for (k, e) in vars.iter() {
            let values: Vec<String> = match &e.value.untag().kind {
                Kind::Seq(items) => items.iter().filter_map(|i| i.scalar_text()).collect(),
                _ => e.value.scalar_text().into_iter().collect(),
            };
            dims.insert(k.to_string(), values);
        }
        out.push(dims);
    }
    Some(out)
}

fn parse_allow_failure(node: &Node) -> AllowFailure {
    if let Some(b) = node.as_bool() {
        return AllowFailure::Bool(b);
    }
    if let Some(codes) = node.get("exit_codes") {
        let list: Vec<i64> = match &codes.untag().kind {
            Kind::Seq(items) => items.iter().filter_map(|i| i.as_int()).collect(),
            _ => codes.as_int().into_iter().collect(),
        };
        return AllowFailure::ExitCodes(list);
    }
    AllowFailure::Bool(false)
}

fn parse_trigger(st: &mut ResolveState<'_>, job_name: &str, node: &Node) -> Option<model::Trigger> {
    let mut strategy = None;
    let mut forward = Forward::default();
    let mut inputs = IndexMap::new();

    let kind = if let Some(project) = node.untag().scalar_text() {
        TriggerKind::MultiProject {
            project,
            branch: None,
            project_resolved: None,
            branch_resolved: None,
        }
    } else if let Some(map) = node.untag().as_map() {
        if let Some(s) = map.get("strategy").and_then(|s| s.scalar_text()) {
            if s == "depend" || s == "mirror" {
                strategy = Some(s);
            } else {
                st.diag_at(
                    Severity::Error,
                    "trigger.invalid-strategy",
                    format!("`strategy: {s}` should be `depend` or `mirror`"),
                    Some(node.span.into()),
                );
            }
        }
        if let Some(f) = map.get("forward").and_then(|f| f.as_map()) {
            if let Some(b) = f.get("yaml_variables").and_then(|b| b.as_bool()) {
                forward.yaml_variables = b;
            }
            if let Some(b) = f.get("pipeline_variables").and_then(|b| b.as_bool()) {
                forward.pipeline_variables = b;
            }
        }
        if let Some(i) = map.get("inputs").and_then(|i| i.as_map()) {
            for (k, e) in i.iter() {
                inputs.insert(k.to_string(), node_to_json(&e.value));
            }
        }

        if let Some(include) = map.get("include") {
            let entries: Vec<&Node> = match &include.untag().kind {
                Kind::Seq(items) => items.iter().collect(),
                _ => vec![include],
            };
            if entries.len() > 3 {
                st.diag_at(
                    Severity::Error,
                    "trigger.too-many-includes",
                    format!(
                        "`{job_name}` triggers a child pipeline with {} config files; GitLab allows at most 3",
                        entries.len()
                    ),
                    Some(include.span.into()),
                );
            }
            let artifact_entry = entries.iter().find_map(|e| {
                let m = e.untag().as_map()?;
                Some((
                    m.get("artifact")?.scalar_text()?,
                    m.get("job")
                        .and_then(|j| j.scalar_text())
                        .unwrap_or_default(),
                ))
            });
            match artifact_entry {
                Some((artifact, job)) if entries.len() == 1 => {
                    TriggerKind::DynamicChild { artifact, job }
                }
                _ => TriggerKind::Child {
                    includes: entries.iter().map(|e| node_to_json(e)).collect(),
                },
            }
        } else if let Some(project) = map.get("project").and_then(|p| p.scalar_text()) {
            TriggerKind::MultiProject {
                project,
                branch: map.get("branch").and_then(|b| b.scalar_text()),
                project_resolved: None,
                branch_resolved: None,
            }
        } else {
            st.diag_at(
                Severity::Error,
                "trigger.invalid",
                format!("trigger of `{job_name}` must specify either `project` or `include`"),
                Some(node.span.into()),
            );
            return None;
        }
    } else {
        st.diag_at(
            Severity::Error,
            "trigger.invalid",
            format!("trigger of `{job_name}` must be a project path or a mapping"),
            Some(node.span.into()),
        );
        return None;
    };

    Some(model::Trigger {
        kind,
        strategy,
        forward,
        inputs,
        target: None,
    })
}
