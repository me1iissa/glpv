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
/// Where `glpv scan --clone-missing` puts its bare clones, under the first
/// clone root: `<root>/.glpv-clones/<host>/<group>/<project>.git`.
pub const CLONES_DIR: &str = ".glpv-clones";
const SKIP_DIRS: [&str; 4] = ["node_modules", "target", CLONES_DIR, "vendor"];

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
            // The general walk skips dot directories; the clone cache is
            // ours and is picked up explicitly (bare repositories inside).
            find_repos(&root.join(CLONES_DIR), 0, &mut repo_dirs);
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
    if dir.join(".git").exists() || is_bare_repo(dir) {
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

/// A bare repository (`git clone --bare`): the object store sits directly
/// in the directory instead of under `.git/`.
fn is_bare_repo(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir()
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

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::source::TreeRef;

    fn run(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn bare_clones_under_the_clone_cache_are_indexed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("projects");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        run(&src, &["init", "-q", "-b", "main"]);
        run(&src, &["config", "commit.gpgsign", "false"]);
        std::fs::write(src.join(".gitlab-ci.yml"), "job:\n  script: echo\n").unwrap();
        run(&src, &["add", "-A"]);
        run(&src, &["commit", "-q", "-m", "one"]);
        run(&src, &["tag", "v1.0.0"]);

        // What `--clone-missing` produces: a bare clone whose origin is the
        // instance URL, under `<root>/.glpv-clones/<host>/<path>.git`.
        let dest = root
            .join(CLONES_DIR)
            .join("gitlab.example.com/acme/api.git");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        run(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                &src.display().to_string(),
                &dest.display().to_string(),
            ],
        );
        run(
            &dest,
            &[
                "remote",
                "set-url",
                "origin",
                "https://gitlab.example.com/acme/api.git",
            ],
        );

        let index = LocalIndex::build(&[root], &[]);
        assert!(index.diagnostics.is_empty(), "{:?}", index.diagnostics);
        let project = index
            .lookup(&ProjectKey::new("gitlab.example.com", "acme/api"))
            .expect("the bare clone is indexed by its origin URL");
        assert!(project.is_bare());
        assert_eq!(project.meta().display_path, "acme/api");
        assert_eq!(project.default_branch().unwrap(), "main");
        let sha = project.resolve_ref("main").unwrap().expect("main resolves");
        assert_eq!(project.resolve_ref("v1.0.0").unwrap(), Some(sha.clone()));
        let tree = TreeRef::Commit(sha);
        assert_eq!(
            project.read(&tree, ".gitlab-ci.yml").unwrap().as_deref(),
            Some("job:\n  script: echo\n")
        );
        assert_eq!(&*project.list_tree(&tree).unwrap(), &[".gitlab-ci.yml"]);
        assert_eq!(project.tags().unwrap(), vec!["v1.0.0"]);
    }
}
