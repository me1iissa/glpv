//! Declarative fixture repositories. A `spec.toml` describes one project —
//! remote URL (the identity glpv indexes by), default branch and an ordered
//! list of commits with files, tags and branches — and is materialised into a
//! real git repository with deterministic commits (fixed author and dates, so
//! shas are stable across machines).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Materialise one spec into `<root>/<spec.dir>`, reusing it when the spec is
/// unchanged. Returns the clone directory.
pub fn materialize(spec_path: &Path, root: &Path) -> PathBuf {
    let spec_text = std::fs::read_to_string(spec_path)
        .unwrap_or_else(|e| panic!("{}: {e}", spec_path.display()));
    let spec: toml::Table = spec_text.parse().expect("spec.toml parses");

    let dir_name = spec["dir"].as_str().expect("dir");
    let default_branch = spec["default_branch"].as_str().expect("default_branch");

    let mut hasher = DefaultHasher::new();
    spec_text.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());

    let clone_dir = root.join(dir_name);
    let marker = clone_dir.join(".glpv-spec-hash");
    let marker_valid = |m: &Path| std::fs::read_to_string(m).ok().as_deref() == Some(hash.as_str());
    if marker_valid(&marker) {
        return clone_dir;
    }

    // Tests materialise fixtures concurrently — threads within one binary and
    // separate test binaries sharing a root — so building (and replacing a
    // stale build) must be mutually exclusive with anyone else building or
    // already reading the directory. `create_dir` is atomic on every platform
    // and works across processes; it is the lock. Once a valid marker exists
    // nobody touches the directory again, so readers that saw a valid marker
    // are never swapped out from under.
    let lock = root.join(format!(".lock-{dir_name}"));
    let started = std::time::Instant::now();
    loop {
        if marker_valid(&marker) {
            return clone_dir;
        }
        match std::fs::create_dir(&lock) {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if started.elapsed().as_secs() > 120 {
                    // a crashed builder left the lock behind; steal it
                    let _ = std::fs::remove_dir(&lock);
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(e) => panic!("cannot lock fixture {}: {e}", lock.display()),
        }
    }
    let result = (|| {
        if marker_valid(&marker) {
            return clone_dir.clone();
        }
        std::fs::remove_dir_all(&clone_dir)
            .or_else(|e| if clone_dir.exists() { Err(e) } else { Ok(()) })
            .unwrap_or_else(|e| panic!("cannot clear stale fixture {}: {e}", clone_dir.display()));
        std::fs::create_dir_all(&clone_dir).unwrap();
        build_into(&clone_dir, &spec, default_branch, &hash);
        clone_dir.clone()
    })();
    let _ = std::fs::remove_dir(&lock);
    result
}

fn build_into(clone_dir: &Path, spec: &toml::Table, default_branch: &str, hash: &str) {
    let remote = spec["remote"].as_str().expect("remote");
    git(clone_dir, &["init", "-q", "-b", default_branch]);
    git(&clone_dir, &["config", "commit.gpgsign", "false"]);
    git(&clone_dir, &["remote", "add", "origin", remote]);
    // The marker must not become part of the fixture's tree.
    std::fs::write(clone_dir.join(".git/info/exclude"), ".glpv-spec-hash\n").unwrap();

    let commits = spec["commits"].as_array().expect("commits");
    for commit in commits {
        let commit = commit.as_table().unwrap();
        if let Some(checkout) = commit.get("checkout").and_then(|c| c.as_str()) {
            git(&clone_dir, &["checkout", "-q", checkout]);
        }
        if let Some(files) = commit.get("files").and_then(|f| f.as_table()) {
            for (path, content) in files {
                let full = clone_dir.join(path);
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                std::fs::write(&full, content.as_str().expect("file content is a string")).unwrap();
            }
        }
        git(&clone_dir, &["add", "-A"]);
        git(
            &clone_dir,
            &[
                "commit",
                "-q",
                "-m",
                commit
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("fixture"),
            ],
        );
        if let Some(tags) = commit.get("tags").and_then(|t| t.as_array()) {
            for tag in tags {
                git(&clone_dir, &["tag", tag.as_str().unwrap()]);
            }
        }
        if let Some(branch) = commit.get("branch").and_then(|b| b.as_str()) {
            git(&clone_dir, &["branch", "-f", branch]);
        }
    }
    git(&clone_dir, &["checkout", "-q", default_branch]);

    std::fs::write(clone_dir.join(".glpv-spec-hash"), hash).unwrap();
}

/// Materialise every `*.toml` (or `<name>/spec.toml`) under `specs` into `root`.
pub fn build_set(spec_paths: &[PathBuf], root: &Path) -> Vec<PathBuf> {
    std::fs::create_dir_all(root).unwrap();
    spec_paths.iter().map(|p| materialize(p, root)).collect()
}

/// Collect spec files from a directory: `<dir>/*.toml` plus `<dir>/*/spec.toml`.
pub fn collect_specs(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "toml") {
                out.push(p);
            } else if p.is_dir() && p.join("spec.toml").exists() {
                out.push(p.join("spec.toml"));
            }
        }
    }
    out.sort();
    out
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.com")
        .env("GIT_AUTHOR_DATE", "2024-01-01T00:00:00 +0000")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.com")
        .env("GIT_COMMITTER_DATE", "2024-01-01T00:00:00 +0000")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}
