//! `glpv` — crawl a GitLab CI configuration and render the pipeline graph.

mod cmd;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "glpv",
    version,
    about = "GitLab end-to-end pipeline crawler & viewer"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Crawl a pipeline (following includes and triggers) and write graph outputs.
    Scan(cmd::scan::ScanArgs),
    /// Print the fully merged configuration (includes + extends + !reference).
    Resolve(cmd::resolve::ResolveArgs),
    /// Show the project index built from the clones folder(s).
    Index(cmd::index::IndexCmdArgs),
    /// Compare the local resolution with the server's lint API (merged
    /// configuration and the jobs that would run); exit 1 on any difference.
    Check(cmd::check::CheckArgs),
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan(args) => cmd::scan::run(args),
        Command::Resolve(args) => cmd::resolve::run(args),
        Command::Index(args) => cmd::index::run(args),
        Command::Check(args) => cmd::check::run(args),
    }
}
