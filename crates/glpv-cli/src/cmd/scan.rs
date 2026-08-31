use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use glpv_core::resolve::ResolveOpts;
use glpv_core::scan::{ScanOutput, scan_entry, scan_file};
use glpv_core::source::{ProjectKey, ProjectLocator, ProjectSource, TreeRef};

#[derive(clap::Args)]
pub struct ScanArgs {
    /// Entry `.gitlab-ci.yml` (its repository provides the project identity).
    #[arg(long, conflicts_with = "entry")]
    pub file: Option<PathBuf>,
    /// Entry project path (e.g. `acme/api`), located via the index.
    #[arg(long)]
    pub entry: Option<String>,
    /// Crawl every indexed project, then discover unreferenced CI-looking
    /// YAML files as detached pipelines.
    #[arg(long, conflicts_with_all = ["entry", "file"])]
    pub all: bool,
    /// Resolve at this git ref (default: worktree for --file, default branch for --entry).
    #[arg(long = "ref")]
    pub git_ref: Option<String>,
    /// Override the entry config path (default: the project's ci_config_path).
    #[arg(long)]
    pub config_path: Option<String>,
    #[command(flatten)]
    pub index: super::IndexArgs,
    #[command(flatten)]
    pub scenario: super::ScenarioArgs,
    /// Output directory.
    #[arg(short, long, default_value = "glpv-out")]
    pub out: PathBuf,
    /// Comma-separated formats: html,json,dot,mermaid.
    #[arg(long, default_value = "html,json,dot,mermaid")]
    pub format: String,
    /// Do not embed source file texts in the JSON graph.
    #[arg(long)]
    pub no_embed_sources: bool,
    /// Record per-key provenance spans on every job (bigger output).
    #[arg(long)]
    pub full_provenance: bool,
    /// Allow `include:remote` HTTP fetches.
    #[arg(long)]
    pub allow_remote: bool,
    /// Stop crawling after this many pipelines.
    #[arg(long, default_value_t = 200)]
    pub max_pipelines: u32,
}

pub fn run(args: ScanArgs) -> anyhow::Result<()> {
    let scenario = args.scenario.to_scenario()?;
    let opts = ResolveOpts {
        embed_sources: !args.no_embed_sources,
        allow_remote: args.allow_remote,
        max_pipelines: args.max_pipelines,
        full_provenance: args.full_provenance,
        ..ResolveOpts::default()
    };
    let tool_args: Vec<String> = std::env::args().skip(1).collect();
    let output = run_scan(&args, &scenario, &opts, tool_args)?;
    let graph = &output.graph;

    std::fs::create_dir_all(&args.out)
        .with_context(|| format!("cannot create {}", args.out.display()))?;

    let formats: Vec<&str> = args.format.split(',').map(|f| f.trim()).collect();
    for format in &formats {
        match *format {
            "json" => {
                let path = args.out.join("graph.json");
                std::fs::write(&path, glpv_render::render_json(graph))?;
                println!("wrote {}", path.display());
            }
            "dot" => {
                let path = args.out.join("graph.dot");
                std::fs::write(&path, glpv_render::render_dot(graph))?;
                println!("wrote {}", path.display());
            }
            "mermaid" => {
                let dir = args.out.join("mermaid");
                std::fs::create_dir_all(&dir)?;
                for (name, content) in glpv_render::render_mermaid(graph) {
                    std::fs::write(dir.join(&name), content)?;
                }
                println!("wrote {}/", dir.display());
            }
            "html" => {
                let path = args.out.join("index.html");
                std::fs::write(&path, glpv_render::render_html(graph))?;
                println!("wrote {}", path.display());
            }
            other => anyhow::bail!("unknown format `{other}`"),
        }
    }

    let jobs: usize = graph.pipelines.iter().map(|p| p.jobs.len()).sum();
    println!(
        "{} pipeline(s), {} job(s), {} trigger edge(s), {} diagnostic(s)",
        graph.pipelines.len(),
        jobs,
        graph.trigger_edges.len(),
        graph.diagnostics.len()
    );
    super::print_diagnostics(graph);
    Ok(())
}

pub fn run_scan(
    args: &ScanArgs,
    scenario: &glpv_core::vars::Scenario,
    opts: &ResolveOpts,
    tool_args: Vec<String>,
) -> anyhow::Result<ScanOutput> {
    let setup = args.index.build()?;

    if args.all {
        let metas = setup.index.all();
        if metas.is_empty() {
            anyhow::bail!("the project index is empty; pass --projects <dir>");
        }
        let projects: Vec<Arc<dyn ProjectSource>> = metas
            .iter()
            .filter_map(|m| setup.index.lookup(&m.key))
            .map(|p| p as Arc<dyn ProjectSource>)
            .collect();
        return Ok(glpv_core::scan::scan_all(
            &setup.sources,
            projects,
            scenario,
            opts,
            tool_args,
            setup.index_diags,
        ));
    }

    if let Some(file) = &args.file {
        return Ok(scan_file(
            file,
            args.git_ref.as_deref(),
            scenario,
            opts,
            &setup.sources,
            tool_args,
        )?);
    }

    let Some(entry) = &args.entry else {
        anyhow::bail!("pass --file <path> or --entry <group/project>");
    };
    let host = match &setup.host {
        Some(h) => h.clone(),
        None => {
            let hosts: std::collections::BTreeSet<String> =
                setup.index.all().into_iter().map(|m| m.key.host).collect();
            match hosts.len() {
                1 => hosts.into_iter().next().unwrap(),
                0 => anyhow::bail!("the project index is empty; pass --projects <dir>"),
                _ => anyhow::bail!(
                    "several hosts in the index ({}); pass --host",
                    hosts.into_iter().collect::<Vec<_>>().join(", ")
                ),
            }
        }
    };
    let key = ProjectKey::new(&host, entry);
    let Some(project) = setup.index.lookup(&key) else {
        anyhow::bail!(
            "{host}/{entry} is not in the index; run `glpv index --projects …` to see what is"
        );
    };
    let project: Arc<dyn ProjectSource> = project;

    let ref_name = match &args.git_ref {
        Some(r) => r.clone(),
        None => project.default_branch()?,
    };
    let Some(sha) = project.resolve_ref(&ref_name)? else {
        anyhow::bail!(
            "ref `{ref_name}` not found in {}",
            project.meta().display_path
        );
    };

    Ok(scan_entry(
        &setup.sources,
        project,
        TreeRef::Commit(sha),
        Some(ref_name),
        args.config_path.clone(),
        scenario,
        opts,
        tool_args,
        setup.index_diags,
    ))
}
