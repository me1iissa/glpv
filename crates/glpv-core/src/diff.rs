//! The diff oracle `rules:changes` is evaluated against for one
//! `(project, tree)`: the push-event file list (from `--diff <base>` or an
//! explicit `--changed-file` list), lazily computed `compare_to` lists, and
//! the diagnostics those git calls produce — queued until the scan can stamp
//! them with a pipeline id.
//!
//! One oracle is shared by a root pipeline and its child pipelines (children
//! inherit the parent's diff); a multi-project pipeline gets its own,
//! without a push-event list.

use std::sync::{Arc, Mutex};

use indexmap::IndexMap;

use crate::model::{self, Diagnostic, Severity};
use crate::rules::changes::{ChangesMatch, ChangesQuery, match_changes};
use crate::source::{ProjectSource, TreeRef};

/// Where the changed-file list of a scan comes from.
#[derive(Clone, Debug)]
pub enum DiffSpec {
    /// `git diff BASE...<scanned tree>` (merge base) in every root project.
    Base(String),
    /// An explicit list of repository-relative paths.
    Files(Vec<String>),
}

pub struct DiffOracle {
    project: Arc<dyn ProjectSource>,
    tree: TreeRef,
    base: Option<String>,
    files: Option<Arc<[String]>>,
    compare_cache: Mutex<IndexMap<String, Option<Arc<[String]>>>>,
    pending: Mutex<Vec<Diagnostic>>,
}

fn diag(code: &str, message: String, hint: Option<&str>) -> Diagnostic {
    Diagnostic {
        severity: Severity::Warning,
        code: code.to_string(),
        message,
        span: None,
        related: Vec::new(),
        hint: hint.map(|h| h.to_string()),
        pipeline: None,
    }
}

impl DiffOracle {
    /// Build the oracle; a `Base` spec runs the diff immediately so that the
    /// scan's report can say how many files changed.
    pub fn new(
        project: Arc<dyn ProjectSource>,
        tree: TreeRef,
        spec: Option<&DiffSpec>,
    ) -> Arc<Self> {
        let mut pending = Vec::new();
        let (base, files) = match spec {
            None => (None, None),
            Some(DiffSpec::Files(list)) => (None, Some(Arc::from(list.clone()))),
            Some(DiffSpec::Base(base)) => {
                let name = &project.meta().display_path;
                let files = match project.changed_files(base, &tree) {
                    Ok(Some(f)) => Some(f),
                    Ok(None) => {
                        pending.push(diag(
                            "diff.unavailable",
                            format!(
                                "cannot diff {name} against `{base}`: the ref does not resolve \
                                 or shares no history with the scanned tree; `changes:` stays \
                                 undecided"
                            ),
                            Some("fetch the ref into the clone, or pass --changed-file <path>"),
                        ));
                        None
                    }
                    Err(e) => {
                        pending.push(diag(
                            "source.error",
                            format!("cannot diff {name} against `{base}`: {e}"),
                            None,
                        ));
                        None
                    }
                };
                (Some(base.clone()), files)
            }
        };
        Arc::new(DiffOracle {
            project,
            tree,
            base,
            files,
            compare_cache: Mutex::new(IndexMap::new()),
            pending: Mutex::new(pending),
        })
    }

    pub fn base(&self) -> Option<&str> {
        self.base.as_deref()
    }

    /// The push-event changed files, when known.
    pub fn files(&self) -> Option<&[String]> {
        self.files.as_deref()
    }

    /// Files changed since the merge base of `r` (`changes:compare_to`),
    /// diffed once per ref. `None` when the ref does not resolve.
    pub fn compare_to(&self, r: &str) -> Option<Arc<[String]>> {
        if let Some(v) = self.compare_cache.lock().unwrap().get(r) {
            return v.clone();
        }
        let name = &self.project.meta().display_path;
        let result = match self.project.changed_files(r, &self.tree) {
            Ok(Some(f)) => Some(f),
            Ok(None) => {
                self.pending.lock().unwrap().push(diag(
                    "diff.compare-to-unresolved",
                    format!(
                        "`changes:compare_to: {r}` does not resolve in {name} (or shares no \
                         history with the scanned tree); the clause is undecided"
                    ),
                    Some("GitLab would reject the pipeline; fetch the ref into the clone"),
                ));
                None
            }
            Err(e) => {
                self.pending.lock().unwrap().push(diag(
                    "source.error",
                    format!("cannot diff {name} against `changes:compare_to: {r}`: {e}"),
                    None,
                ));
                None
            }
        };
        self.compare_cache
            .lock()
            .unwrap()
            .insert(r.to_string(), result.clone());
        result
    }

    /// The `ChangesChecker` behind `EvalContext.changes`.
    pub fn check(&self, q: &ChangesQuery<'_>) -> Option<ChangesMatch> {
        let files = match q.compare_to {
            Some(r) => self.compare_to(r)?,
            None => self.files.clone()?,
        };
        Some(match_changes(q.patterns, &files))
    }

    /// Diagnostics produced since the last call (unstamped: no pipeline id).
    pub fn take_diags(&self) -> Vec<Diagnostic> {
        std::mem::take(&mut *self.pending.lock().unwrap())
    }

    /// The graph JSON view. `own` — this pipeline owns the push-event diff
    /// (a child pipeline inherits it and lists only its `compare_to` refs).
    /// `None` when there is nothing to record.
    pub fn to_model(&self, own: bool, compare_refs: &[String]) -> Option<model::Diff> {
        let mut d = model::Diff::default();
        if own {
            d.base = self.base.clone();
            d.files = self.files.as_ref().map(|f| f.to_vec());
        }
        for r in compare_refs {
            if let Some(f) = self.compare_to(r) {
                d.compare_to.insert(r.clone(), f.to_vec());
            }
        }
        if d.base.is_none() && d.files.is_none() && d.compare_to.is_empty() {
            None
        } else {
            Some(d)
        }
    }
}
