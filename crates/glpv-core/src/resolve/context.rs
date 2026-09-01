//! Shared state for one pipeline resolution.

use std::sync::Arc;

use glpv_yaml::FileId;

use crate::diff::{DiffOracle, DiffSpec};
use crate::model::{self, Diagnostic, PipelineId, Severity};
use crate::source::{ProjectSource, SourceMap, Sources, TreeRef};

#[derive(Clone, Debug)]
pub struct ResolveOpts {
    /// GitLab's `ci_max_includes` default.
    pub max_includes: u32,
    pub allow_remote: bool,
    pub embed_sources: bool,
    pub max_pipelines: u32,
    /// Record per-leaf-key winning spans on every job (large graphs get big).
    pub full_provenance: bool,
    /// The changed files `rules:changes` is evaluated against in every root
    /// project (`None`: clauses without `compare_to` stay undecided).
    pub diff: Option<DiffSpec>,
    /// `spec:inputs` values for the entry file of an entry scan (`--input`),
    /// taken as strings.
    pub root_inputs: indexmap::IndexMap<String, String>,
}

impl Default for ResolveOpts {
    fn default() -> Self {
        ResolveOpts {
            max_includes: 150,
            allow_remote: false,
            embed_sources: true,
            max_pipelines: 200,
            full_provenance: false,
            diff: None,
            root_inputs: indexmap::IndexMap::new(),
        }
    }
}

/// The include context: which project/tree the current file was fetched from.
/// Nested `include:local` and `rules:exists` resolve against this frame.
#[derive(Clone)]
pub struct Frame {
    pub project: Arc<dyn ProjectSource>,
    pub tree: TreeRef,
    pub file: FileId,
    pub file_path: String,
}

impl Frame {
    pub fn stack_key(&self) -> StackKey {
        let meta = self.project.meta();
        StackKey {
            host: meta.key.host.clone(),
            path_lc: meta.key.path_lc.clone(),
            tree: self.tree.clone(),
            file_path: self.file_path.clone(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StackKey {
    pub host: String,
    pub path_lc: String,
    pub tree: TreeRef,
    pub file_path: String,
}

/// Mutable state threaded through the resolution of one pipeline.
pub struct ResolveState<'a> {
    pub files: &'a mut SourceMap,
    pub diags: &'a mut Vec<Diagnostic>,
    pub opts: &'a ResolveOpts,
    pub sources: &'a Sources,
    pub pipeline_id: PipelineId,
    pub include_files: Vec<model::IncludeFile>,
    pub include_edges: Vec<model::IncludeEdge>,
    /// Files fetched so far for this pipeline (the 150 budget).
    pub budget_used: u32,
    pub stack: Vec<StackKey>,
    pub order_counter: u32,
    /// The pipeline's diff oracle, for `include:rules:changes` (always the
    /// root pipeline's diff, never the include frame's).
    pub diff: Option<Arc<DiffOracle>>,
    /// See `EvalContext::push_event`.
    pub push_event: bool,
    /// The entry file's `spec:inputs` (see `model::SpecInputMeta`).
    pub spec_inputs: Vec<model::SpecInputMeta>,
    /// The `spec:inputs` of the document `load_document` parsed last.
    pub last_spec_inputs: Vec<model::SpecInputMeta>,
    /// For a `trigger:include` child: the parent's file the include list
    /// came from. The files it includes directly are the child's entry
    /// documents, whose `spec:inputs` are the child's.
    pub child_entry_file: Option<FileId>,
}

impl ResolveState<'_> {
    pub fn diag(&mut self, severity: Severity, code: &str, message: impl Into<String>) {
        self.diag_at(severity, code, message, None);
    }

    pub fn diag_at(
        &mut self,
        severity: Severity,
        code: &str,
        message: impl Into<String>,
        span: Option<model::Span>,
    ) {
        self.diags.push(Diagnostic {
            severity,
            code: code.to_string(),
            message: message.into(),
            span,
            related: Vec::new(),
            hint: None,
            pipeline: Some(self.pipeline_id.clone()),
        });
    }

    pub fn diag_hint(
        &mut self,
        severity: Severity,
        code: &str,
        message: impl Into<String>,
        span: Option<model::Span>,
        hint: impl Into<String>,
    ) {
        self.diags.push(Diagnostic {
            severity,
            code: code.to_string(),
            message: message.into(),
            span,
            related: Vec::new(),
            hint: Some(hint.into()),
            pipeline: Some(self.pipeline_id.clone()),
        });
    }

    pub fn import_yaml_diags(&mut self, diags: Vec<glpv_yaml::YamlDiag>) {
        for d in diags {
            self.diags.push(Diagnostic {
                severity: d.severity.into(),
                code: d.code.to_string(),
                message: d.message,
                span: Some(d.span.into()),
                related: d.related.into_iter().map(Into::into).collect(),
                hint: None,
                pipeline: Some(self.pipeline_id.clone()),
            });
        }
    }
}
