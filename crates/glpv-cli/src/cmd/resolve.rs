use std::path::PathBuf;

use glpv_core::resolve::ResolveOpts;
use glpv_core::util::node_to_json;

#[derive(clap::Args)]
pub struct ResolveArgs {
    /// Entry `.gitlab-ci.yml`.
    #[arg(long, conflicts_with = "entry")]
    pub file: Option<PathBuf>,
    /// Entry project path (e.g. `acme/api`), located via the index.
    #[arg(long)]
    pub entry: Option<String>,
    /// Resolve at this git ref (default: worktree for --file, default branch for --entry).
    #[arg(long = "ref")]
    pub git_ref: Option<String>,
    #[command(flatten)]
    pub index: super::IndexArgs,
    #[command(flatten)]
    pub scenario: super::ScenarioArgs,
    /// Print JSON instead of YAML.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: ResolveArgs) -> anyhow::Result<()> {
    let scenario = args.scenario.to_scenario()?;
    let opts = ResolveOpts {
        // Only the entry pipeline matters here; do not crawl downstream.
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

    match &output.merged_root {
        None => anyhow::bail!("the configuration could not be resolved"),
        Some(root) => {
            if args.json {
                println!("{}", serde_json::to_string_pretty(&node_to_json(root))?);
            } else {
                print!("{}", glpv_yaml::emit_document(root));
            }
        }
    }
    super::print_diagnostics(&output.graph);
    Ok(())
}
