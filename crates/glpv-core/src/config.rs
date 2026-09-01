//! `glpv.toml`: defaults and per-project overrides for the local index.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlpvConfig {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default, rename = "projects")]
    pub project_overrides: Vec<ProjectOverride>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// Clone roots to index.
    #[serde(default)]
    pub projects: Vec<PathBuf>,
    /// Host assumed for `--entry group/project`.
    pub host: Option<String>,
    /// Index key (`host/group/project`) of a gitlab-org/gitlab clone that
    /// provides `include:template` files. Defaults to gitlab.com/gitlab-org/gitlab.
    pub templates_from: Option<String>,
    /// GitLab host (or URL) whose REST API provides projects that are not
    /// cloned locally, as if `--api <host>` were passed on every run.
    pub api: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectOverride {
    /// Clone directory this override applies to (suffix match).
    pub dir: PathBuf,
    pub host: Option<String>,
    pub path: Option<String>,
    pub default_branch: Option<String>,
    pub ci_config_path: Option<String>,
}

#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("{0}: {1}")]
    Parse(PathBuf, toml::de::Error),
}

impl GlpvConfig {
    pub fn load(path: &Path) -> Result<GlpvConfig, ConfigError> {
        let text =
            std::fs::read_to_string(path).map_err(|e| ConfigError::Io(path.to_path_buf(), e))?;
        toml::from_str(&text).map_err(|e| ConfigError::Parse(path.to_path_buf(), e))
    }

    /// Search order: explicit path → ./glpv.toml → $XDG_CONFIG_HOME/glpv/config.toml.
    pub fn discover(explicit: Option<&Path>) -> Result<GlpvConfig, ConfigError> {
        if let Some(p) = explicit {
            return Self::load(p);
        }
        let local = Path::new("glpv.toml");
        if local.exists() {
            return Self::load(local);
        }
        if let Some(base) = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        {
            let user = base.join("glpv/config.toml");
            if user.exists() {
                return Self::load(&user);
            }
        }
        Ok(GlpvConfig::default())
    }
}
