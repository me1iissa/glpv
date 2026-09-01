//! The pipeline graph data model — also the canonical JSON (`schema_version` 1).
//!
//! Rule: nothing unresolvable is dropped. Whatever cannot be followed becomes a
//! node/edge with `unresolved` set plus a diagnostic, so the graph always shows
//! where analysis stopped and why.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

/// JSON form of a source span: `{file, start: [line, col], end: [line, col]}`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Span {
    pub file: u32,
    pub start: [u32; 2],
    pub end: [u32; 2],
}

impl From<glpv_yaml::Span> for Span {
    fn from(s: glpv_yaml::Span) -> Self {
        Span {
            file: s.file.0,
            start: [s.start.line, s.start.col],
            end: [s.end.line, s.end.col],
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct PipelineId(pub String);

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct JobId(pub String);

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ProjectRef {
    pub host: String,
    /// Display-cased project path, e.g. `acme/api`.
    pub path: String,
    /// Lower-cased path used for matching.
    pub path_lc: String,
}

impl ProjectRef {
    pub fn new(host: impl Into<String>, path: impl Into<String>) -> Self {
        let host = host.into().to_lowercase();
        let path = path.into();
        let path_lc = path.to_lowercase();
        ProjectRef {
            host,
            path,
            path_lc,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Graph {
    pub schema_version: u32,
    pub generated_at: String,
    pub tool: ToolInfo,
    pub scenarios: Vec<ScenarioInfo>,
    pub pipelines: Vec<Pipeline>,
    pub trigger_edges: Vec<TriggerEdge>,
    pub include_files: Vec<IncludeFile>,
    pub include_edges: Vec<IncludeEdge>,
    pub diagnostics: Vec<Diagnostic>,
    pub sources: Vec<SourceFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub version: String,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScenarioInfo {
    pub id: String,
    pub source: String,
    pub git_ref: Option<String>,
    pub vars: IndexMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceFile {
    pub file: u32,
    pub project: Option<ProjectRef>,
    pub sha: Option<String>,
    pub path: String,
    /// Full text, unless the scan ran with `--no-embed-sources`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineKind {
    Root,
    MultiProject,
    Child,
    DynamicChild,
    Unresolved,
    /// Found by the discovery sweep: a CI-looking config file no scanned
    /// pipeline references (used via ci_config_path, schedules, or unwired).
    Detached,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pipeline {
    pub id: PipelineId,
    pub kind: PipelineKind,
    pub project: ProjectRef,
    pub git_ref: Option<String>,
    pub sha: Option<String>,
    pub config_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    /// The changed-file lists `rules:changes` was evaluated against (only
    /// when a diff was supplied or a `compare_to` ref resolved).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<Diff>,
    #[serde(skip_serializing_if = "IndexMap::is_empty", default)]
    pub variables: IndexMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_source: Option<u32>,
    pub stages: Vec<String>,
    pub jobs: Vec<Job>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_rules: Option<RulesSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<Unresolved>,
    /// `(parent pipeline, trigger job name)` for downstream pipelines.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<(PipelineId, String)>,
    pub depth: u32,
    /// Files merged into this pipeline's configuration, in merge order.
    pub includes: Vec<u32>,
    /// `spec:inputs` declared by the entry file, with the values in effect.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spec_inputs: Vec<SpecInputMeta>,
}

/// One `spec:inputs` declaration of a pipeline's entry file and the value
/// it resolved to for that pipeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpecInputMeta {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<serde_json::Value>,
    /// The value in effect: provided by the trigger, include or `--input`,
    /// else the default. `None` when neither exists (an error).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    /// Whether `value` was supplied rather than defaulted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub provided: bool,
}

/// Changed files of the event that would create a pipeline, plus the
/// per-ref lists `rules:changes:compare_to` clauses diffed against.
///
/// `base`/`files` sit only on the pipeline that owns the diff (a root or
/// detached pipeline scanned with `--diff`/`--changed-file`; `base` is absent
/// for an explicit file list). Child pipelines inherit the parent's files
/// (see `parent`) and carry only their own `compare_to` lists; multi-project
/// pipelines have no push event and therefore no `files`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Diff {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
    /// Expanded `compare_to` ref → files changed since its merge base.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub compare_to: IndexMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub name: String,
    /// Name before `parallel`/`matrix` expansion, when expanded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_name: Option<String>,
    pub stage: String,
    pub needs: Vec<Need>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub dependencies: Vec<String>,
    pub when: When,
    pub allow_failure: AllowFailure,
    pub rules: RulesSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Trigger>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel: Option<Parallel>,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub is_pages: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "IndexMap::is_empty", default)]
    pub variables: IndexMap<String, String>,
    pub provenance: Provenance,
    /// The job's effective configuration (defaults applied, extends resolved).
    pub merged_yaml: String,
    /// Per-scenario evaluation results (filled from M4 on).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub evaluations: Vec<JobEvaluation>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum When {
    OnSuccess,
    OnFailure,
    Always,
    Manual,
    Delayed,
    Never,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllowFailure {
    Bool(bool),
    ExitCodes(Vec<i64>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NeedKind {
    /// Ordinary same-pipeline `needs` edge.
    Normal,
    /// `needs:project` — cross-project artifact download, not a trigger.
    CrossProjectArtifact,
    /// `needs:pipeline` — artifact from the parent/upstream pipeline.
    ParentPipeline,
    Unresolved,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Need {
    pub job: String,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub optional: bool,
    #[serde(default = "default_true")]
    pub artifacts: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<String>,
    pub kind: NeedKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<Unresolved>,
    pub span: Span,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RulesMode {
    /// No rules and no only/except: runs whenever the pipeline runs.
    Always,
    Conditional,
    Manual,
    Never,
    /// Legacy `only`/`except`.
    Legacy,
    /// No rules at all (identical behaviour to Always; kept distinct for display).
    None,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RulesSummary {
    pub mode: RulesMode,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub rules: Vec<RuleClause>,
}

impl RulesSummary {
    pub fn none() -> Self {
        RulesSummary {
            mode: RulesMode::None,
            rules: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuleClause {
    #[serde(skip_serializing_if = "Option::is_none", rename = "if")]
    pub r#if: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<Vec<String>>,
    /// `changes:compare_to` (unexpanded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare_to: Option<String>,
    /// `changes:regexp` (unexpanded); `changes` is then an empty list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes_regexp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exists: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<When>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_failure: Option<bool>,
    #[serde(skip_serializing_if = "IndexMap::is_empty", default)]
    pub variables: IndexMap<String, String>,
    pub span: Span,
    /// Present when this "clause" is really legacy only/except content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy: Option<LegacyOnlyExcept>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LegacyOnlyExcept {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub except: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trigger {
    pub kind: TriggerKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    pub forward: Forward,
    #[serde(skip_serializing_if = "IndexMap::is_empty", default)]
    pub inputs: IndexMap<String, serde_json::Value>,
    /// Filled by the trigger walk (M3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<PipelineId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum TriggerKind {
    MultiProject {
        project: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        project_resolved: Option<ProjectRef>,
        #[serde(skip_serializing_if = "Option::is_none")]
        branch_resolved: Option<String>,
    },
    Child {
        includes: Vec<serde_json::Value>,
    },
    DynamicChild {
        artifact: String,
        job: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Forward {
    pub yaml_variables: bool,
    pub pipeline_variables: bool,
}

impl Default for Forward {
    fn default() -> Self {
        // GitLab's FORWARD_DEFAULTS.
        Forward {
            yaml_variables: true,
            pipeline_variables: false,
        }
    }
}

impl Forward {
    pub fn is_default(&self) -> bool {
        self.yaml_variables && !self.pipeline_variables
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Parallel {
    Count(u32),
    Matrix(Vec<IndexMap<String, Vec<String>>>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TriggerEdge {
    pub from_job: JobId,
    pub to_pipeline: PipelineId,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub cycle: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    /// The bridge's `trigger:forward`: what the downstream pipeline inherits.
    #[serde(default, skip_serializing_if = "Forward::is_default")]
    pub forward: Forward,
    pub span: Span,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncludeKind {
    Local,
    Project,
    Remote,
    Template,
    Component,
    /// Synthetic root documents (`trigger:include` child configs).
    Synthetic,
    /// The entry `.gitlab-ci.yml` itself.
    Entry,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IncludeFile {
    pub file: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    pub path: String,
    pub kind: IncludeKind,
    /// The include location as written in the source.
    pub location: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<Unresolved>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IncludeEdge {
    /// Including file (index into `sources`); `None` for the entry file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<u32>,
    /// Included file; `None` when the include is unresolved (see `include_files`
    /// entry with matching `location`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<u32>,
    pub location: String,
    pub order: u32,
    pub span: Span,
    pub pipeline: PipelineId,
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub cycle: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedReason {
    VariableInLocation,
    ProjectNotFound,
    RefNotFound,
    FileNotFound,
    ComponentNeedsCatalog,
    TemplateUnavailable,
    RemoteDisabled,
    RemoteFailed,
    DynamicChild,
    IncludeBudgetExceeded,
    ChildDepthExceeded,
    Cycle,
    ExtendsDepth,
    ReferenceDepth,
    InvalidConfig,
    NotYetImplemented,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Unresolved {
    pub reason: UnresolvedReason,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

impl From<glpv_yaml::Severity> for Severity {
    fn from(s: glpv_yaml::Severity) -> Self {
        match s {
            glpv_yaml::Severity::Error => Severity::Error,
            glpv_yaml::Severity::Warning => Severity::Warning,
            glpv_yaml::Severity::Info => Severity::Info,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub related: Vec<Span>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<PipelineId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Provenance {
    pub defined_at: Span,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub contributors: Vec<Contribution>,
    /// Winning source span per leaf key path (e.g. `"script/0"`).
    #[serde(skip_serializing_if = "IndexMap::is_empty", default)]
    pub leaf_spans: IndexMap<String, Span>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Contribution {
    pub kind: ContributionKind,
    pub span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "name")]
pub enum ContributionKind {
    Include,
    Extends(String),
    Reference(String),
    Default,
    Alias,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobEvaluation {
    pub scenario_id: String,
    pub outcome: Outcome,
    /// `rules:variables` of the matched clause: job variables under this
    /// scenario, forwarded downstream with `trigger:forward:yaml_variables`.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub variables: IndexMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub trace: Vec<RuleTrace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Runs,
    Manual,
    Delayed,
    Skipped,
    Blocked,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuleTrace {
    pub index: usize,
    pub result: String,
    pub clause: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<When>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub vars_used: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Stable pipeline id: sha is deliberately excluded so re-scans compare.
pub fn pipeline_id(
    project: &ProjectRef,
    git_ref: Option<&str>,
    config_path: &str,
    parent: Option<(&PipelineId, &str)>,
) -> PipelineId {
    let mut h = blake3::Hasher::new();
    h.update(project.host.as_bytes());
    h.update(b"|");
    h.update(project.path_lc.as_bytes());
    h.update(b"|");
    h.update(git_ref.unwrap_or("").as_bytes());
    h.update(b"|");
    h.update(config_path.as_bytes());
    if let Some((pid, job)) = parent {
        h.update(b"|");
        h.update(pid.0.as_bytes());
        h.update(b"|");
        h.update(job.as_bytes());
    }
    let hex = h.finalize().to_hex();
    PipelineId(format!("p-{}", &hex.as_str()[..16]))
}

pub fn job_id(pipeline: &PipelineId, name: &str) -> JobId {
    JobId(format!("{}/{}", pipeline.0, name))
}
