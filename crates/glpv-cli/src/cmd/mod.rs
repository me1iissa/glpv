pub mod check;
pub mod index;
pub mod resolve;
pub mod scan;

use std::path::PathBuf;
use std::sync::Arc;

use glpv_core::config::GlpvConfig;
use glpv_core::model::Graph;
use glpv_core::source::index::LocalIndex;
use glpv_core::source::{ProjectKey, Sources};
use glpv_core::vars::Scenario;
use indexmap::IndexMap;

/// Shared scenario flags.
#[derive(clap::Args, Clone)]
pub struct ScenarioArgs {
    /// Pipeline source to simulate (a CI_PIPELINE_SOURCE value).
    #[arg(long, default_value = "push")]
    pub source: String,
    /// Simulated ref name (defaults to the project's default branch).
    #[arg(long = "sim-ref")]
    pub sim_ref: Option<String>,
    /// Treat the simulated ref as a tag.
    #[arg(long)]
    pub tag: bool,
    /// Extra CI variables, K=V (repeatable).
    #[arg(long = "var", value_name = "K=V")]
    pub vars: Vec<String>,
}

impl ScenarioArgs {
    pub fn to_scenario(&self) -> anyhow::Result<Scenario> {
        let mut vars = IndexMap::new();
        for kv in &self.vars {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("--var must be K=V, got `{kv}`"))?;
            vars.insert(k.to_string(), v.to_string());
        }
        let id = format!(
            "{}@{}",
            self.source,
            self.sim_ref.as_deref().unwrap_or("default")
        );
        Ok(Scenario {
            id,
            source: self.source.clone(),
            git_ref: self.sim_ref.clone(),
            is_tag: self.tag,
            vars,
        })
    }
}

/// Shared flags controlling the project index.
#[derive(clap::Args, Clone)]
pub struct IndexArgs {
    /// Clone root(s) to index for cross-project resolution (repeatable).
    #[arg(long = "projects", value_name = "DIR")]
    pub projects: Vec<PathBuf>,
    /// GitLab host assumed for `--entry group/project`.
    #[arg(long)]
    pub host: Option<String>,
    /// Path to glpv.toml (default: ./glpv.toml, then $XDG_CONFIG_HOME/glpv/config.toml).
    #[arg(long)]
    pub config: Option<PathBuf>,
}

pub struct IndexSetup {
    pub sources: Sources,
    pub index: Arc<LocalIndex>,
    #[allow(dead_code)] // carried for M4/M5 consumers (scenario defaults, hosts)
    pub config: GlpvConfig,
    pub index_diags: Vec<glpv_core::model::Diagnostic>,
    pub host: Option<String>,
}

impl IndexArgs {
    pub fn build(&self) -> anyhow::Result<IndexSetup> {
        let config = GlpvConfig::discover(self.config.as_deref())?;
        let mut roots = self.projects.clone();
        if roots.is_empty() {
            roots = config.defaults.projects.clone();
        }
        let index = Arc::new(LocalIndex::build(&roots, &config.project_overrides));
        let index_diags = index.diagnostics.clone();

        let templates_key = config
            .defaults
            .templates_from
            .as_deref()
            .and_then(|s| s.split_once('/'))
            .map(|(host, path)| ProjectKey::new(host, path))
            .unwrap_or_else(|| ProjectKey::new("gitlab.com", "gitlab-org/gitlab"));

        let host = self.host.clone().or_else(|| config.defaults.host.clone());
        Ok(IndexSetup {
            sources: Sources {
                locator: Some(index.clone()),
                templates_key,
            },
            index,
            config,
            index_diags,
            host,
        })
    }
}

pub fn print_diagnostics(graph: &Graph) {
    use glpv_core::model::Severity;
    let diags = &graph.diagnostics;
    let errors = diags
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warnings = diags
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .count();
    let path_of = |file: u32| {
        graph
            .sources
            .iter()
            .find(|s| s.file == file)
            .map(|s| s.path.clone())
            .unwrap_or_else(|| format!("file{file}"))
    };
    for d in diags {
        let tag = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warn ",
            Severity::Info => "info ",
        };
        let loc = d
            .span
            .map(|s| format!("   {}:{}", path_of(s.file), s.start[0]))
            .unwrap_or_default();
        eprintln!("{tag} [{}] {}{loc}", d.code, d.message);
        if let Some(h) = &d.hint {
            eprintln!("      hint: {h}");
        }
    }
    if errors + warnings > 0 {
        eprintln!("{errors} error(s), {warnings} warning(s)");
    }
}
