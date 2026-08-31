//! The project index: walk a folder of clones and key every repository by its
//! git remote URLs. Folder names are untrustworthy (a clone named `vidchat/`
//! can be `acme/arcwave`); remotes are the identity. Explicit overrides
//! from `glpv.toml` win over everything.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::ProjectOverride;
use crate::model::{Diagnostic, Severity};
use crate::source::local::LocalGitProject;
use crate::source::{ProjectKey, ProjectLocator, ProjectMeta, ProjectSource, SourceError};

const MAX_WALK_DEPTH: usize = 6;
const SKIP_DIRS: [&str; 4] = ["node_modules", "target", ".glpv-clones", "vendor"];

pub struct LocalIndex {
    by_key: HashMap<ProjectKey, Arc<LocalGitProject>>,
    /// path_lc → hosts that have it (for host-less lookup).
    by_path: HashMap<String, Vec<ProjectKey>>,
    metas: Vec<ProjectMeta>,
    pub diagnostics: Vec<Diagnostic>,
}

impl LocalIndex {
    pub fn build(roots: &[PathBuf], overrides: &[ProjectOverride]) -> LocalIndex {
        let mut index = LocalIndex {
            by_key: HashMap::new(),
            by_path: HashMap::new(),
            metas: Vec::new(),
            diagnostics: Vec::new(),
        };

        let mut repo_dirs = Vec::new();
        for root in roots {
            find_repos(root, 0, &mut repo_dirs);
        }
        repo_dirs.sort();
        repo_dirs.dedup();

        for dir in repo_dirs {
            let project = match LocalGitProject::open(&dir) {
                Ok(p) => p,
                Err(e) => {
                    index.diagnostics.push(diag(
                        Severity::Warning,
                        "index.unreadable",
                        format!("skipping {}: {e}", dir.display()),
                    ));
                    continue;
                }
            };
            let mut project = project;
            if let Some(ov) = overrides.iter().find(|o| dir.ends_with(&o.dir)) {
                project.apply_override(ov);
            }
            index.insert(Arc::new(project));
        }
        index
    }

    fn insert(&mut self, project: Arc<LocalGitProject>) {
        let meta = project.meta().clone();
        let key = meta.key.clone();
        if let Some(existing) = self.by_key.get(&key) {
            let a = existing.root().display().to_string();
            let b = project.root().display().to_string();
            if a != b {
                self.diagnostics.push(diag(
                    Severity::Warning,
                    "index.ambiguous",
                    format!(
                        "{}/{} is claimed by both {a} and {b}; using {a} \
                         (add a [[projects]] override in glpv.toml to choose)",
                        key.host, key.path_lc
                    ),
                ));
            }
            return;
        }
        self.by_path
            .entry(key.path_lc.clone())
            .or_default()
            .push(key.clone());
        self.by_key.insert(key, project);
        self.metas.push(meta);
    }

    /// Exact host+path lookup, falling back to a unique path-only match so
    /// `--entry group/proj` works without naming the host.
    pub fn lookup(&self, key: &ProjectKey) -> Option<Arc<LocalGitProject>> {
        if let Some(p) = self.by_key.get(key) {
            return Some(p.clone());
        }
        match self.by_path.get(&key.path_lc).map(|v| v.as_slice()) {
            Some([only]) => self.by_key.get(only).cloned(),
            _ => None,
        }
    }
}

impl ProjectLocator for LocalIndex {
    fn locate(&self, key: &ProjectKey) -> Result<Option<Arc<dyn ProjectSource>>, SourceError> {
        Ok(self.lookup(key).map(|p| p as Arc<dyn ProjectSource>))
    }

    fn all(&self) -> Vec<ProjectMeta> {
        let mut v = self.metas.clone();
        v.sort_by(|a, b| (&a.key.host, &a.key.path_lc).cmp(&(&b.key.host, &b.key.path_lc)));
        v
    }
}

fn find_repos(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    if dir.join(".git").exists() {
        if let Ok(canon) = dir.canonicalize() {
            out.push(canon);
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    children.sort();
    for child in children {
        let name = child.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        find_repos(&child, depth + 1, out);
    }
}

fn diag(severity: Severity, code: &str, message: String) -> Diagnostic {
    Diagnostic {
        severity,
        code: code.to_string(),
        message,
        span: None,
        related: Vec::new(),
        hint: None,
        pipeline: None,
    }
}
