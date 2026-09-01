//! High-level entry points: crawl from an entry pipeline through every
//! `trigger` into one graph. Child pipelines resolve their synthetic
//! `include:` document in the parent project; multi-project triggers locate
//! the downstream clone through the project index and recurse, with a
//! visited set for sharing and an ancestor stack for cycle marking.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use glpv_yaml::Node;
use indexmap::IndexMap;

use crate::diff::DiffOracle;
use crate::model::{
    self, Diagnostic, Graph, PipelineId, PipelineKind, ProjectRef, Severity, ToolInfo, TriggerKind,
    Unresolved, UnresolvedReason,
};
use crate::resolve::{Entry, PipelineRequest, ResolveOpts, resolve_pipeline};
use crate::rules::changes::has_push_event;
use crate::rules::{ChangesMatch, ChangesQuery};
use crate::source::local::LocalGitProject;
use crate::source::{ProjectKey, ProjectOrigin, ProjectSource, SourceMap, Sources, TreeRef};
use crate::vars::{Scenario, VarState, VarTable, predefined_vars};

#[derive(thiserror::Error, Debug)]
pub enum ScanError {
    #[error(transparent)]
    Source(#[from] crate::source::SourceError),
    #[error("{0}")]
    Other(String),
}

pub struct ScanOutput {
    pub graph: Graph,
    /// Merged root of the entry pipeline (for `glpv resolve`).
    pub merged_root: Option<Node>,
}

struct PipeCtx {
    project: Option<Arc<dyn ProjectSource>>,
    tree: Option<TreeRef>,
    scenario: Scenario,
    child_depth: u32,
    merged_root: Option<Node>,
    /// See `PipelineRequest::diff` / `diff_inherited` / `push_event`.
    diff: Option<Arc<DiffOracle>>,
    diff_inherited: bool,
    push_event: bool,
}

type VisitKey = (String, String, String, String); // host, path_lc, ref, config label

pub struct GraphBuilder<'a> {
    opts: &'a ResolveOpts,
    sources: &'a Sources,
    files: SourceMap,
    diags: Vec<Diagnostic>,
    pipelines: Vec<model::Pipeline>,
    ctxs: Vec<PipeCtx>,
    trigger_edges: Vec<model::TriggerEdge>,
    include_files: Vec<model::IncludeFile>,
    include_edges: Vec<model::IncludeEdge>,
    visited: HashMap<VisitKey, PipelineId>,
    limit_hit: bool,
}

impl<'a> GraphBuilder<'a> {
    fn new(opts: &'a ResolveOpts, sources: &'a Sources) -> Self {
        GraphBuilder {
            opts,
            sources,
            files: SourceMap::default(),
            diags: Vec::new(),
            pipelines: Vec::new(),
            ctxs: Vec::new(),
            trigger_edges: Vec::new(),
            include_files: Vec::new(),
            include_edges: Vec::new(),
            visited: HashMap::new(),
            limit_hit: false,
        }
    }

    fn add_pipeline(&mut self, req: PipelineRequest, child_depth: u32) -> usize {
        let scenario = req.scenario.clone();
        let project = req.project.clone();
        let tree = req.tree.clone();
        let diff = req.diff.clone();
        let diff_inherited = req.diff_inherited;
        let push_event = req.push_event;
        let outcome = resolve_pipeline(
            &mut self.files,
            &mut self.diags,
            self.opts,
            self.sources,
            req,
        );
        let id = outcome.pipeline.id.clone();
        self.include_files.extend(outcome.include_files);
        self.include_edges.extend(outcome.include_edges);
        self.pipelines.push(outcome.pipeline);
        self.ctxs.push(PipeCtx {
            project: Some(project),
            tree: Some(tree),
            scenario,
            child_depth,
            merged_root: outcome.merged_root,
            diff,
            diff_inherited,
            push_event,
        });
        if let Some(o) = self.ctxs.last().and_then(|c| c.diff.clone()) {
            self.drain_oracle(&o, &id);
        }
        self.pipelines.len() - 1
    }

    /// Move the diff oracle's queued diagnostics into the graph, stamped
    /// with the pipeline they surfaced in.
    fn drain_oracle(&mut self, oracle: &DiffOracle, id: &PipelineId) {
        for mut d in oracle.take_diags() {
            d.pipeline = Some(id.clone());
            self.diags.push(d);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_unresolved(
        &mut self,
        kind: PipelineKind,
        project: ProjectRef,
        git_ref: Option<String>,
        config_label: String,
        parent: (PipelineId, String),
        depth: u32,
        reason: UnresolvedReason,
        detail: String,
        hint: Option<String>,
    ) -> usize {
        let id = model::pipeline_id(
            &project,
            git_ref.as_deref(),
            &config_label,
            Some((&parent.0, parent.1.as_str())),
        );
        let severity = match reason {
            UnresolvedReason::DynamicChild => Severity::Warning,
            _ => Severity::Error,
        };
        self.diags.push(Diagnostic {
            severity,
            code: unresolved_trigger_code(reason).to_string(),
            message: detail.clone(),
            span: None,
            related: Vec::new(),
            hint,
            pipeline: Some(id.clone()),
        });
        self.pipelines.push(model::Pipeline {
            id: id.clone(),
            kind,
            project,
            git_ref,
            sha: None,
            config_path: config_label,
            default_branch: None,
            diff: None,
            variables: IndexMap::new(),
            entry_source: None,
            stages: Vec::new(),
            jobs: Vec::new(),
            workflow_rules: None,
            unresolved: Some(Unresolved {
                reason,
                detail,
                span: None,
            }),
            parent: Some(parent),
            depth,
            includes: Vec::new(),
        });
        self.ctxs.push(PipeCtx {
            project: None,
            tree: None,
            scenario: Scenario::push_default(),
            child_depth: 0,
            merged_root: None,
            diff: None,
            diff_inherited: false,
            push_event: false,
        });
        self.pipelines.len() - 1
    }

    fn visit_key(&self, idx: usize) -> VisitKey {
        let p = &self.pipelines[idx];
        (
            p.project.host.clone(),
            p.project.path_lc.clone(),
            p.git_ref.clone().unwrap_or_else(|| "worktree".to_string()),
            p.config_path.clone(),
        )
    }

    /// Job-scope variable table for expanding `trigger:project`/`branch`.
    fn trigger_vars(&self, idx: usize, job_base: &str) -> VarTable {
        let p = &self.pipelines[idx];
        let ctx = &self.ctxs[idx];
        let mut table = match &ctx.project {
            Some(project) => {
                let db = project
                    .default_branch()
                    .unwrap_or_else(|_| "main".to_string());
                predefined_vars(
                    project.meta(),
                    &db,
                    p.sha.as_deref(),
                    &p.config_path,
                    &ctx.scenario,
                )
            }
            None => VarTable::default(),
        };
        if let Some(root) = &ctx.merged_root {
            collect_yaml_vars(root.get("variables"), &mut table);
            if let Some(job) = root.get(job_base) {
                collect_yaml_vars(job.get("variables"), &mut table);
            }
        }
        // Two passes so values referencing other variables settle.
        for _ in 0..2 {
            let pairs: Vec<(String, String)> = table
                .iter()
                .filter_map(|(k, v)| match v {
                    VarState::Known(s) if s.contains('$') => Some((k.to_string(), s.clone())),
                    _ => None,
                })
                .collect();
            for (k, v) in pairs {
                if let Ok(expanded) = table.expand(&v) {
                    table.set_known(k, expanded);
                }
            }
        }
        for (k, v) in &ctx.scenario.vars {
            table.set_known(k.clone(), v.clone());
        }
        table
    }

    /// Variables forwarded into a downstream pipeline's scenario.
    fn forwarded_vars(
        &self,
        idx: usize,
        job_base: &str,
        forward: &model::Forward,
    ) -> IndexMap<String, String> {
        let mut out = IndexMap::new();
        if forward.yaml_variables
            && let Some(root) = &self.ctxs[idx].merged_root
        {
            let mut t = VarTable::default();
            collect_yaml_vars(root.get("variables"), &mut t);
            if let Some(job) = root.get(job_base) {
                collect_yaml_vars(job.get("variables"), &mut t);
            }
            // `rules:variables` of the bridge's matched clause are job
            // variables too, and travel with the YAML variables.
            if let Some(job) = self.pipelines[idx]
                .jobs
                .iter()
                .find(|j| j.base_name.as_deref().unwrap_or(&j.name) == job_base)
            {
                let vars = self.trigger_vars(idx, job_base);
                let eval = self.job_evaluation(idx, &job.rules, job.when, &vars);
                for (k, v) in &eval.variables {
                    t.set_known(k.clone(), v.clone());
                }
            }
            for (k, v) in t.iter() {
                if let VarState::Known(s) = v {
                    out.insert(k.to_string(), s.clone());
                }
            }
        }
        if forward.pipeline_variables {
            for (k, v) in &self.ctxs[idx].scenario.vars {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }

    /// Evaluate every job's rules under its pipeline's scenario, with
    /// workflow gating. Fills `job.evaluations` (one entry per job).
    /// Evaluate one rules chain in the context of pipeline `idx`: its
    /// project tree for `exists:`, its diff for `changes:`, its scenario.
    fn job_evaluation(
        &self,
        idx: usize,
        rules: &model::RulesSummary,
        job_when: model::When,
        vars: &VarTable,
    ) -> model::JobEvaluation {
        use crate::rules::{EvalContext, evaluate_rules};

        let pctx = &self.ctxs[idx];
        let p = &self.pipelines[idx];
        let scenario = &pctx.scenario;
        let default_branch = p
            .default_branch
            .clone()
            .or_else(|| {
                pctx.project
                    .as_ref()
                    .and_then(|pr| pr.default_branch().ok())
            })
            .unwrap_or_else(|| "main".to_string());
        let ref_name = p
            .git_ref
            .clone()
            .or_else(|| scenario.git_ref.clone())
            .unwrap_or(default_branch);
        let diff = pctx.diff.clone();
        let changes_checker =
            |q: &ChangesQuery<'_>| -> Option<ChangesMatch> { diff.as_ref()?.check(q) };
        let exists_checker = |patterns: &[String]| -> Option<bool> {
            let project = pctx.project.as_ref()?;
            let tree = pctx.tree.as_ref()?;
            let listing = project.list_tree(tree).ok()?;
            Some(patterns.iter().any(|pat| {
                let re = crate::glob::glob_to_regex(pat.trim_start_matches('/'));
                listing.iter().any(|f| re.is_match(f))
            }))
        };
        let ctx = EvalContext {
            vars,
            exists: Some(&exists_checker),
            changes: Some(&changes_checker),
            source: &scenario.source,
            ref_name: &ref_name,
            is_tag: scenario.is_tag,
            push_event: pctx.push_event,
        };
        evaluate_rules(rules, &ctx, &scenario.id, job_when)
    }

    fn evaluate_graph(&mut self) {
        for idx in 0..self.pipelines.len() {
            if self.ctxs[idx].project.is_none() || self.ctxs[idx].tree.is_none() {
                continue;
            }
            let diff = self.ctxs[idx].diff.clone();
            // Expanded `compare_to` refs, for the graph JSON.
            let mut compare_refs: Vec<String> = Vec::new();

            // Workflow gate first (pipeline-level variables only).
            let wf_vars = self.trigger_vars(idx, "");
            let wf_outcome = self.pipelines[idx].workflow_rules.as_ref().map(|wf| {
                collect_compare_refs(wf, &wf_vars, &mut compare_refs);
                self.job_evaluation(idx, wf, model::When::OnSuccess, &wf_vars)
                    .outcome
            });

            let mut evals = Vec::with_capacity(self.pipelines[idx].jobs.len());
            let mut by_base: HashMap<String, model::JobEvaluation> = HashMap::new();
            for j in &self.pipelines[idx].jobs {
                let base = j.base_name.as_deref().unwrap_or(&j.name).to_string();
                if let Some(prev) = by_base.get(&base) {
                    // Sibling expansion: identical rules, identical outcome.
                    evals.push(prev.clone());
                    continue;
                }
                let vars = self.trigger_vars(idx, &base);
                collect_compare_refs(&j.rules, &vars, &mut compare_refs);
                let mut eval = self.job_evaluation(idx, &j.rules, j.when, &vars);
                match wf_outcome {
                    Some(model::Outcome::Skipped) => {
                        eval.outcome = model::Outcome::Blocked;
                        eval.blocked_by = Some("workflow:rules".to_string());
                    }
                    Some(model::Outcome::Unknown)
                        if !matches!(eval.outcome, model::Outcome::Skipped) =>
                    {
                        eval.outcome = model::Outcome::Unknown;
                        eval.blocked_by = Some("workflow:rules undecided".to_string());
                    }
                    _ => {}
                }
                by_base.insert(base, eval.clone());
                evals.push(eval);
            }
            // Store the full evaluation only on the first expansion; siblings
            // carry the outcome without the (identical) trace.
            let mut seen: std::collections::HashSet<String> = Default::default();
            for (j, mut eval) in self.pipelines[idx].jobs.iter_mut().zip(evals) {
                let base = j.base_name.as_deref().unwrap_or(&j.name).to_string();
                if !seen.insert(base) {
                    eval.trace = Vec::new();
                }
                j.evaluations.push(eval);
            }
            if let Some(o) = &diff {
                let own = !self.ctxs[idx].diff_inherited;
                self.pipelines[idx].diff = o.to_model(own, &compare_refs);
                let id = self.pipelines[idx].id.clone();
                self.drain_oracle(o, &id);
            }
        }
    }

    /// After an all-projects crawl, a project scanned as a root may turn out
    /// to be triggered by another pipeline: re-classify it as downstream and
    /// recompute every depth from the parent chains so the graph reads in
    /// execution order.
    fn reclassify_triggered_roots(&mut self) {
        let index_of: HashMap<PipelineId, usize> = self
            .pipelines
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id.clone(), i))
            .collect();
        let src_of_job = |pipelines: &[model::Pipeline], job: &model::JobId| {
            pipelines
                .iter()
                .position(|p| job.0.starts_with(&format!("{}/", p.id.0)))
        };

        // Pass 1: give triggered ex-roots a parent, guarding against loops
        // (A triggers B and B triggers A must not become each other's parent).
        for e in &self.trigger_edges {
            let Some(src) = src_of_job(&self.pipelines, &e.from_job) else {
                continue;
            };
            let Some(&dst) = index_of.get(&e.to_pipeline) else {
                continue;
            };
            if dst == src {
                continue;
            }
            let dst_p = &self.pipelines[dst];
            if dst_p.parent.is_some() || !matches!(dst_p.kind, PipelineKind::Root) {
                continue;
            }
            // Walk src's ancestor chain; if it passes through dst, adopting
            // would create a parent loop.
            let mut cursor = Some(src);
            let mut loops = false;
            let mut guard = 0;
            while let Some(i) = cursor {
                if i == dst {
                    loops = true;
                    break;
                }
                guard += 1;
                if guard > self.pipelines.len() {
                    break;
                }
                cursor = self.pipelines[i]
                    .parent
                    .as_ref()
                    .and_then(|(pid, _)| index_of.get(pid).copied());
            }
            if loops {
                continue;
            }
            let job = e.from_job.0.rsplit('/').next().unwrap_or("").to_string();
            let parent_id = self.pipelines[src].id.clone();
            let p = &mut self.pipelines[dst];
            p.kind = PipelineKind::MultiProject;
            p.parent = Some((parent_id, job));
            // Downstream of a trigger there is no push event: plain
            // `changes:` clauses always match; `compare_to` still diffs.
            self.ctxs[dst].push_event = false;
            if let (Some(project), Some(tree)) =
                (self.ctxs[dst].project.clone(), self.ctxs[dst].tree.clone())
            {
                self.ctxs[dst].diff = Some(DiffOracle::new(project, tree, None));
            }
        }

        // Pass 2: depths follow the parent chains.
        for _ in 0..self.pipelines.len() {
            let mut changed = false;
            for i in 0..self.pipelines.len() {
                let Some((pid, _)) = self.pipelines[i].parent.clone() else {
                    continue;
                };
                let Some(&pi) = index_of.get(&pid) else {
                    continue;
                };
                let want = self.pipelines[pi].depth + 1;
                if self.pipelines[i].depth != want {
                    self.pipelines[i].depth = want;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Sweep every project tree for `*.yml`/`*.yaml` files that no scanned
    /// pipeline consumed but that look like CI configuration, and crawl each
    /// as a first-class "detached" pipeline.
    fn discover_detached(&mut self, projects: &[Arc<dyn ProjectSource>], scenario: &Scenario) {
        let mut consumed: std::collections::HashSet<(String, String, String)> =
            std::collections::HashSet::new();
        let mut cursor = 0;
        let refresh = |consumed: &mut std::collections::HashSet<(String, String, String)>,
                       files: &SourceMap,
                       cursor: &mut usize| {
            for i in *cursor..files.len() {
                let o = files.origin(glpv_yaml::FileId(i as u32));
                if let Some(p) = &o.project {
                    consumed.insert((p.host.clone(), p.path_lc.clone(), o.path.clone()));
                }
            }
            *cursor = files.len();
        };
        refresh(&mut consumed, &self.files, &mut cursor);

        for project in projects {
            let meta = project.meta().clone();
            let Ok(branch) = project.default_branch() else {
                continue;
            };
            let Ok(Some(sha)) = project.resolve_ref(&branch) else {
                continue;
            };
            let tree = TreeRef::Commit(sha);
            let Ok(listing) = project.list_tree(&tree) else {
                continue;
            };
            let mut candidates: Vec<String> = listing
                .iter()
                .filter(|f| f.ends_with(".yml") || f.ends_with(".yaml"))
                .cloned()
                .collect();
            candidates.sort();
            for path in candidates {
                let key = (
                    meta.key.host.clone(),
                    meta.key.path_lc.clone(),
                    path.clone(),
                );
                if consumed.contains(&key) {
                    continue;
                }
                let Ok(Some(text)) = project.read(&tree, &path) else {
                    continue;
                };
                if !looks_like_ci(&text) {
                    continue;
                }
                let req = PipelineRequest {
                    project: project.clone(),
                    tree: tree.clone(),
                    git_ref: Some(branch.clone()),
                    entry: Entry::ConfigPath {
                        path: path.clone(),
                        config_source: None,
                    },
                    scenario: scenario.clone(),
                    kind: PipelineKind::Detached,
                    depth: 0,
                    parent: None,
                    inputs: IndexMap::new(),
                    diff: Some(DiffOracle::new(
                        project.clone(),
                        tree.clone(),
                        self.opts.diff.as_ref(),
                    )),
                    diff_inherited: false,
                    push_event: has_push_event(&scenario.source, scenario.is_tag),
                };
                let idx = self.add_pipeline(req, 0);
                let id = self.pipelines[idx].id.clone();
                self.diags.push(Diagnostic {
                    severity: Severity::Info,
                    code: "discover.detached".to_string(),
                    message: format!(
                        "{}/{path} looks like CI configuration but is not referenced by any \
                         scanned pipeline (maybe used via ci_config_path, a scheduled pipeline, \
                         or unwired)",
                        meta.display_path
                    ),
                    span: None,
                    related: Vec::new(),
                    hint: None,
                    pipeline: Some(id),
                });
                let mut ancestors = Vec::new();
                self.walk(idx, &mut ancestors);
                refresh(&mut consumed, &self.files, &mut cursor);
            }
        }
    }

    fn walk(&mut self, idx: usize, ancestors: &mut Vec<PipelineId>) {
        let pid = self.pipelines[idx].id.clone();
        ancestors.push(pid.clone());

        // Snapshot the trigger jobs (base-name deduplicated for parallel bridges).
        let mut seen = Vec::new();
        let mut triggers = Vec::new();
        for j in &self.pipelines[idx].jobs {
            let Some(t) = &j.trigger else { continue };
            let base = j.base_name.clone().unwrap_or_else(|| j.name.clone());
            if seen.contains(&base) {
                continue;
            }
            seen.push(base.clone());
            triggers.push((
                j.id.clone(),
                base,
                t.kind.clone(),
                t.strategy.clone(),
                t.forward.clone(),
                j.provenance.defined_at,
            ));
        }

        for (job_id, base, kind, strategy, forward, span) in triggers {
            if self.limit_hit {
                break;
            }
            if self.pipelines.len() as u32 >= self.opts.max_pipelines {
                self.limit_hit = true;
                self.diags.push(Diagnostic {
                    severity: Severity::Error,
                    code: "trigger.pipeline-limit".to_string(),
                    message: format!(
                        "stopping the crawl at {} pipelines (--max-pipelines)",
                        self.pipelines.len()
                    ),
                    span: None,
                    related: Vec::new(),
                    hint: None,
                    pipeline: Some(pid.clone()),
                });
                break;
            }

            let (target, cycle, spawned) = match kind {
                TriggerKind::DynamicChild { artifact, job } => {
                    let p = self.pipelines[idx].project.clone();
                    let gref = self.pipelines[idx].git_ref.clone();
                    let depth = self.pipelines[idx].depth + 1;
                    let t = self.add_unresolved(
                        PipelineKind::DynamicChild,
                        p,
                        gref,
                        format!("<generated by {job}: {artifact}>"),
                        (pid.clone(), base.clone()),
                        depth,
                        UnresolvedReason::DynamicChild,
                        format!(
                            "child pipeline config is generated at runtime by `{job}` (artifact `{artifact}`)"
                        ),
                        None,
                    );
                    (Some(self.pipelines[t].id.clone()), false, None)
                }
                TriggerKind::Child { .. } => self.spawn_child(idx, &pid, &base, &forward),
                TriggerKind::MultiProject {
                    ref project,
                    ref branch,
                    ..
                } => self.spawn_multi_project(
                    idx,
                    &pid,
                    &base,
                    project,
                    branch.as_deref(),
                    &forward,
                    ancestors,
                ),
            };

            if let Some(target_id) = &target {
                self.trigger_edges.push(model::TriggerEdge {
                    from_job: job_id.clone(),
                    to_pipeline: target_id.clone(),
                    cycle,
                    strategy: strategy.clone(),
                    forward: forward.clone(),
                    span,
                });
                // Fill trigger.target on every expansion of this bridge.
                for j in self.pipelines[idx].jobs.iter_mut() {
                    let jbase = j.base_name.as_deref().unwrap_or(&j.name);
                    if jbase == base
                        && let Some(t) = &mut j.trigger
                    {
                        t.target = Some(target_id.clone());
                    }
                }
            }
            if let Some(new_idx) = spawned {
                self.walk(new_idx, ancestors);
            }
        }
        ancestors.pop();
    }

    fn spawn_child(
        &mut self,
        idx: usize,
        pid: &PipelineId,
        base: &str,
        forward: &model::Forward,
    ) -> (Option<PipelineId>, bool, Option<usize>) {
        let depth = self.pipelines[idx].depth + 1;
        let child_depth = self.ctxs[idx].child_depth + 1;
        let parent_project_ref = self.pipelines[idx].project.clone();
        let parent_ref = self.pipelines[idx].git_ref.clone();

        if child_depth > 2 {
            let t = self.add_unresolved(
                PipelineKind::Unresolved,
                parent_project_ref,
                parent_ref,
                format!("<child of {base}>"),
                (pid.clone(), base.to_string()),
                depth,
                UnresolvedReason::ChildDepthExceeded,
                format!(
                    "`{base}` would create a child pipeline {child_depth} levels deep; \
                     GitLab allows at most 2 (parent → child → grandchild)"
                ),
                None,
            );
            return (Some(self.pipelines[t].id.clone()), false, None);
        }

        let (Some(project), Some(tree)) =
            (self.ctxs[idx].project.clone(), self.ctxs[idx].tree.clone())
        else {
            return (None, false, None);
        };
        let Some(root) = &self.ctxs[idx].merged_root else {
            return (None, false, None);
        };
        let Some(include_node) = root
            .get(base)
            .and_then(|j| j.get("trigger"))
            .and_then(|t| t.get("include"))
            .cloned()
        else {
            return (None, false, None);
        };
        let inputs = root
            .get(base)
            .and_then(|j| j.get("trigger"))
            .and_then(|t| t.get("inputs"))
            .and_then(|i| i.as_map())
            .map(|m| {
                m.iter()
                    .map(|(k, e)| (k.to_string(), e.value.clone()))
                    .collect::<IndexMap<String, Node>>()
            })
            .unwrap_or_default();

        let from_file = include_node.span.file;
        let from_path = self.files.origin(from_file).path.clone();
        let scenario = Scenario {
            id: self.ctxs[idx].scenario.id.clone(),
            source: "parent_pipeline".to_string(),
            git_ref: parent_ref.clone(),
            is_tag: self.ctxs[idx].scenario.is_tag,
            vars: self.forwarded_vars(idx, base, forward),
        };
        let req = PipelineRequest {
            project,
            tree,
            git_ref: parent_ref,
            entry: Entry::ChildIncludes {
                include_node,
                from_file,
                from_path,
                label: format!("trigger:include via {base}"),
            },
            scenario,
            kind: PipelineKind::Child,
            depth,
            parent: Some((pid.clone(), base.to_string())),
            inputs,
            // Child pipelines inherit the parent's diff and push event.
            diff: self.ctxs[idx].diff.clone(),
            diff_inherited: true,
            push_event: self.ctxs[idx].push_event,
        };
        let new_idx = self.add_pipeline(req, child_depth);
        (
            Some(self.pipelines[new_idx].id.clone()),
            false,
            Some(new_idx),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_multi_project(
        &mut self,
        idx: usize,
        pid: &PipelineId,
        base: &str,
        project_text: &str,
        branch_text: Option<&str>,
        forward: &model::Forward,
        ancestors: &[PipelineId],
    ) -> (Option<PipelineId>, bool, Option<usize>) {
        let depth = self.pipelines[idx].depth + 1;
        let host = self.pipelines[idx].project.host.clone();
        let vars = self.trigger_vars(idx, base);

        let unresolved = |builder: &mut Self,
                          project_ref: ProjectRef,
                          git_ref: Option<String>,
                          reason,
                          detail: String,
                          hint: Option<String>| {
            let t = builder.add_unresolved(
                PipelineKind::Unresolved,
                project_ref,
                git_ref,
                String::new(),
                (pid.clone(), base.to_string()),
                depth,
                reason,
                detail,
                hint,
            );
            (Some(builder.pipelines[t].id.clone()), false, None)
        };

        let project_path = match vars.expand(project_text) {
            Ok(p) => p.trim_matches('/').to_string(),
            Err(missing) => {
                return unresolved(
                    self,
                    ProjectRef::new(host.clone(), project_text.to_string()),
                    None,
                    UnresolvedReason::VariableInLocation,
                    format!(
                        "cannot expand ${} in trigger project `{project_text}` of `{base}`",
                        missing.join(", $")
                    ),
                    Some("pass --var NAME=value to supply it".to_string()),
                );
            }
        };

        let key = ProjectKey::new(&host, &project_path);
        let target = match self.sources.locate(&key) {
            Ok(Some(t)) => t,
            outcome => {
                let detail = match outcome {
                    Err(e) => {
                        format!("no clone of {host}/{project_path} in the project index; {e}")
                    }
                    _ => format!("no clone of {host}/{project_path} in the project index"),
                };
                return unresolved(
                    self,
                    ProjectRef::new(host.clone(), project_path.clone()),
                    None,
                    UnresolvedReason::ProjectNotFound,
                    detail,
                    Some(format!(
                        "clone it into the projects folder: `git clone git@{host}:{project_path}.git`"
                    )),
                );
            }
        };
        let target_ref_name = match branch_text {
            Some(b) => match vars.expand(b) {
                Ok(b) => b,
                Err(missing) => {
                    return unresolved(
                        self,
                        target.meta().project_ref(),
                        Some(b.to_string()),
                        UnresolvedReason::VariableInLocation,
                        format!(
                            "cannot expand ${} in trigger branch `{b}` of `{base}`",
                            missing.join(", $")
                        ),
                        Some("pass --var NAME=value to supply it".to_string()),
                    );
                }
            },
            None => match target.default_branch() {
                Ok(b) => b,
                Err(e) => {
                    return unresolved(
                        self,
                        target.meta().project_ref(),
                        None,
                        UnresolvedReason::RefNotFound,
                        e.to_string(),
                        None,
                    );
                }
            },
        };
        let sha = match target.resolve_ref(&target_ref_name) {
            Ok(Some(s)) => s,
            _ => {
                let detail = match target.meta().origin {
                    ProjectOrigin::LocalClone(_) => format!(
                        "ref `{target_ref_name}` not found in the clone of {} (fetch it?)",
                        target.meta().display_path
                    ),
                    ProjectOrigin::Api { .. } => format!(
                        "ref `{target_ref_name}` not found in {} through the API",
                        target.meta().display_path
                    ),
                };
                return unresolved(
                    self,
                    target.meta().project_ref(),
                    Some(target_ref_name.clone()),
                    UnresolvedReason::RefNotFound,
                    detail,
                    None,
                );
            }
        };

        // The downstream project runs its own entry config (`ci_config_path`).
        let config_setting = target
            .meta()
            .ci_config_path
            .clone()
            .unwrap_or_else(|| ".gitlab-ci.yml".to_string());
        let (config_path, config_source) = match parse_config_path(&config_setting) {
            ConfigPathSpec::Local(p) => (p, None),
            ConfigPathSpec::Url(u) => {
                return unresolved(
                    self,
                    target.meta().project_ref(),
                    Some(target_ref_name),
                    UnresolvedReason::RemoteDisabled,
                    format!("ci_config_path of {project_path} is a remote URL ({u})"),
                    None,
                );
            }
            ConfigPathSpec::OtherProject {
                path,
                project,
                git_ref,
            } => {
                let ckey = ProjectKey::new(&host, &project);
                match self.sources.locate(&ckey) {
                    Ok(Some(cp)) => {
                        let _ = git_ref; // the config host's default branch is used
                        (path, Some(cp))
                    }
                    outcome => {
                        let detail = match outcome {
                            Err(e) => format!(
                                "ci_config_path of {project_path} lives in {host}/{project}, which is not in the index; {e}"
                            ),
                            _ => format!(
                                "ci_config_path of {project_path} lives in {host}/{project}, which is not in the index"
                            ),
                        };
                        return unresolved(
                            self,
                            target.meta().project_ref(),
                            Some(target_ref_name),
                            UnresolvedReason::ProjectNotFound,
                            detail,
                            Some(format!("clone it: `git clone git@{host}:{project}.git`")),
                        );
                    }
                }
            }
        };

        // Shared target? (diamonds share; ancestors mark cycles)
        let vkey: VisitKey = (
            key.host.clone(),
            key.path_lc.clone(),
            target_ref_name.clone(),
            config_path.clone(),
        );
        if let Some(existing) = self.visited.get(&vkey) {
            let cycle = ancestors.contains(existing);
            return (Some(existing.clone()), cycle, None);
        }

        let scenario = Scenario {
            id: self.ctxs[idx].scenario.id.clone(),
            source: "pipeline".to_string(),
            git_ref: Some(target_ref_name.clone()),
            is_tag: false,
            vars: self.forwarded_vars(idx, base, forward),
        };
        // A downstream pipeline has no push event; its oracle serves
        // `compare_to` only.
        let diff = DiffOracle::new(target.clone(), TreeRef::Commit(sha.clone()), None);
        let req = PipelineRequest {
            project: target,
            tree: TreeRef::Commit(sha),
            git_ref: Some(target_ref_name),
            entry: Entry::ConfigPath {
                path: config_path,
                config_source,
            },
            scenario,
            kind: PipelineKind::MultiProject,
            depth,
            parent: Some((pid.clone(), base.to_string())),
            inputs: IndexMap::new(),
            diff: Some(diff),
            diff_inherited: false,
            push_event: false,
        };
        let new_idx = self.add_pipeline(req, 0);
        let new_id = self.pipelines[new_idx].id.clone();
        self.visited.insert(vkey, new_id.clone());

        // Record the resolved coordinates on the bridge job's trigger.
        let resolved_project = self.pipelines[new_idx].project.clone();
        let resolved_branch = self.pipelines[new_idx].git_ref.clone();
        for j in self.pipelines[idx].jobs.iter_mut() {
            let jbase = j.base_name.as_deref().unwrap_or(&j.name);
            if jbase == base
                && let Some(t) = &mut j.trigger
                && let TriggerKind::MultiProject {
                    project_resolved,
                    branch_resolved,
                    ..
                } = &mut t.kind
            {
                *project_resolved = Some(resolved_project.clone());
                *branch_resolved = resolved_branch.clone();
            }
        }
        (Some(new_id), false, Some(new_idx))
    }
}

enum ConfigPathSpec {
    Local(String),
    OtherProject {
        path: String,
        project: String,
        git_ref: Option<String>,
    },
    Url(String),
}

fn parse_config_path(s: &str) -> ConfigPathSpec {
    if s.starts_with("http://") || s.starts_with("https://") {
        return ConfigPathSpec::Url(s.to_string());
    }
    match s.split_once('@') {
        Some((path, rest)) => match rest.split_once(':') {
            Some((project, git_ref)) => ConfigPathSpec::OtherProject {
                path: path.to_string(),
                project: project.to_string(),
                git_ref: Some(git_ref.to_string()),
            },
            None => ConfigPathSpec::OtherProject {
                path: path.to_string(),
                project: rest.to_string(),
                git_ref: None,
            },
        },
        None => ConfigPathSpec::Local(s.trim_start_matches('/').to_string()),
    }
}

/// Expanded `changes:compare_to` refs of a rules chain, first-seen order.
fn collect_compare_refs(rules: &model::RulesSummary, vars: &VarTable, out: &mut Vec<String>) {
    for c in &rules.rules {
        if let Some(r) = &c.compare_to
            && let Ok(r) = vars.expand_existing(r)
            && !out.contains(&r)
        {
            out.push(r);
        }
    }
}

fn collect_yaml_vars(node: Option<&Node>, table: &mut VarTable) {
    let Some(map) = node.and_then(|n| n.untag().as_map()) else {
        return;
    };
    for (k, e) in map.iter() {
        let value = match &e.value.untag().kind {
            glpv_yaml::Kind::Map(m) => m.get("value").and_then(|v| v.scalar_text()),
            _ => e.value.scalar_text(),
        };
        if let Some(v) = value {
            table.set_known(k.to_string(), v);
        }
    }
}

fn unresolved_trigger_code(reason: UnresolvedReason) -> &'static str {
    match reason {
        UnresolvedReason::VariableInLocation => "trigger.variable-unresolved",
        UnresolvedReason::ProjectNotFound => "trigger.project-not-found",
        UnresolvedReason::RefNotFound => "trigger.ref-not-found",
        UnresolvedReason::RemoteDisabled => "trigger.remote-config",
        UnresolvedReason::ChildDepthExceeded => "trigger.child-depth",
        UnresolvedReason::DynamicChild => "trigger.dynamic-child",
        _ => "trigger.unresolved",
    }
}

/// Crawl starting from an already-located entry project.
#[allow(clippy::too_many_arguments)]
pub fn scan_entry(
    sources: &Sources,
    entry_project: Arc<dyn ProjectSource>,
    tree: TreeRef,
    git_ref: Option<String>,
    config_path: Option<String>,
    scenario: &Scenario,
    opts: &ResolveOpts,
    tool_args: Vec<String>,
    extra_diags: Vec<Diagnostic>,
) -> ScanOutput {
    let config_path = config_path
        .or_else(|| entry_project.meta().ci_config_path.clone())
        .unwrap_or_else(|| ".gitlab-ci.yml".to_string());

    let mut builder = GraphBuilder::new(opts, sources);
    let diff = DiffOracle::new(entry_project.clone(), tree.clone(), opts.diff.as_ref());
    let req = PipelineRequest {
        project: entry_project,
        tree,
        git_ref,
        entry: Entry::ConfigPath {
            path: config_path,
            config_source: None,
        },
        scenario: scenario.clone(),
        kind: PipelineKind::Root,
        depth: 0,
        parent: None,
        inputs: IndexMap::new(),
        diff: Some(diff),
        diff_inherited: false,
        push_event: has_push_event(&scenario.source, scenario.is_tag),
    };
    let root_idx = builder.add_pipeline(req, 0);
    let root_key = builder.visit_key(root_idx);
    let root_id = builder.pipelines[root_idx].id.clone();
    builder.visited.insert(root_key, root_id);

    let mut ancestors = Vec::new();
    builder.walk(root_idx, &mut ancestors);

    let merged_root = builder.ctxs[root_idx].merged_root.clone();
    let graph = finish(builder, scenario, opts, tool_args, extra_diags);
    ScanOutput { graph, merged_root }
}

fn finish(
    mut builder: GraphBuilder<'_>,
    scenario: &Scenario,
    opts: &ResolveOpts,
    tool_args: Vec<String>,
    mut extra_diags: Vec<Diagnostic>,
) -> Graph {
    builder.evaluate_graph();
    extra_diags.append(&mut builder.diags);
    Graph {
        schema_version: model::SCHEMA_VERSION,
        generated_at: now_rfc3339(),
        tool: ToolInfo {
            name: "glpv".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            args: tool_args,
        },
        scenarios: vec![model::ScenarioInfo {
            id: scenario.id.clone(),
            source: scenario.source.clone(),
            git_ref: scenario.git_ref.clone(),
            vars: scenario.vars.clone(),
        }],
        pipelines: builder.pipelines,
        trigger_edges: builder.trigger_edges,
        include_files: builder.include_files,
        include_edges: builder.include_edges,
        diagnostics: extra_diags,
        sources: builder.files.to_model(opts.embed_sources),
    }
}

/// Crawl every indexed project as a root, then sweep for CI-looking YAML
/// files nothing referenced ("detached" pipelines). Projects that turn out
/// to be triggered by another project are re-classified as downstream so the
/// graph reads in execution order.
pub fn scan_all(
    sources: &Sources,
    projects: Vec<Arc<dyn ProjectSource>>,
    scenario: &Scenario,
    opts: &ResolveOpts,
    tool_args: Vec<String>,
    extra_diags: Vec<Diagnostic>,
) -> ScanOutput {
    let mut builder = GraphBuilder::new(opts, sources);

    for project in &projects {
        let Ok(branch) = project.default_branch() else {
            continue;
        };
        let Ok(Some(sha)) = project.resolve_ref(&branch) else {
            continue;
        };
        let tree = TreeRef::Commit(sha);
        let config_path = project
            .meta()
            .ci_config_path
            .clone()
            .unwrap_or_else(|| ".gitlab-ci.yml".to_string());
        // A project without a CI config (e.g. a pure template library) is not
        // a pipeline root.
        if !matches!(project.read(&tree, &config_path), Ok(Some(_))) {
            continue;
        }
        let key: VisitKey = (
            project.meta().key.host.clone(),
            project.meta().key.path_lc.clone(),
            branch.clone(),
            config_path.clone(),
        );
        if builder.visited.contains_key(&key) {
            continue; // already crawled as someone's downstream
        }
        let diff = DiffOracle::new(project.clone(), tree.clone(), opts.diff.as_ref());
        let req = PipelineRequest {
            project: project.clone(),
            tree,
            git_ref: Some(branch),
            entry: Entry::ConfigPath {
                path: config_path,
                config_source: None,
            },
            scenario: scenario.clone(),
            kind: PipelineKind::Root,
            depth: 0,
            parent: None,
            inputs: IndexMap::new(),
            diff: Some(diff),
            diff_inherited: false,
            push_event: has_push_event(&scenario.source, scenario.is_tag),
        };
        let idx = builder.add_pipeline(req, 0);
        let id = builder.pipelines[idx].id.clone();
        builder.visited.insert(key, id);
        let mut ancestors = Vec::new();
        builder.walk(idx, &mut ancestors);
    }

    builder.reclassify_triggered_roots();
    builder.discover_detached(&projects, scenario);

    let graph = finish(builder, scenario, opts, tool_args, extra_diags);
    ScanOutput {
        graph,
        merged_root: None,
    }
}

/// Scan starting from a single `.gitlab-ci.yml` on disk. The containing git
/// repository provides the project identity; `git_ref: None` reads the
/// working tree (uncommitted changes included).
pub fn scan_file(
    file: &Path,
    git_ref: Option<&str>,
    scenario: &Scenario,
    opts: &ResolveOpts,
    sources: &Sources,
    tool_args: Vec<String>,
) -> Result<ScanOutput, ScanError> {
    let file = file
        .canonicalize()
        .map_err(|e| ScanError::Other(format!("{}: {e}", file.display())))?;
    let dir = file.parent().unwrap_or(Path::new("."));
    let project = Arc::new(LocalGitProject::open(dir)?);
    let config_path = file
        .strip_prefix(project.root())
        .map_err(|_| ScanError::Other("config file must live inside its repository".into()))?
        .to_string_lossy()
        .replace('\\', "/");

    let (tree, ref_name) = match git_ref {
        None => (TreeRef::Worktree, None),
        Some(r) => match project.resolve_ref(r)? {
            Some(sha) => (TreeRef::Commit(sha), Some(r.to_string())),
            None => {
                return Err(ScanError::Other(format!(
                    "ref `{r}` not found in {}",
                    project.meta().display_path
                )));
            }
        },
    };

    Ok(scan_entry(
        sources,
        project,
        tree,
        ref_name,
        Some(config_path),
        scenario,
        opts,
        tool_args,
        Vec::new(),
    ))
}

/// Heuristic: does this YAML look like a GitLab CI configuration?
/// Positive markers: top-level `stages`/`include`/`workflow`, or a visible
/// job-shaped entry (a mapping with script/run/trigger/extends/stage).
/// Kubernetes-style manifests (`apiVersion`) are excluded.
pub fn looks_like_ci(text: &str) -> bool {
    if text.len() > 1_000_000 {
        return false;
    }
    let Ok((docs, _)) = glpv_yaml::parse(glpv_yaml::FileId(u32::MAX), text) else {
        return false;
    };
    let Some(root) = docs.iter().rev().find_map(|d| d.root.as_ref()) else {
        return false;
    };
    let Some(map) = root.as_map() else {
        return false;
    };
    if map.contains_key("apiVersion") {
        return false;
    }
    if ["stages", "include", "workflow"]
        .iter()
        .any(|k| map.contains_key(k))
    {
        return true;
    }
    let reserved = crate::resolve::classify::RESERVED;
    for (k, e) in map.iter() {
        if k.starts_with('.') || reserved.contains(&k) {
            continue;
        }
        if let Some(m) = e.value.untag().as_map()
            && ["script", "run", "trigger", "extends", "stage"]
                .iter()
                .any(|kk| m.contains_key(kk))
        {
            return true;
        }
    }
    false
}

/// RFC 3339 UTC timestamp without pulling in a date-time dependency.
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let (h, m, s) = ((secs % 86_400) / 3600, (secs % 3600) / 60, secs % 60);
    // Civil-date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    #[test]
    fn timestamp_shape() {
        let t = super::now_rfc3339();
        assert_eq!(t.len(), 20, "{t}");
        assert!(t.starts_with("20"), "{t}");
        assert!(t.ends_with('Z'));
    }

    #[test]
    fn config_path_forms() {
        use super::{ConfigPathSpec, parse_config_path};
        assert!(
            matches!(parse_config_path(".gitlab-ci.yml"), ConfigPathSpec::Local(p) if p == ".gitlab-ci.yml")
        );
        assert!(
            matches!(parse_config_path("/ci/x.yml"), ConfigPathSpec::Local(p) if p == "ci/x.yml")
        );
        match parse_config_path("ci/pipeline.yml@group/other:stable") {
            ConfigPathSpec::OtherProject {
                path,
                project,
                git_ref,
            } => {
                assert_eq!(path, "ci/pipeline.yml");
                assert_eq!(project, "group/other");
                assert_eq!(git_ref.as_deref(), Some("stable"));
            }
            _ => panic!(),
        }
        assert!(matches!(
            parse_config_path("https://x.example/ci.yml"),
            ConfigPathSpec::Url(_)
        ));
    }
}
