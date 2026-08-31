//! Abstractions over where project files come from (local clones now,
//! the GitLab API later) plus the source-file table shared by the resolver.

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
    fn tags(&self) -> Result<Vec<String>, SourceError>;
}

pub trait ProjectLocator: Send + Sync {
    fn locate(&self, key: &ProjectKey) -> Result<Option<Arc<dyn ProjectSource>>, SourceError>;
    fn all(&self) -> Vec<ProjectMeta>;
}

/// Everything the resolver can pull files from.
pub struct Sources {
    pub locator: Option<Arc<dyn ProjectLocator>>,
    /// The project whose `lib/gitlab/ci/templates/` provides `include:template`.
    pub templates_key: ProjectKey,
}

impl Sources {
    pub fn without_index() -> Sources {
        Sources {
            locator: None,
            templates_key: ProjectKey::new("gitlab.com", "gitlab-org/gitlab"),
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
