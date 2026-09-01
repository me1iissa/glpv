//! `--clone-missing`: back-fill the clones folder with bare, blobless clones
//! of every project the scan read through the API, so the next run can be
//! offline. The index picks `<root>/.glpv-clones/<host>/<path>.git` up on
//! its own; the files the scan read are fetched into the clone right away
//! (a blobless clone otherwise fetches blobs lazily, which needs the network).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use glpv_core::source::ProjectSource;
use glpv_core::source::index::CLONES_DIR;

use super::IndexSetup;

pub fn clone_missing(setup: &IndexSetup) -> anyhow::Result<()> {
    let Some(api) = &setup.api else {
        eprintln!("warn  [clone.skipped] --clone-missing needs --api <host>");
        return Ok(());
    };
    let Some(root) = setup.roots.first() else {
        anyhow::bail!(
            "--clone-missing needs a --projects <dir> (or defaults.projects in glpv.toml) to \
             clone into"
        );
    };
    let projects = api.locator.resolved();
    if projects.is_empty() {
        println!("nothing to clone: no project was read through the API");
        return Ok(());
    }
    let auth = api.client.git_auth_header();
    for p in projects {
        let meta = p.meta();
        let name = format!("{}/{}", meta.key.host, meta.display_path);
        let dest = root
            .join(CLONES_DIR)
            .join(&meta.key.host)
            .join(format!("{}.git", meta.display_path));
        let url = api.client.git_http_url(&meta.display_path);
        let fresh = !dest.join("HEAD").is_file();
        let result = if fresh {
            clone(&url, &dest, auth.as_deref())
        } else {
            fetch(&dest, auth.as_deref())
        };
        if let Err(e) = result {
            eprintln!("warn  [clone.failed] {name}: {e}");
            continue;
        }
        let reads = p.reads();
        if let Err(e) = warm(&dest, &reads, auth.as_deref()) {
            eprintln!("warn  [clone.incomplete] {name}: {e}");
        }
        println!(
            "{} {name} → {} ({} file(s) fetched)",
            if fresh { "cloned" } else { "updated" },
            dest.display(),
            reads.len()
        );
    }
    Ok(())
}

fn clone(url: &str, dest: &Path, auth: Option<&str>) -> anyhow::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    git(
        None,
        &[
            "clone",
            "--quiet",
            "--bare",
            "--filter=blob:none",
            url,
            &dest.display().to_string(),
        ],
        auth,
        None,
    )
}

fn fetch(dest: &Path, auth: Option<&str>) -> anyhow::Result<()> {
    git(
        Some(dest),
        &[
            "fetch",
            "--quiet",
            "origin",
            "+refs/heads/*:refs/heads/*",
            "+refs/tags/*:refs/tags/*",
        ],
        auth,
        None,
    )
}

/// Pull the blobs the scan read into the promisor clone.
fn warm(dest: &Path, reads: &[(String, String)], auth: Option<&str>) -> anyhow::Result<()> {
    if reads.is_empty() {
        return Ok(());
    }
    let mut input = String::new();
    for (sha, path) in reads {
        input.push_str(sha);
        input.push(':');
        input.push_str(path);
        input.push('\n');
    }
    git(
        Some(dest),
        &["cat-file", "--batch"],
        auth,
        Some(input.as_bytes()),
    )
}

/// Run git with the token, if any, passed as configuration through the
/// environment — never on the command line, never persisted in the clone.
fn git(
    cwd: Option<&Path>,
    args: &[&str],
    auth: Option<&str>,
    stdin: Option<&[u8]>,
) -> anyhow::Result<()> {
    let mut cmd = Command::new("git");
    if let Some(d) = cwd {
        cmd.arg("-C").arg(d);
    }
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    if let Some(h) = auth {
        cmd.env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", h);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot run git: {e}"))?;
    if let Some(bytes) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        let _ = pipe.write_all(bytes);
    }
    let out = child.wait_with_output()?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let reason = stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("git failed");
    anyhow::bail!("git {}: {reason}", args[0])
}
