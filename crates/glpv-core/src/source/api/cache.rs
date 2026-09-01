//! The on-disk cache behind the API source.
//!
//! Everything keyed by a commit sha is immutable and cached forever; whatever
//! can move (ref → sha, tags, releases, project metadata, instance templates,
//! remote include bodies) is cached with a short TTL that `--refresh`
//! bypasses. Errors are never cached: a missing entry always means "ask".
//!
//! Layout under `$XDG_CACHE_HOME/glpv` (default `~/.cache/glpv`):
//!
//! ```text
//! <host>/paths/<project path>.json            project metadata          (TTL)
//! <host>/<project id>/refs/<ref>.json         ref → sha                 (TTL)
//! <host>/<project id>/tags.json               tag names                 (TTL)
//! <host>/<project id>/releases.json           release tag names         (TTL)
//! <host>/<project id>/blobs/<sha>/<path>      file content              (immutable)
//! <host>/<project id>/tree/<sha>.json         recursive file listing    (immutable)
//! <host>/<project id>/tree/<sha>/<dir>.json   one subtree of it         (immutable)
//! <host>/<project id>/compare/<base>_<head>.json  changed files         (immutable)
//! <host>/templates/<key>.yml                  instance CI templates     (TTL)
//! remote/<sha256 of the url>                  include:remote bodies     (TTL)
//! ```
//!
//! Path components that come from the API (`<ref>`, `<path>`, `<key>`) are
//! percent-encoded where a file name needs it (see [`encode_segment`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// How long a mutable entry (ref, tags, releases, project metadata,
/// templates, remote bodies) is trusted before it is fetched again.
pub const TTL: Duration = Duration::from_secs(10 * 60);

pub struct ApiCache {
    root: PathBuf,
    /// Treat every TTL entry as expired (`--refresh`); writes still happen.
    refresh: bool,
}

#[derive(Serialize, Deserialize)]
struct Dated<T> {
    fetched_at: u64,
    value: T,
}

impl ApiCache {
    pub fn new(root: PathBuf, refresh: bool) -> Self {
        ApiCache { root, refresh }
    }

    /// `$XDG_CACHE_HOME/glpv`, else `~/.cache/glpv`, else a temp directory.
    pub fn default_root() -> PathBuf {
        if let Some(x) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(x).join("glpv");
        }
        if let Some(h) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(h).join(".cache/glpv");
        }
        std::env::temp_dir().join("glpv-cache")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn refresh(&self) -> bool {
        self.refresh
    }

    // ----- immutable entries ------------------------------------------------

    pub fn get_bytes(&self, rel: &Path) -> Option<Vec<u8>> {
        std::fs::read(self.root.join(rel)).ok()
    }

    pub fn put_bytes(&self, rel: &Path, bytes: &[u8]) {
        let _ = self.write_atomic(rel, bytes);
    }

    pub fn get_json<T: DeserializeOwned>(&self, rel: &Path) -> Option<T> {
        let bytes = self.get_bytes(rel)?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn put_json<T: Serialize>(&self, rel: &Path, value: &T) {
        if let Ok(bytes) = serde_json::to_vec(value) {
            self.put_bytes(rel, &bytes);
        }
    }

    // ----- TTL entries ------------------------------------------------------

    /// A dated entry younger than [`TTL`] (unless refreshing).
    pub fn get_fresh<T: DeserializeOwned>(&self, rel: &Path) -> Option<T> {
        self.get_fresh_at(rel, now_secs())
    }

    fn get_fresh_at<T: DeserializeOwned>(&self, rel: &Path, now: u64) -> Option<T> {
        if self.refresh {
            return None;
        }
        let dated: Dated<T> = self.get_json(rel)?;
        // A clock that went backwards makes the entry look younger; harmless.
        (now.saturating_sub(dated.fetched_at) < TTL.as_secs()).then_some(dated.value)
    }

    pub fn put_dated<T: Serialize>(&self, rel: &Path, value: &T) {
        self.put_dated_at(rel, value, now_secs());
    }

    fn put_dated_at<T: Serialize>(&self, rel: &Path, value: &T, now: u64) {
        self.put_json(
            rel,
            &Dated {
                fetched_at: now,
                value,
            },
        );
    }

    fn write_atomic(&self, rel: &Path, bytes: &[u8]) -> std::io::Result<()> {
        let full = self.root.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A sibling temp file then a rename: readers never see a torn entry
        // even when two scans share the cache.
        let tmp = full.with_extension(format!(
            "{}.tmp-{}",
            full.extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default(),
            std::process::id()
        ));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &full)
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Percent-encode one path component so it is a safe, portable file name:
/// `/`, `%`, `\`, the characters Windows rejects, controls and the two dot
/// names are escaped; everything else is kept readable.
pub fn encode_segment(s: &str) -> String {
    if s == "." || s == ".." || s.is_empty() {
        return s.bytes().map(|b| format!("%{b:02X}")).collect();
    }
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'/' | b'\\' | b'%' | b':' | b'*' | b'?' | b'"' | b'<' | b'>' | b'|' => {
                out.push_str(&format!("%{b:02X}"));
            }
            0..=0x1f | 0x7f => out.push_str(&format!("%{b:02X}")),
            _ => out.push(b as char),
        }
    }
    out
}

/// A repository-relative file path as a cache path: every segment encoded,
/// directory structure kept.
pub fn encode_path(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        out.push(encode_segment(seg));
    }
    out
}

// ----- entry paths ------------------------------------------------------------

pub fn project_meta_path(host: &str, project_path: &str) -> PathBuf {
    Path::new(host)
        .join("paths")
        .join(format!("{}.json", encode_segment(project_path)))
}

pub fn ref_path(host: &str, project_id: u64, r: &str) -> PathBuf {
    Path::new(host)
        .join(project_id.to_string())
        .join("refs")
        .join(format!("{}.json", encode_segment(r)))
}

pub fn tags_path(host: &str, project_id: u64) -> PathBuf {
    Path::new(host)
        .join(project_id.to_string())
        .join("tags.json")
}

pub fn releases_path(host: &str, project_id: u64) -> PathBuf {
    Path::new(host)
        .join(project_id.to_string())
        .join("releases.json")
}

pub fn blob_path(host: &str, project_id: u64, sha: &str, path: &str) -> PathBuf {
    Path::new(host)
        .join(project_id.to_string())
        .join("blobs")
        .join(encode_segment(sha))
        .join(encode_path(path))
}

/// The whole tree at `sha`, or (`prefix`) one of its subtrees.
pub fn tree_path(host: &str, project_id: u64, sha: &str, prefix: Option<&str>) -> PathBuf {
    let dir = Path::new(host).join(project_id.to_string()).join("tree");
    match prefix {
        None => dir.join(format!("{}.json", encode_segment(sha))),
        Some(p) => dir
            .join(encode_segment(sha))
            .join(format!("{}.json", encode_segment(p))),
    }
}

pub fn compare_path(host: &str, project_id: u64, base: &str, head: &str) -> PathBuf {
    Path::new(host)
        .join(project_id.to_string())
        .join("compare")
        .join(format!(
            "{}_{}.json",
            encode_segment(base),
            encode_segment(head)
        ))
}

pub fn template_path(host: &str, key: &str) -> PathBuf {
    Path::new(host)
        .join("templates")
        .join(format!("{}.yml", encode_segment(key)))
}

pub fn remote_path(url: &str) -> PathBuf {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(url.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Path::new("remote").join(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_are_safe_file_names() {
        assert_eq!(encode_segment("main"), "main");
        assert_eq!(encode_segment("refs/tags/v1.0"), "refs%2Ftags%2Fv1.0");
        assert_eq!(encode_segment("a:b*c"), "a%3Ab%2Ac");
        assert_eq!(encode_segment(".."), "%2E%2E");
        assert_eq!(encode_segment("."), "%2E");
        assert_eq!(encode_segment("100%"), "100%25");
        assert_eq!(
            encode_path("/.gitlab/ci/../x.yml"),
            PathBuf::from(".gitlab/ci/%2E%2E/x.yml")
        );
    }

    #[test]
    fn layout() {
        assert_eq!(
            blob_path("gitlab.example.com", 42, "abc", ".gitlab/ci/x.yml"),
            PathBuf::from("gitlab.example.com/42/blobs/abc/.gitlab/ci/x.yml")
        );
        assert_eq!(
            tree_path("gitlab.example.com", 42, "abc", None),
            PathBuf::from("gitlab.example.com/42/tree/abc.json")
        );
        assert_eq!(
            tree_path("gitlab.example.com", 42, "abc", Some(".gitlab/ci")),
            PathBuf::from("gitlab.example.com/42/tree/abc/.gitlab%2Fci.json")
        );
        assert_eq!(
            ref_path("gitlab.example.com", 42, "refs/tags/v1"),
            PathBuf::from("gitlab.example.com/42/refs/refs%2Ftags%2Fv1.json")
        );
        assert_eq!(
            project_meta_path("gitlab.example.com", "acme/api"),
            PathBuf::from("gitlab.example.com/paths/acme%2Fapi.json")
        );
        assert_eq!(
            compare_path("h", 1, "a", "b"),
            PathBuf::from("h/1/compare/a_b.json")
        );
        assert_eq!(
            template_path("h", "Jobs/Build"),
            PathBuf::from("h/templates/Jobs%2FBuild.yml")
        );
        let r = remote_path("https://gitlab.example.com/x.yml");
        assert!(r.starts_with("remote"));
        assert_eq!(r.file_name().unwrap().len(), 64);
        assert_ne!(r, remote_path("https://gitlab.example.com/y.yml"));
    }

    #[test]
    fn ttl_entries_expire_and_refresh_bypasses_them() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ApiCache::new(tmp.path().to_path_buf(), false);
        let rel = ref_path("h", 1, "main");
        assert!(cache.get_fresh_at::<String>(&rel, 1_000).is_none());

        cache.put_dated_at(&rel, &"abc".to_string(), 1_000);
        assert_eq!(
            cache.get_fresh_at::<String>(&rel, 1_000 + TTL.as_secs() - 1),
            Some("abc".to_string())
        );
        assert!(
            cache
                .get_fresh_at::<String>(&rel, 1_000 + TTL.as_secs())
                .is_none()
        );

        let refreshing = ApiCache::new(tmp.path().to_path_buf(), true);
        assert!(refreshing.get_fresh_at::<String>(&rel, 1_000).is_none());
        // ...but it still writes.
        refreshing.put_dated_at(&rel, &"def".to_string(), 2_000);
        assert_eq!(
            cache.get_fresh_at::<String>(&rel, 2_000),
            Some("def".to_string())
        );
    }

    #[test]
    fn immutable_entries_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = ApiCache::new(tmp.path().to_path_buf(), true);
        let rel = blob_path("h", 7, "deadbeef", "dir/file.yml");
        assert!(cache.get_bytes(&rel).is_none());
        cache.put_bytes(&rel, b"stages: [a]\n");
        assert_eq!(
            cache.get_bytes(&rel).as_deref(),
            Some(&b"stages: [a]\n"[..])
        );
        assert!(tmp.path().join("h/7/blobs/deadbeef/dir/file.yml").is_file());
        // No temp file is left behind.
        let entries: Vec<_> = std::fs::read_dir(tmp.path().join("h/7/blobs/deadbeef/dir"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("file.yml")]);

        let tree = tree_path("h", 7, "deadbeef", None);
        cache.put_json(&tree, &vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            cache.get_json::<Vec<String>>(&tree),
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }
}
