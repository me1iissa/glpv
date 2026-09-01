//! `glpv check` — the oracle: resolve a project locally, ask the GitLab
//! server to lint the same entry file at the same ref (`merged_yaml` +
//! `include_jobs`), and report every difference. Exit 0 when identical, 1 on
//! differences, 2 when the check could not be carried out.
//!
//! `--pipeline` is the token-free variant for CI: the oracle is the pipeline
//! the job runs in — the jobs the server actually created, read with the
//! job's own `CI_JOB_TOKEN` — and the scenario is the real one (source, ref,
//! tag, diff base) from the CI environment. Scripts are not compared (the
//! Jobs API does not carry them); everything else is.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use glpv_cli::check::{self, Oracle};
use glpv_core::resolve::ResolveOpts;
use glpv_core::util::node_to_json;

#[derive(clap::Args)]
pub struct CheckArgs {
    /// Entry `.gitlab-ci.yml` (the project is identified from the clone's remote).
    #[arg(long, conflicts_with = "entry")]
    pub file: Option<PathBuf>,
    /// Entry project path (e.g. `acme/api`), located via the index.
    #[arg(long)]
    pub entry: Option<String>,
    /// Ref to resolve and to lint against (default: the default branch).
    #[arg(long = "ref")]
    pub git_ref: Option<String>,
    #[command(flatten)]
    pub index: super::IndexArgs,
    #[command(flatten)]
    pub scenario: super::ScenarioArgs,
    /// How to reach the server: `glab` shells out to an authenticated `glab api`;
    /// `curl` sends the token in GLPV_TOKEN (or GITLAB_TOKEN) as PRIVATE-TOKEN.
    #[arg(long = "api-transport", value_enum, default_value_t = Transport::Glab)]
    pub transport: Transport,
    /// API base URL (default: https://<project host>/api/v4).
    #[arg(long)]
    pub api_url: Option<String>,
    /// Use a saved lint response instead of calling the server.
    #[arg(long, conflicts_with = "save_oracle")]
    pub oracle_json: Option<PathBuf>,
    /// Save the server's response here (reusable with --oracle-json).
    #[arg(long)]
    pub save_oracle: Option<PathBuf>,
    /// Compare only the merged configuration, not the job list.
    #[arg(long)]
    pub merged_only: bool,
    /// Compare against the pipeline this job runs in (or --pipeline-id): the
    /// jobs the server created, read with the job token. Needs no other
    /// credential; source, ref, tag and diff base come from the CI environment.
    #[arg(long, conflicts_with_all = ["entry", "oracle_json", "merged_only"])]
    pub pipeline: bool,
    /// The pipeline to compare against (default: $CI_PIPELINE_ID).
    #[arg(long, requires = "pipeline")]
    pub pipeline_id: Option<u64>,
    /// Its project, id or path (default: $CI_PROJECT_ID).
    #[arg(long, requires = "pipeline")]
    pub project_id: Option<String>,
    /// Use a saved pipeline snapshot (written by --save-oracle in --pipeline mode).
    #[arg(long, requires = "pipeline")]
    pub pipeline_json: Option<PathBuf>,
}

/// What `--save-oracle` stores in `--pipeline` mode.
#[derive(serde::Serialize, serde::Deserialize)]
struct PipelineSnapshot {
    pipeline: serde_json::Value,
    jobs: Vec<check::PipelineJob>,
    bridges: Vec<check::PipelineJob>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub enum Transport {
    Glab,
    Curl,
}

pub fn run(args: CheckArgs) -> anyhow::Result<()> {
    std::process::exit(match run_inner(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            2
        }
    })
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn root_pipeline(
    output: &glpv_core::scan::ScanOutput,
) -> anyhow::Result<&glpv_core::model::Pipeline> {
    output
        .graph
        .pipelines
        .iter()
        .find(|p| p.kind == glpv_core::model::PipelineKind::Root)
        .or_else(|| output.graph.pipelines.first())
        .ok_or_else(|| anyhow::anyhow!("no pipeline was resolved"))
}

/// Print the job comparison; 0 when clean, 1 otherwise.
fn report_jobs(
    root: &glpv_core::model::Pipeline,
    server_jobs: &[check::OracleJob],
    fields: check::CompareFields,
    server_label: &str,
) -> i32 {
    use glpv_core::model::Outcome;
    let local_jobs = check::local_jobs(root);
    let r = check::compare_jobs_with(&local_jobs, server_jobs, fields);
    let expected = local_jobs
        .iter()
        .filter(|j| {
            matches!(
                j.outcome,
                Outcome::Runs | Outcome::Manual | Outcome::Delayed
            )
        })
        .count();
    println!(
        "jobs: server {server_label} {}; local expects {} to run ({} undecided){}",
        server_jobs.len(),
        expected,
        r.undecided.len(),
        if r.is_clean() { " — identical" } else { "" }
    );
    for j in &r.missing_on_server {
        println!("  local runs, server does not create: {j}");
    }
    for j in &r.unexpected_on_server {
        println!("  server creates, local skips or lacks: {j}");
    }
    for (job, field, a, b) in &r.field_mismatches {
        println!(
            "  {job}: {field} differs\n    local:  {}\n    server: {}",
            oneline(a),
            oneline(b)
        );
    }
    if !r.undecided.is_empty() {
        println!(
            "  undecided locally (unknown variables): {}",
            r.undecided.join(", ")
        );
    }
    if r.is_clean() { 0 } else { 1 }
}

/// `--pipeline`: the oracle is the pipeline this job runs in.
fn run_pipeline(args: CheckArgs) -> anyhow::Result<i32> {
    use glpv_core::diff::DiffSpec;

    // The real scenario, from the CI environment; --var still adds to it.
    let mut sargs = args.scenario.clone();
    if let Some(s) = env_nonempty("CI_PIPELINE_SOURCE") {
        sargs.source = s;
    }
    if let Some(r) = env_nonempty("CI_COMMIT_REF_NAME") {
        sargs.sim_ref = Some(r);
    }
    if env_nonempty("CI_COMMIT_TAG").is_some() {
        sargs.tag = true;
    }
    let scenario = sargs.to_scenario()?;
    // The push diff: the merge request's base, else the previous head of the
    // branch. A new branch (all-zero before sha) has no diff, as in GitLab.
    let diff_base = env_nonempty("CI_MERGE_REQUEST_DIFF_BASE_SHA")
        .or_else(|| env_nonempty("CI_COMMIT_BEFORE_SHA").filter(|s| s.chars().any(|c| c != '0')));
    let file = args.file.clone().unwrap_or_else(|| {
        let dir = env_nonempty("CI_PROJECT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        dir.join(env_nonempty("CI_CONFIG_PATH").unwrap_or_else(|| ".gitlab-ci.yml".to_string()))
    });
    let opts = ResolveOpts {
        max_pipelines: 1,
        diff: diff_base.clone().map(DiffSpec::Base),
        ..ResolveOpts::default()
    };
    let scan_args = super::scan::ScanArgs {
        file: Some(file),
        entry: None,
        all: false,
        git_ref: None,
        config_path: None,
        index: args.index.clone(),
        scenario: sargs.clone(),
        out: PathBuf::new(),
        format: String::new(),
        no_embed_sources: false,
        full_provenance: false,
        allow_remote: false,
        max_pipelines: 1,
        diff: diff_base.clone(),
        changed_files: vec![],
        inputs: vec![],
        clone_missing: false,
    };
    let (output, _setup) = super::scan::run_scan(&scan_args, &scenario, &opts, vec![])?;
    if output.merged_root.is_none() {
        super::print_diagnostics(&output.graph);
        anyhow::bail!("the configuration could not be resolved locally");
    }
    let root = root_pipeline(&output)?;

    let snapshot: PipelineSnapshot = match &args.pipeline_json {
        Some(p) => serde_json::from_str(&std::fs::read_to_string(p)?)?,
        None => fetch_pipeline(&args)?,
    };
    if let Some(p) = &args.save_oracle {
        std::fs::write(p, serde_json::to_string_pretty(&snapshot)?)?;
    }
    let field = |k: &str| {
        snapshot
            .pipeline
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string()
    };
    println!(
        "glpv check --pipeline: {} pipeline #{} — source {}, ref {}{}{}",
        root.project.path,
        snapshot
            .pipeline
            .get("id")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_string()),
        field("source"),
        field("ref"),
        if scenario.is_tag { " (tag)" } else { "" },
        match &diff_base {
            Some(b) => format!(", changes since {}", &b[..b.len().min(12)]),
            None => ", no diff (new branch or tag)".to_string(),
        }
    );
    let server =
        check::pipeline_jobs_to_oracle(snapshot.jobs.iter().chain(snapshot.bridges.iter()));
    let exit = report_jobs(root, &server, check::CompareFields::PIPELINE, "created");
    super::print_diagnostics(&output.graph);
    Ok(exit)
}

fn fetch_pipeline(args: &CheckArgs) -> anyhow::Result<PipelineSnapshot> {
    let base = args
        .api_url
        .clone()
        .or_else(|| env_nonempty("CI_API_V4_URL"))
        .ok_or_else(|| {
            anyhow::anyhow!("pass --api-url or run inside a GitLab CI job (CI_API_V4_URL)")
        })?;
    let project = args
        .project_id
        .clone()
        .or_else(|| env_nonempty("CI_PROJECT_ID"))
        .ok_or_else(|| {
            anyhow::anyhow!("pass --project-id or run inside a GitLab CI job (CI_PROJECT_ID)")
        })?;
    let pipeline = args
        .pipeline_id
        .map(|v| v.to_string())
        .or_else(|| env_nonempty("CI_PIPELINE_ID"))
        .ok_or_else(|| {
            anyhow::anyhow!("pass --pipeline-id or run inside a GitLab CI job (CI_PIPELINE_ID)")
        })?;
    let auth = if let Some(t) = env_nonempty("CI_JOB_TOKEN") {
        format!("JOB-TOKEN: {t}")
    } else if let Some(t) = env_nonempty("GLPV_TOKEN").or_else(|| env_nonempty("GITLAB_TOKEN")) {
        format!("PRIVATE-TOKEN: {t}")
    } else {
        anyhow::bail!("no credential: run inside a CI job (CI_JOB_TOKEN) or set GLPV_TOKEN");
    };
    let get = |path: &str| -> anyhow::Result<String> {
        let url = format!(
            "{}/projects/{}/pipelines/{}{}",
            base.trim_end_matches('/'),
            encode_path(&project),
            pipeline,
            path
        );
        curl_get(&url, &auth)
    };
    let pipeline_json: serde_json::Value = serde_json::from_str(&get("")?)?;
    if let Some(m) = pipeline_json.get("message") {
        anyhow::bail!("the server answered with an error: {m}");
    }
    let mut jobs = Vec::new();
    let mut bridges = Vec::new();
    for (kind, out) in [("jobs", &mut jobs), ("bridges", &mut bridges)] {
        for page in 1..=100u32 {
            let raw = get(&format!("/{kind}?per_page=100&page={page}"))?;
            let batch: Vec<check::PipelineJob> = serde_json::from_str(&raw).map_err(|e| {
                anyhow::anyhow!("unexpected {kind} response ({e}): {}", oneline(&raw))
            })?;
            let n = batch.len();
            out.extend(batch);
            if n < 100 {
                break;
            }
        }
    }
    Ok(PipelineSnapshot {
        pipeline: pipeline_json,
        jobs,
        bridges,
    })
}

fn curl_get(url: &str, auth_header: &str) -> anyhow::Result<String> {
    let out = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            "-H",
            auth_header,
            url,
        ])
        .output()
        .map_err(|e| anyhow::anyhow!("cannot run curl: {e}"))?;
    let body = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        anyhow::bail!(
            "GET {url}: {}{}",
            String::from_utf8_lossy(&out.stderr).trim(),
            if body.is_empty() {
                String::new()
            } else {
                format!(" — {}", oneline(&body))
            }
        );
    }
    Ok(body)
}

fn run_inner(args: CheckArgs) -> anyhow::Result<i32> {
    if args.pipeline {
        return run_pipeline(args);
    }
    let scenario = args.scenario.to_scenario()?;
    let opts = ResolveOpts {
        max_pipelines: 1,
        ..ResolveOpts::default()
    };
    let scan_args = super::scan::ScanArgs {
        file: args.file.clone(),
        entry: args.entry.clone(),
        all: false,
        git_ref: args.git_ref.clone(),
        config_path: None,
        index: args.index.clone(),
        scenario: args.scenario.clone(),
        out: PathBuf::new(),
        format: String::new(),
        no_embed_sources: false,
        full_provenance: false,
        allow_remote: false,
        max_pipelines: 1,
        diff: None,
        changed_files: vec![],
        inputs: vec![],
        clone_missing: false,
    };
    let (output, _setup) = super::scan::run_scan(&scan_args, &scenario, &opts, vec![])?;
    let Some(merged) = &output.merged_root else {
        super::print_diagnostics(&output.graph);
        anyhow::bail!("the configuration could not be resolved locally");
    };
    let root = root_pipeline(&output)?;
    let entry_text = root
        .entry_source
        .and_then(|f| output.graph.sources.iter().find(|s| s.file == f))
        .and_then(|s| s.text.clone())
        .ok_or_else(|| anyhow::anyhow!("the entry file's text is not available"))?;
    let git_ref = args
        .git_ref
        .clone()
        .or_else(|| root.git_ref.clone())
        .or_else(|| root.default_branch.clone())
        .unwrap_or_else(|| "main".to_string());
    let host = &root.project.host;
    let path = &root.project.path;
    println!("glpv check {path} @ {git_ref}  (host {host})");

    let oracle: Oracle = match &args.oracle_json {
        Some(p) => serde_json::from_str(&std::fs::read_to_string(p)?)?,
        None => {
            let body = serde_json::json!({
                "content": entry_text,
                "dry_run": true,
                "include_jobs": true,
                "ref": git_ref,
            })
            .to_string();
            let api_path = format!("projects/{}/ci/lint", encode_path(path));
            let raw = match args.transport {
                Transport::Glab => call_glab(host, &api_path, &body)?,
                Transport::Curl => {
                    let base = args
                        .api_url
                        .clone()
                        .unwrap_or_else(|| format!("https://{host}/api/v4"));
                    call_curl(&base, &api_path, &body)?
                }
            };
            if let Some(p) = &args.save_oracle {
                std::fs::write(p, &raw)?;
            }
            serde_json::from_str(&raw)?
        }
    };
    if let Some(m) = oracle.message.as_ref().or(oracle.error.as_ref()) {
        anyhow::bail!("the server answered with an error: {m}");
    }
    if oracle.valid == Some(false) {
        println!("the server rejected the configuration:");
        for e in &oracle.errors {
            println!("  error: {e}");
        }
        return Ok(2);
    }
    for w in &oracle.warnings {
        println!("  server warning: {w}");
    }

    let mut exit = 0;
    let local = check::normalize_root(node_to_json(merged));
    let server_yaml = oracle
        .merged_yaml
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("the server returned no merged_yaml"))?;
    let server = check::normalize_root(check::yaml_to_json(server_yaml)?);
    match check::unified_diff(&local, &server) {
        None => println!(
            "merged configuration: identical ({} top-level keys)",
            local.as_object().map(|m| m.len()).unwrap_or(0)
        ),
        Some(diff) => {
            println!("merged configuration: DIFFERS");
            print!("{diff}");
            exit = 1;
        }
    }

    if !args.merged_only {
        match &oracle.jobs {
            None => println!("jobs: the server returned no job list (include_jobs unsupported?)"),
            Some(server_jobs) => {
                exit = exit.max(report_jobs(
                    root,
                    server_jobs,
                    check::CompareFields::ALL,
                    "would create",
                ));
            }
        }
    }
    super::print_diagnostics(&output.graph);
    Ok(exit)
}

fn oneline(s: &str) -> String {
    let s = s.replace('\n', " ⏎ ");
    if s.len() > 160 {
        format!("{}…", &s[..160])
    } else {
        s
    }
}

fn encode_path(path: &str) -> String {
    path.replace('%', "%25").replace('/', "%2F")
}

fn call_glab(host: &str, api_path: &str, body: &str) -> anyhow::Result<String> {
    let mut child = Command::new("glab")
        .args([
            "api",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "--hostname",
            host,
            api_path,
            "--input",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            anyhow::anyhow!("cannot run `glab` ({e}); install it or use --api-transport curl")
        })?;
    child.stdin.take().unwrap().write_all(body.as_bytes())?;
    let out = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() && stdout.trim().is_empty() {
        anyhow::bail!(
            "glab api failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(stdout)
}

fn call_curl(base: &str, api_path: &str, body: &str) -> anyhow::Result<String> {
    let token = std::env::var("GLPV_TOKEN")
        .or_else(|_| std::env::var("GITLAB_TOKEN"))
        .map_err(|_| {
            anyhow::anyhow!("set GLPV_TOKEN (or GITLAB_TOKEN) for --api-transport curl")
        })?;
    let mut child = Command::new("curl")
        .args([
            "-sS",
            "-X",
            "POST",
            "-H",
            &format!("PRIVATE-TOKEN: {token}"),
            "-H",
            "Content-Type: application/json",
            "--data-binary",
            "@-",
            &format!("{}/{api_path}", base.trim_end_matches('/')),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot run `curl` ({e})"))?;
    child.stdin.take().unwrap().write_all(body.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!(
            "curl failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}
