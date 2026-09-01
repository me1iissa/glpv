//! `glpv check` — the oracle: resolve a project locally, ask the GitLab
//! server to lint the same entry file at the same ref (`merged_yaml` +
//! `include_jobs`), and report every difference. Exit 0 when identical, 1 on
//! differences, 2 when the check could not be carried out.

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

fn run_inner(args: CheckArgs) -> anyhow::Result<i32> {
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
    let root = output
        .graph
        .pipelines
        .iter()
        .find(|p| p.kind == glpv_core::model::PipelineKind::Root)
        .or_else(|| output.graph.pipelines.first())
        .ok_or_else(|| anyhow::anyhow!("no pipeline was resolved"))?;
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
                let local_jobs = check::local_jobs(root);
                let r = check::compare_jobs(&local_jobs, server_jobs);
                let expected = local_jobs
                    .iter()
                    .filter(|j| {
                        matches!(
                            j.outcome,
                            glpv_core::model::Outcome::Runs
                                | glpv_core::model::Outcome::Manual
                                | glpv_core::model::Outcome::Delayed
                        )
                    })
                    .count();
                println!(
                    "jobs: server would create {}; local expects {} to run ({} undecided){}",
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
                if !r.is_clean() {
                    exit = 1;
                }
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
