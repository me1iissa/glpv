//! Resolution of one pipeline: entry file (or a synthetic `trigger:include`
//! document) → includes → extends → !reference → stages → jobs, mirroring
//! GitLab's processing order exactly.

pub mod classify;
pub mod context;
pub mod document;
pub mod extends;
pub mod includes;
pub mod jobs;
pub mod merge;
pub mod parallel;
pub mod reference;
pub mod stages;

use std::sync::Arc;

use glpv_yaml::{FileId, Node};
use indexmap::IndexMap;

use crate::diff::DiffOracle;
use crate::model::{
    self, Diagnostic, IncludeKind, PipelineId, PipelineKind, Severity, Unresolved, UnresolvedReason,
};
use crate::source::{FileOrigin, ProjectSource, SourceMap, Sources, TreeRef};
use crate::vars::{Scenario, predefined_vars};

pub use context::{Frame, ResolveOpts, ResolveState};

/// What the pipeline's configuration is.
pub enum Entry {
    /// A config file (usually `.gitlab-ci.yml`) read from `config_source`
    /// (or the pipeline's own project when `None` — the normal case; a
    /// `ci_config_path` of the `file@group/project` form points elsewhere).
    ConfigPath {
        path: String,
        config_source: Option<Arc<dyn ProjectSource>>,
    },
    /// A `trigger:include` child: the synthetic document `include: <node>`,
    /// resolved in the parent project at the parent sha.
    ChildIncludes {
        include_node: Node,
        from_file: FileId,
        from_path: String,
        label: String,
    },
}

pub struct PipelineRequest {
    pub project: Arc<dyn ProjectSource>,
    pub tree: TreeRef,
    pub git_ref: Option<String>,
    pub entry: Entry,
    pub scenario: Scenario,
    pub kind: PipelineKind,
    pub depth: u32,
    pub parent: Option<(PipelineId, String)>,
    /// Inputs supplied by the including/triggering side (spec:inputs values).
    pub inputs: IndexMap<String, Node>,
    /// The diff `rules:changes` is evaluated against (shared with child
    /// pipelines, which inherit the parent's).
    pub diff: Option<Arc<DiffOracle>>,
    /// `diff` is the parent's: the graph JSON then records only this
    /// pipeline's `compare_to` lists, not the files again.
    pub diff_inherited: bool,
    /// Whether this pipeline has a changed-paths set at all (see
    /// `rules::changes::has_push_event`).
    pub push_event: bool,
}

pub struct PipelineOutcome {
    pub pipeline: model::Pipeline,
    /// The fully merged root (extends/!reference resolved), for `glpv resolve`
    /// and for the trigger walk.
    pub merged_root: Option<Node>,
    pub include_files: Vec<model::IncludeFile>,
    pub include_edges: Vec<model::IncludeEdge>,
}

pub fn resolve_pipeline(
    files: &mut SourceMap,
    diags: &mut Vec<Diagnostic>,
    opts: &ResolveOpts,
    sources: &Sources,
    req: PipelineRequest,
) -> PipelineOutcome {
    let meta = req.project.meta().clone();
    let project_ref = meta.project_ref();
    let config_label = match &req.entry {
        Entry::ConfigPath { path, .. } => path.clone(),
        Entry::ChildIncludes { label, .. } => label.clone(),
    };
    let pipeline_id = model::pipeline_id(
        &project_ref,
        req.git_ref.as_deref(),
        &config_label,
        req.parent.as_ref().map(|(p, j)| (p, j.as_str())),
    );
    let sha = match &req.tree {
        TreeRef::Commit(s) => Some(s.0.clone()),
        TreeRef::Worktree => None,
    };

    let mut st = ResolveState {
        files,
        diags,
        opts,
        sources,
        pipeline_id: pipeline_id.clone(),
        include_files: Vec::new(),
        include_edges: Vec::new(),
        budget_used: 0,
        stack: Vec::new(),
        order_counter: 0,
        diff: req.diff.clone(),
        push_event: req.push_event,
        spec_inputs: Vec::new(),
        last_spec_inputs: Vec::new(),
        child_entry_file: None,
    };

    let make_pipeline = |st: &ResolveState<'_>,
                         entry_source: Option<u32>,
                         stages: Vec<String>,
                         jobs: Vec<model::Job>,
                         workflow: Option<model::RulesSummary>,
                         unresolved: Option<Unresolved>| {
        model::Pipeline {
            id: pipeline_id.clone(),
            kind: req.kind,
            project: project_ref.clone(),
            git_ref: req.git_ref.clone(),
            sha: sha.clone(),
            config_path: config_label.clone(),
            default_branch: None,
            diff: None,
            variables: indexmap::IndexMap::new(),
            entry_source,
            stages,
            jobs,
            workflow_rules: workflow,
            unresolved,
            parent: req.parent.clone(),
            depth: req.depth,
            spec_inputs: st.spec_inputs.clone(),
            includes: st
                .include_files
                .iter()
                .map(|f| f.file)
                .filter(|f| *f != u32::MAX)
                .collect(),
        }
    };

    let fail = |st: &mut ResolveState<'_>, reason: UnresolvedReason, detail: String| {
        let pipeline = make_pipeline(
            st,
            None,
            Vec::new(),
            Vec::new(),
            None,
            Some(Unresolved {
                reason,
                detail,
                span: None,
            }),
        );
        PipelineOutcome {
            pipeline,
            merged_root: None,
            include_files: std::mem::take(&mut st.include_files),
            include_edges: std::mem::take(&mut st.include_edges),
        }
    };

    let default_branch = match req.project.default_branch() {
        Ok(b) => b,
        Err(e) => {
            st.diag(Severity::Warning, "source.default-branch", e.to_string());
            "main".to_string()
        }
    };
    let vars = predefined_vars(
        &meta,
        &default_branch,
        sha.as_deref(),
        &config_label,
        &req.scenario,
    );

    // Obtain the body and the frame it lives in.
    let (body, frame) = match &req.entry {
        Entry::ConfigPath {
            path,
            config_source,
        } => {
            let source = config_source.clone().unwrap_or_else(|| req.project.clone());
            let source_tree = if config_source.is_some() {
                // A remote-config project's file lives at its own default branch.
                match source
                    .default_branch()
                    .ok()
                    .and_then(|b| source.resolve_ref(&b).ok().flatten())
                {
                    Some(sha) => TreeRef::Commit(sha),
                    None => req.tree.clone(),
                }
            } else {
                req.tree.clone()
            };
            let text = match source.read(&source_tree, path) {
                Ok(Some(t)) => t,
                Ok(None) => {
                    let detail = format!(
                        "`{path}` does not exist in {} at {}",
                        source.meta().display_path,
                        req.git_ref.as_deref().unwrap_or("the working tree"),
                    );
                    st.diag_hint(
                        Severity::Error,
                        "config.not-found",
                        detail.clone(),
                        None,
                        "an Auto DevOps pipeline may apply if the feature is enabled for the project",
                    );
                    return fail(&mut st, UnresolvedReason::FileNotFound, detail);
                }
                Err(e) => {
                    let detail = format!("cannot read `{path}`: {e}");
                    st.diag(Severity::Error, "source.error", detail.clone());
                    return fail(&mut st, UnresolvedReason::FileNotFound, detail);
                }
            };
            let entry_sha = match &source_tree {
                TreeRef::Commit(s) => Some(s.0.clone()),
                TreeRef::Worktree => None,
            };
            let entry_file = st.files.insert(
                FileOrigin {
                    project: Some(source.meta().project_ref()),
                    sha: entry_sha.clone(),
                    path: path.clone(),
                },
                text.clone(),
            );
            st.include_files.push(model::IncludeFile {
                file: entry_file.0,
                project: Some(source.meta().project_ref()),
                sha: entry_sha,
                path: path.clone(),
                kind: IncludeKind::Entry,
                location: path.clone(),
                unresolved: None,
            });
            let frame = Frame {
                project: source,
                tree: source_tree,
                file: entry_file,
                file_path: path.clone(),
            };
            st.stack.push(frame.stack_key());
            let body = document::load_document(&mut st, entry_file, &text, &req.inputs);
            st.spec_inputs = std::mem::take(&mut st.last_spec_inputs);
            let Some(body) = body else {
                return fail(
                    &mut st,
                    UnresolvedReason::InvalidConfig,
                    "the configuration could not be parsed".to_string(),
                );
            };
            (
                includes::expand_includes(&mut st, body, &frame, &vars),
                frame,
            )
        }
        Entry::ChildIncludes {
            include_node,
            from_file,
            from_path,
            ..
        } => {
            let frame = Frame {
                project: req.project.clone(),
                tree: req.tree.clone(),
                file: *from_file,
                file_path: from_path.clone(),
            };
            let body = Node::map(glpv_yaml::Map::default(), include_node.span);
            st.child_entry_file = Some(*from_file);
            (
                includes::expand_include_node(&mut st, body, include_node, &frame, &vars),
                frame,
            )
        }
    };
    let _ = frame;

    let mut merged = body;
    let extends_contribs = extends::resolve_extends(&mut merged, st.diags);
    let reference_contribs = reference::resolve_references(&mut merged, st.diags);

    let stage_list = stages::final_stages(&mut st, &merged);
    let classified = classify::classify_top_level(&mut st, &merged);

    let workflow_rules = classified
        .workflow
        .as_ref()
        .map(|w| crate::rules::summarize_rules(&mut st, w.get("rules"), None, None));

    let jobs = jobs::build_jobs(
        &mut st,
        &classified,
        &stage_list,
        &extends_contribs,
        &reference_contribs,
        &pipeline_id,
    );

    if jobs.is_empty() {
        st.diag(
            Severity::Warning,
            "config.no-jobs",
            "no visible jobs found in the configuration",
        );
    }

    let entry_source = st
        .include_files
        .iter()
        .find(|f| matches!(f.kind, IncludeKind::Entry))
        .map(|f| f.file);
    let mut pipeline = make_pipeline(&st, entry_source, stage_list, jobs, workflow_rules, None);
    pipeline.default_branch = Some(default_branch.clone());
    pipeline.variables = crate::util::yaml_vars_map(classified.variables.as_ref());

    PipelineOutcome {
        pipeline,
        merged_root: Some(merged),
        include_files: st.include_files,
        include_edges: st.include_edges,
    }
}
