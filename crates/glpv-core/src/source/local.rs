//! Local git clones as a [`ProjectSource`], via the `git` subprocess.
//!
//! Files are read at a ref with `git show <sha>:<path>` — no checkout — and
//! the working tree is a first-class tree so `glpv scan --file` reflects
//! uncommitted edits.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use super::{ProjectKey, ProjectMeta, ProjectOrigin, ProjectSource, Sha, SourceError, TreeRef};

pub struct LocalGitProject {
    meta: ProjectMeta,
    root: PathBuf,
    override_default_branch: Option<String>,
    tree_cache: Mutex<Vec<(TreeRef, std::sync::Arc<[String]>)>>,
}

impl LocalGitProject {
    /// Open the repository containing `dir`. Project identity comes from the
    /// `origin` remote URL when parseable, else from the directory name.
    pub fn open(dir: &Path) -> Result<Self, SourceError> {
        let root = git_str(dir, &["rev-parse", "--show-toplevel"])?
            .ok_or_else(|| SourceError::NotAGitRepo(dir.to_path_buf()))?;
        let root = PathBuf::from(root.trim());

        let mut host = String::from("local");
        let mut display_path = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());

        if let Some(remotes) = git_str(&root, &["config", "--get-regexp", r"^remote\..*\.url$"])? {
            let mut parsed: Vec<(String, String, String)> = Vec::new(); // (remote, host, path)
            for line in remotes.lines() {
                let Some((key, url)) = line.split_once(' ') else {
                    continue;
                };
                let name = key
                    .strip_prefix("remote.")
                    .and_then(|k| k.strip_suffix(".url"))
                    .unwrap_or(key);
                if let Some((h, p)) = parse_remote_url(url) {
                    parsed.push((name.to_string(), h, p));
                }
            }
            parsed.sort_by_key(|(name, _, _)| (name != "origin", name.clone()));
            if let Some((_, h, p)) = parsed.first() {
                host = h.clone();
                display_path = p.clone();
            }
        }

        Ok(LocalGitProject {
            meta: ProjectMeta {
                key: ProjectKey::new(&host, &display_path),
                display_path,
                origin: ProjectOrigin::LocalClone(root.clone()),
                ci_config_path: None,
            },
            root,
            override_default_branch: None,
            tree_cache: Mutex::new(Vec::new()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Apply a `glpv.toml` `[[projects]]` override (identity, branch, config path).
    pub fn apply_override(&mut self, ov: &crate::config::ProjectOverride) {
        if let Some(path) = &ov.path {
            let host = ov
                .host
                .clone()
                .unwrap_or_else(|| self.meta.key.host.clone());
            self.meta.key = ProjectKey::new(&host, path);
            self.meta.display_path = path.clone();
        } else if let Some(host) = &ov.host {
            self.meta.key = ProjectKey::new(host, &self.meta.display_path);
        }
        if let Some(b) = &ov.default_branch {
            self.override_default_branch = Some(b.clone());
        }
        if let Some(c) = &ov.ci_config_path {
            self.meta.ci_config_path = Some(c.clone());
        }
    }

    fn git(&self, args: &[&str]) -> Result<Option<String>, SourceError> {
        git_str(&self.root, args)
    }
}

impl ProjectSource for LocalGitProject {
    fn meta(&self) -> &ProjectMeta {
        &self.meta
    }

    fn default_branch(&self) -> Result<String, SourceError> {
        if let Some(b) = &self.override_default_branch {
            return Ok(b.clone());
        }
        if let Some(s) = self.git(&["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])?
            && let Some(b) = s.trim().strip_prefix("refs/remotes/origin/")
        {
            return Ok(b.to_string());
        }
        for candidate in ["main", "master"] {
            if self
                .git(&[
                    "rev-parse",
                    "--verify",
                    "-q",
                    &format!("refs/heads/{candidate}"),
                ])?
                .is_some()
            {
                return Ok(candidate.to_string());
            }
        }
        if let Some(s) = self.git(&["symbolic-ref", "-q", "--short", "HEAD"])? {
            return Ok(s.trim().to_string());
        }
        Ok("HEAD".to_string())
    }

    fn resolve_ref(&self, r: &str) -> Result<Option<Sha>, SourceError> {
        // Prefer the remote-tracking ref (the state GitLab would see), then
        // tags, then whatever git can resolve locally.
        let candidates = [
            format!("refs/remotes/origin/{r}"),
            format!("refs/tags/{r}"),
            r.to_string(),
        ];
        for c in candidates {
            let spec = format!("{c}^{{commit}}");
            if let Some(s) = self.git(&["rev-parse", "--verify", "-q", &spec])? {
                return Ok(Some(Sha(s.trim().to_string())));
            }
        }
        Ok(None)
    }

    fn read(&self, at: &TreeRef, path: &str) -> Result<Option<String>, SourceError> {
        match at {
            TreeRef::Worktree => {
                let full = self.root.join(path);
                match std::fs::read_to_string(&full) {
                    Ok(s) => Ok(Some(s)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) if full.is_dir() => {
                        let _ = e;
                        Ok(None)
                    }
                    Err(e) => Err(e.into()),
                }
            }
            TreeRef::Commit(sha) => Ok(self.git(&["show", &format!("{}:{path}", sha.0)])?),
        }
    }

    fn list_tree(&self, at: &TreeRef) -> Result<std::sync::Arc<[String]>, SourceError> {
        {
            let cache = self.tree_cache.lock().unwrap();
            if let Some((_, v)) = cache.iter().find(|(t, _)| t == at) {
                return Ok(v.clone());
            }
        }
        let listing = match at {
            TreeRef::Worktree => {
                // Tracked plus untracked-but-not-ignored: what a fresh commit would see.
                self.git(&["ls-files", "--cached", "--others", "--exclude-standard"])?
            }
            TreeRef::Commit(sha) => self.git(&["ls-tree", "-r", "--name-only", &sha.0])?,
        }
        .unwrap_or_default();
        let v: std::sync::Arc<[String]> = listing
            .lines()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .into();
        self.tree_cache
            .lock()
            .unwrap()
            .push((at.clone(), v.clone()));
        Ok(v)
    }

    fn tags(&self) -> Result<Vec<String>, SourceError> {
        Ok(self
            .git(&["tag", "--list"])?
            .unwrap_or_default()
            .lines()
            .map(|l| l.to_string())
            .collect())
    }
}

/// Run git in `dir`; `Ok(None)` for a clean non-zero exit (missing ref/file),
/// `Err` for real failures (git absent, not a repo, etc. are surfaced by rev-parse).
fn git_str(dir: &Path, args: &[&str]) -> Result<Option<String>, SourceError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| SourceError::Git(format!("failed to run git: {e}")))?;
    if out.status.success() {
        Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
    } else {
        Ok(None)
    }
}

/// Parse a git remote URL into `(host, project path)`.
///
/// Handles scp-like `git@host:group/proj.git`, `ssh://git@host[:port]/group/proj.git`
/// and `http(s)://host[/subpath]/group/proj.git`. The port is not part of the
/// host identity for project matching (GitLab instance == hostname).
pub fn parse_remote_url(url: &str) -> Option<(String, String)> {
    let url = url.trim();

    let strip_suffix = |p: &str| {
        let p = p.strip_suffix('/').unwrap_or(p);
        let p = p.strip_suffix(".git").unwrap_or(p);
        p.to_string()
    };

    if let Some(rest) = url.strip_prefix("ssh://") {
        let rest = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
        let (hostport, path) = rest.split_once('/')?;
        let host = hostport.split(':').next()?.to_lowercase();
        let path = strip_suffix(path);
        if path.is_empty() {
            return None;
        }
        return Some((host, path));
    }
    if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("git://"))
    {
        let rest = rest.split_once('@').map(|(_, r)| r).unwrap_or(rest);
        let (hostport, path) = rest.split_once('/')?;
        let host = hostport.split(':').next()?.to_lowercase();
        let path = strip_suffix(path);
        if path.is_empty() {
            return None;
        }
        return Some((host, path));
    }
    // scp-like: user@host:path (no scheme, single colon before the path)
    if !url.contains("://") {
        let rest = url.split_once('@').map(|(_, r)| r).unwrap_or(url);
        if let Some((host, path)) = rest.split_once(':')
            && !host.is_empty()
            && !path.starts_with('/')
            && path.contains('/')
        {
            return Some((host.to_lowercase(), strip_suffix(path)));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::parse_remote_url;

    #[test]
    fn remote_urls() {
        assert_eq!(
            parse_remote_url("git@gitlab.example.com:acme/arcwave.git"),
            Some(("gitlab.example.com".into(), "acme/arcwave".into()))
        );
        assert_eq!(
            parse_remote_url("ssh://git@gitlab.com:2222/group/sub/proj.git"),
            Some(("gitlab.com".into(), "group/sub/proj".into()))
        );
        assert_eq!(
            parse_remote_url("https://gitlab.com/group/proj.git"),
            Some(("gitlab.com".into(), "group/proj".into()))
        );
        assert_eq!(
            parse_remote_url("https://user:pass@gitlab.com/group/proj"),
            Some(("gitlab.com".into(), "group/proj".into()))
        );
        assert_eq!(
            parse_remote_url("git@github.com:me1iissa/isopod.git"),
            Some(("github.com".into(), "me1iissa/isopod".into()))
        );
        assert_eq!(parse_remote_url("/absolute/path/repo"), None);
        assert_eq!(parse_remote_url("file:///x/y"), None);
    }
}
