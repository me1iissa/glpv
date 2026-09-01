//! Abstractions over where project files come from (local clones, the
//! GitLab REST API) plus the source-file table shared by the resolver.

#[cfg(feature = "api")]
pub mod api;
pub mod index;
pub mod local;

use std::sync::Arc;

use glpv_yaml::FileId;

use crate::model::ProjectRef;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ProjectKey {
    pub host: String,
    pub path_lc: String,
}

impl ProjectKey {
    pub fn new(host: &str, path: &str) -> Self {
        ProjectKey {
            host: host.to_lowercase(),
            path_lc: path.to_lowercase(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Sha(pub String);

/// Which tree of a project to read from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TreeRef {
    /// The working tree of a local clone (uncommitted changes included).
    Worktree,
    Commit(Sha),
}

#[derive(Clone, Debug)]
pub enum ProjectOrigin {
    LocalClone(std::path::PathBuf),
    Api { project_id: u64 },
}

#[derive(Clone, Debug)]
pub struct ProjectMeta {
    pub key: ProjectKey,
    /// Display-cased path (as written in the remote URL / API).
    pub display_path: String,
    pub origin: ProjectOrigin,
    pub ci_config_path: Option<String>,
}

impl ProjectMeta {
    pub fn project_ref(&self) -> ProjectRef {
        ProjectRef::new(self.key.host.clone(), self.display_path.clone())
    }
}

#[derive(thiserror::Error, Debug)]
pub enum SourceError {
    #[error("git failed: {0}")]
    Git(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0} is not inside a git repository")]
    NotAGitRepo(std::path::PathBuf),
    #[error("api error: {0}")]
    Api(String),
}

pub trait ProjectSource: Send + Sync {
    fn meta(&self) -> &ProjectMeta;
    fn default_branch(&self) -> Result<String, SourceError>;
    /// Resolve a branch / tag / sha spelling to a commit, `None` if unknown.
    fn resolve_ref(&self, r: &str) -> Result<Option<Sha>, SourceError>;
    /// Read a file; `Ok(None)` means the file does not exist at that tree.
    fn read(&self, at: &TreeRef, path: &str) -> Result<Option<String>, SourceError>;
    fn exists(&self, at: &TreeRef, path: &str) -> Result<bool, SourceError> {
        Ok(self.read(at, path)?.is_some())
    }
    /// Recursive file listing of the tree, in `git ls-tree` order.
    fn list_tree(&self, at: &TreeRef) -> Result<Arc<[String]>, SourceError>;
    /// The part of [`list_tree`](Self::list_tree) under directory `prefix`
    /// (no trailing slash; empty for the whole tree), same order. A source
    /// that can list a subtree cheaply overrides this.
    fn list_tree_under(&self, at: &TreeRef, prefix: &str) -> Result<Arc<[String]>, SourceError> {
        let all = self.list_tree(at)?;
        let prefix = prefix.trim_matches('/');
        if prefix.is_empty() {
            return Ok(all);
        }
        let dir = format!("{prefix}/");
        Ok(all
            .iter()
            .filter(|p| p.starts_with(&dir))
            .cloned()
            .collect::<Vec<_>>()
            .into())
    }
    fn tags(&self) -> Result<Vec<String>, SourceError>;
    /// Paths changed between the merge base of `base` and `head` — GitLab's
    /// push diff: no rename detection (a rename is its old and new path) and,
    /// for the working tree, uncommitted and untracked files included.
    /// `Ok(None)` when `base` does not resolve or shares no history with `head`.
    fn changed_files(
        &self,
        base: &str,
        head: &TreeRef,
    ) -> Result<Option<Arc<[String]>>, SourceError> {
        let _ = (base, head);
        Ok(None)
    }
}

pub trait ProjectLocator: Send + Sync {
    fn locate(&self, key: &ProjectKey) -> Result<Option<Arc<dyn ProjectSource>>, SourceError>;
    fn all(&self) -> Vec<ProjectMeta>;
}

/// Several locators in order of preference: the first hit wins (the local
/// clones folder before the API). A locator that fails does not stop the
/// search; when nothing is found, its reason is what the caller sees.
pub struct ChainLocator {
    locators: Vec<Arc<dyn ProjectLocator>>,
}

impl ChainLocator {
    pub fn new(locators: Vec<Arc<dyn ProjectLocator>>) -> ChainLocator {
        ChainLocator { locators }
    }
}

impl ProjectLocator for ChainLocator {
    fn locate(&self, key: &ProjectKey) -> Result<Option<Arc<dyn ProjectSource>>, SourceError> {
        let mut failure: Option<SourceError> = None;
        for l in &self.locators {
            match l.locate(key) {
                Ok(Some(p)) => return Ok(Some(p)),
                Ok(None) => {}
                Err(e) => failure = Some(e),
            }
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(None),
        }
    }

    fn all(&self) -> Vec<ProjectMeta> {
        let mut out: Vec<ProjectMeta> = Vec::new();
        for l in &self.locators {
            for m in l.all() {
                if !out.iter().any(|o| o.key == m.key) {
                    out.push(m);
                }
            }
        }
        out
    }
}

/// Instance-level services beyond project files: the CI templates the
/// instance ships and the release list behind CI/CD catalog versions.
pub trait InstanceApi: Send + Sync {
    /// The host this API serves (lower-cased, no port).
    fn host(&self) -> &str;
    /// `include:template` content (`Ok(None)`: the instance does not serve it).
    fn template(&self, name: &str) -> Result<Option<String>, SourceError>;
    /// Release tag names of a project, newest first (`Ok(None)`: another
    /// host, no such project, or no visible releases).
    fn release_tags(&self, key: &ProjectKey) -> Result<Option<Vec<String>>, SourceError>;
}

/// `include:remote` bodies. `integrity` is the include's `sha256-<base64>`
/// pin, verified when given.
pub trait RemoteFetcher: Send + Sync {
    fn fetch(&self, url: &str, integrity: Option<&str>) -> Result<String, SourceError>;
}

/// Everything the resolver can pull files from.
pub struct Sources {
    pub locator: Option<Arc<dyn ProjectLocator>>,
    /// The project whose `lib/gitlab/ci/templates/` provides `include:template`.
    pub templates_key: ProjectKey,
    /// The configured instance API, when there is one.
    pub api: Option<Arc<dyn InstanceApi>>,
    /// How `include:remote` is fetched when `--allow-remote` is set.
    pub remote: Option<Arc<dyn RemoteFetcher>>,
}

impl Sources {
    pub fn without_index() -> Sources {
        Sources {
            locator: None,
            templates_key: ProjectKey::new("gitlab.com", "gitlab-org/gitlab"),
            api: None,
            remote: None,
        }
    }

    pub fn with_locator(locator: Arc<dyn ProjectLocator>) -> Sources {
        Sources {
            locator: Some(locator),
            ..Sources::without_index()
        }
    }

    pub fn locate(&self, key: &ProjectKey) -> Result<Option<Arc<dyn ProjectSource>>, SourceError> {
        match &self.locator {
            Some(l) => l.locate(key),
            None => Ok(None),
        }
    }
}

/// Where a registered source file came from (mirrors `model::SourceFile`).
#[derive(Clone, Debug)]
pub struct FileOrigin {
    pub project: Option<ProjectRef>,
    pub sha: Option<String>,
    pub path: String,
}

/// FileId ↔ text/origin table for one scan.
#[derive(Default)]
pub struct SourceMap {
    files: Vec<(FileOrigin, String)>,
}

impl SourceMap {
    pub fn insert(&mut self, origin: FileOrigin, text: String) -> FileId {
        let id = FileId(self.files.len() as u32);
        self.files.push((origin, text));
        id
    }

    pub fn text(&self, id: FileId) -> &str {
        &self.files[id.0 as usize].1
    }

    pub fn origin(&self, id: FileId) -> &FileOrigin {
        &self.files[id.0 as usize].0
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn to_model(&self, embed_text: bool) -> Vec<crate::model::SourceFile> {
        self.files
            .iter()
            .enumerate()
            .map(|(i, (o, text))| crate::model::SourceFile {
                file: i as u32,
                project: o.project.clone(),
                sha: o.sha.clone(),
                path: o.path.clone(),
                text: embed_text.then(|| text.clone()),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct Stub {
        meta: ProjectMeta,
    }

    impl ProjectSource for Stub {
        fn meta(&self) -> &ProjectMeta {
            &self.meta
        }
        fn default_branch(&self) -> Result<String, SourceError> {
            Ok("main".into())
        }
        fn resolve_ref(&self, _: &str) -> Result<Option<Sha>, SourceError> {
            Ok(None)
        }
        fn read(&self, _: &TreeRef, _: &str) -> Result<Option<String>, SourceError> {
            Ok(None)
        }
        fn list_tree(&self, _: &TreeRef) -> Result<Arc<[String]>, SourceError> {
            Ok(Vec::new().into())
        }
        fn tags(&self) -> Result<Vec<String>, SourceError> {
            Ok(Vec::new())
        }
    }

    /// Serves `known`; fails with `failure` for anything else when set.
    struct StubLocator {
        label: &'static str,
        known: Vec<ProjectKey>,
        failure: Option<String>,
        asked: Mutex<Vec<ProjectKey>>,
    }

    impl ProjectLocator for StubLocator {
        fn locate(&self, key: &ProjectKey) -> Result<Option<Arc<dyn ProjectSource>>, SourceError> {
            self.asked.lock().unwrap().push(key.clone());
            if self.known.contains(key) {
                return Ok(Some(Arc::new(Stub {
                    meta: ProjectMeta {
                        key: key.clone(),
                        display_path: format!("{}:{}", self.label, key.path_lc),
                        origin: ProjectOrigin::LocalClone(std::path::PathBuf::new()),
                        ci_config_path: None,
                    },
                })));
            }
            match &self.failure {
                Some(f) => Err(SourceError::Api(f.clone())),
                None => Ok(None),
            }
        }

        fn all(&self) -> Vec<ProjectMeta> {
            self.known
                .iter()
                .map(|k| ProjectMeta {
                    key: k.clone(),
                    display_path: k.path_lc.clone(),
                    origin: ProjectOrigin::LocalClone(std::path::PathBuf::new()),
                    ci_config_path: None,
                })
                .collect()
        }
    }

    #[test]
    fn chain_prefers_the_first_locator_and_reports_the_last_failure() {
        let shared = ProjectKey::new("gitlab.example.com", "acme/shared");
        let api_only = ProjectKey::new("gitlab.example.com", "acme/api-only");
        let nowhere = ProjectKey::new("gitlab.example.com", "acme/nowhere");
        let local = Arc::new(StubLocator {
            label: "local",
            known: vec![shared.clone()],
            failure: None,
            asked: Mutex::new(Vec::new()),
        });
        let api = Arc::new(StubLocator {
            label: "api",
            known: vec![shared.clone(), api_only.clone()],
            failure: Some("not visible through the API".into()),
            asked: Mutex::new(Vec::new()),
        });
        let chain = ChainLocator::new(vec![
            local.clone() as Arc<dyn ProjectLocator>,
            api.clone() as Arc<dyn ProjectLocator>,
        ]);

        let p = chain.locate(&shared).unwrap().unwrap();
        assert_eq!(p.meta().display_path, "local:acme/shared");
        assert!(api.asked.lock().unwrap().is_empty(), "the API is not asked");

        let p = chain.locate(&api_only).unwrap().unwrap();
        assert_eq!(p.meta().display_path, "api:acme/api-only");

        let e = match chain.locate(&nowhere) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("neither locator has it"),
        };
        assert!(e.contains("not visible through the API"), "{e}");

        let all = chain.all();
        assert_eq!(all.len(), 2, "shared is listed once");
        assert_eq!(all[0].key, shared);
        assert_eq!(all[1].key, api_only);

        // Without a failing locator a miss is a plain `None`.
        let quiet = ChainLocator::new(vec![local as Arc<dyn ProjectLocator>]);
        assert!(quiet.locate(&nowhere).unwrap().is_none());
    }
}
