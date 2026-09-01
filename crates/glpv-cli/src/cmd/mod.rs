pub mod check;
pub mod clone;
pub mod index;
pub mod resolve;
pub mod scan;
pub mod serve;

use std::path::PathBuf;
use std::sync::Arc;

use glpv_core::config::GlpvConfig;
use glpv_core::model::Graph;
use glpv_core::source::api::auth::{self, Credentials};
use glpv_core::source::api::cache::ApiCache;
use glpv_core::source::api::transport::{GlabTransport, HttpsTransport, Transport};
use glpv_core::source::api::{ApiClient, ApiLocator, split_origin};
use glpv_core::source::index::LocalIndex;
use glpv_core::source::{ChainLocator, InstanceApi, ProjectKey, ProjectLocator, Sources};
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
    /// Read projects that are not cloned through the GitLab REST API of this
    /// host (or URL); with no value, the --host / glpv.toml host. Local
    /// clones are still preferred.
    #[arg(long, value_name = "HOST", num_args = 0..=1, default_missing_value = "")]
    pub api: Option<String>,
    /// API token (personal/project access token, or an OAuth token); else
    /// $GLPV_TOKEN, $GITLAB_TOKEN, glab's config, `glab api`, anonymous.
    #[arg(long, value_name = "TOKEN")]
    pub token: Option<String>,
    /// Ask the API again for refs, tags, releases and project metadata
    /// instead of trusting the ten-minute cache (file contents at a sha are
    /// immutable and stay cached).
    #[arg(long)]
    pub refresh: bool,
}

/// The configured instance API.
pub struct ApiSetup {
    pub client: Arc<ApiClient>,
    pub locator: Arc<ApiLocator>,
}

pub struct IndexSetup {
    pub sources: Sources,
    pub index: Arc<LocalIndex>,
    #[allow(dead_code)] // carried for later consumers (scenario defaults, hosts)
    pub config: GlpvConfig,
    pub index_diags: Vec<glpv_core::model::Diagnostic>,
    pub host: Option<String>,
    /// The clone roots the index was built from.
    pub roots: Vec<PathBuf>,
    pub api: Option<ApiSetup>,
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

        let mut host = self.host.clone().or_else(|| config.defaults.host.clone());
        let api_host = match &self.api {
            Some(h) if !h.trim().is_empty() => Some(h.trim().to_string()),
            Some(_) => Some(host.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "--api needs a host: pass --api <host>, --host <host>, or set \
                     defaults.host in glpv.toml"
                )
            })?),
            None => config.defaults.api.clone(),
        };
        let api = match api_host {
            Some(h) => Some(build_api(&h, self.token.as_deref(), self.refresh)?),
            None => None,
        };
        if host.is_none()
            && let Some(a) = &api
        {
            host = Some(a.client.host().to_string());
        }
        let locator: Arc<dyn ProjectLocator> = match &api {
            Some(a) => Arc::new(ChainLocator::new(vec![
                index.clone() as Arc<dyn ProjectLocator>,
                a.locator.clone() as Arc<dyn ProjectLocator>,
            ])),
            None => index.clone(),
        };
        Ok(IndexSetup {
            sources: Sources {
                locator: Some(locator),
                templates_key,
                api: api
                    .as_ref()
                    .map(|a| a.client.clone() as Arc<dyn InstanceApi>),
                remote: None,
            },
            index,
            config,
            index_diags,
            host,
            roots,
            api,
        })
    }
}

/// Credentials → transport → client for one instance.
fn build_api(host_or_url: &str, token: Option<&str>, refresh: bool) -> anyhow::Result<ApiSetup> {
    let (origin, host) = split_origin(host_or_url);
    let (creds, label) = auth::discover(&host, token);
    let transport: Box<dyn Transport> = match creds {
        Credentials::Token(a) => Box::new(HttpsTransport::new(Some(a), label)?),
        Credentials::Glab => Box::new(GlabTransport::new(&host, &format!("{origin}/api/v4/"))?),
        Credentials::Anonymous => Box::new(HttpsTransport::new(None, label)?),
    };
    let cache = ApiCache::new(ApiCache::default_root(), refresh);
    let client = Arc::new(ApiClient::new(host_or_url, transport, cache));
    Ok(ApiSetup {
        locator: Arc::new(ApiLocator::new(client.clone())),
        client,
    })
}

/// The `include:remote` fetcher for `--allow-remote`: the configured API
/// (credentials for its own host) or anonymous HTTPS.
pub fn remote_fetcher(
    setup: &IndexSetup,
    refresh: bool,
) -> anyhow::Result<Arc<dyn glpv_core::source::RemoteFetcher>> {
    Ok(match &setup.api {
        Some(a) => a.client.clone(),
        None => Arc::new(glpv_core::source::api::RemoteOnly::new(
            Box::new(HttpsTransport::new(None, "anonymous")?),
            ApiCache::new(ApiCache::default_root(), refresh),
        )),
    })
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
