//! Local git clones as a [`ProjectSource`], via the `git` subprocess.
//!
//! Files are read at a ref with `git show <sha>:<path>` — no checkout — and
//! the working tree is a first-class tree so `glpv scan --file` reflects
//! uncommitted edits.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use super::{ProjectKey, ProjectMeta, ProjectOrigin, ProjectSource, Sha, SourceError, TreeRef};

/// `(base, head)` → changed files (`None` = base unresolvable).
type DiffCacheEntry = ((String, TreeRef), Option<std::sync::Arc<[String]>>);

pub struct LocalGitProject {
    meta: ProjectMeta,
    root: PathBuf,
    /// A bare repository (`git clone --bare`, e.g. from `--clone-missing`):
    /// no working tree, `HEAD` names the default branch.
    bare: bool,
    override_default_branch: Option<String>,
    tree_cache: Mutex<Vec<(TreeRef, std::sync::Arc<[String]>)>>,
    diff_cache: Mutex<Vec<DiffCacheEntry>>,
}

impl LocalGitProject {
    /// Open the repository containing `dir` (a working tree or a bare
    /// repository). Project identity comes from the `origin` remote URL when
    /// parseable, else from the directory name.
    pub fn open(dir: &Path) -> Result<Self, SourceError> {
        let (root, bare) = match git_str(dir, &["rev-parse", "--show-toplevel"])? {
            Some(top) => (PathBuf::from(top.trim()), false),
            None => {
                let is_bare = git_str(dir, &["rev-parse", "--is-bare-repository"])?
                    .is_some_and(|s| s.trim() == "true");
                let git_dir = if is_bare {
                    git_str(dir, &["rev-parse", "--absolute-git-dir"])?
                } else {
                    None
                };
                match git_dir {
                    Some(d) => (PathBuf::from(d.trim()), true),
                    None => return Err(SourceError::NotAGitRepo(dir.to_path_buf())),
                }
            }
        };

        let mut host = String::from("local");
        let mut display_path = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .map(|n| match bare {
                true => n.strip_suffix(".git").unwrap_or(&n).to_string(),
                false => n,
            })
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
            bare,
            override_default_branch: None,
            tree_cache: Mutex::new(Vec::new()),
            diff_cache: Mutex::new(Vec::new()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn is_bare(&self) -> bool {
        self.bare
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

    fn compute_changed_files(
        &self,
        base: &str,
        head: &TreeRef,
    ) -> Result<Option<std::sync::Arc<[String]>>, SourceError> {
        let Some(base_sha) = self.resolve_ref(base)? else {
            return Ok(None);
        };
        let nul_split = |s: String| -> Vec<String> {
            s.split('\0')
                .filter(|p| !p.is_empty())
                .map(|p| p.to_string())
                .collect()
        };
        let mut files: Vec<String> = Vec::new();
        match head {
            TreeRef::Commit(sha) => {
                // Three-dot: the merge base of both, like a branch push / MR diff.
                let range = format!("{}...{}", base_sha.0, sha.0);
                let Some(out) = self.git(&["diff", "--name-only", "--no-renames", "-z", &range])?
                else {
                    return Ok(None);
                };
                files.extend(nul_split(out));
            }
            TreeRef::Worktree => {
                let merge_base = self
                    .git(&["merge-base", &base_sha.0, "HEAD"])?
                    .map(|s| s.trim().to_string())
                    .unwrap_or(base_sha.0);
                let Some(out) =
                    self.git(&["diff", "--name-only", "--no-renames", "-z", &merge_base])?
                else {
                    return Ok(None);
                };
                files.extend(nul_split(out));
                // Untracked-but-not-ignored files count as added, consistent
                // with `list_tree` for the working tree.
                if let Some(out) =
                    self.git(&["ls-files", "--others", "--exclude-standard", "-z"])?
                {
                    files.extend(nul_split(out));
                }
            }
        }
        Ok(Some(files.into()))
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
        // A bare clone's HEAD is the upstream default branch, not a checkout.
        if self.bare
            && let Some(s) = self.git(&["symbolic-ref", "-q", "HEAD"])?
            && let Some(b) = s.trim().strip_prefix("refs/heads/")
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

    fn changed_files(
        &self,
        base: &str,
        head: &TreeRef,
    ) -> Result<Option<std::sync::Arc<[String]>>, SourceError> {
        let key = (base.to_string(), head.clone());
        {
            let cache = self.diff_cache.lock().unwrap();
            if let Some((_, v)) = cache.iter().find(|(k, _)| *k == key) {
                return Ok(v.clone());
            }
        }
        let result = self.compute_changed_files(base, head)?;
        self.diff_cache.lock().unwrap().push((key, result.clone()));
        Ok(result)
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
    use super::*;

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
    fn changed_files_is_the_push_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        run(dir, &["init", "-q", "-b", "main"]);
        run(dir, &["config", "commit.gpgsign", "false"]);
        for f in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(dir.join(f), f).unwrap();
        }
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "-q", "-m", "one"]);
        run(dir, &["tag", "base"]);
        // Diverging side branch: the merge base, not the tip, must be used.
        run(dir, &["checkout", "-q", "-b", "side"]);
        std::fs::write(dir.join("side.txt"), "s").unwrap();
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "-q", "-m", "side"]);
        run(dir, &["checkout", "-q", "main"]);
        std::fs::write(dir.join("a.txt"), "changed").unwrap();
        run(dir, &["mv", "b.txt", "d.txt"]);
        std::fs::write(dir.join("e.txt"), "new").unwrap();
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "-q", "-m", "two"]);
        std::fs::write(dir.join("untracked.txt"), "u").unwrap();

        let project = LocalGitProject::open(dir).unwrap();
        let head = project.resolve_ref("main").unwrap().unwrap();

        let committed = project
            .changed_files("side", &TreeRef::Commit(head.clone()))
            .unwrap()
            .expect("side resolves");
        assert_eq!(&*committed, &["a.txt", "b.txt", "d.txt", "e.txt"]);
        let vs_tag = project
            .changed_files("base", &TreeRef::Commit(head))
            .unwrap()
            .unwrap();
        assert_eq!(&*vs_tag, &["a.txt", "b.txt", "d.txt", "e.txt"]);

        let worktree = project
            .changed_files("side", &TreeRef::Worktree)
            .unwrap()
            .unwrap();
        assert_eq!(
            &*worktree,
            &["a.txt", "b.txt", "d.txt", "e.txt", "untracked.txt"]
        );

        assert!(
            project
                .changed_files("no-such-ref", &TreeRef::Worktree)
                .unwrap()
                .is_none()
        );
    }

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
