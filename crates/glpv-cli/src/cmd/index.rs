use glpv_core::source::{ProjectLocator, ProjectOrigin};

#[derive(clap::Args)]
pub struct IndexCmdArgs {
    #[command(flatten)]
    pub index: super::IndexArgs,
    /// Print JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: IndexCmdArgs) -> anyhow::Result<()> {
    let setup = args.index.build()?;
    let metas = setup.index.all();

    if args.json {
        let rows: Vec<serde_json::Value> = metas
            .iter()
            .map(|m| {
                let dir = match &m.origin {
                    ProjectOrigin::LocalClone(p) => p.display().to_string(),
                    ProjectOrigin::Api { project_id } => format!("api:{project_id}"),
                };
                serde_json::json!({
                    "host": m.key.host,
                    "path": m.display_path,
                    "dir": dir,
                    "ci_config_path": m.ci_config_path,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        let width = metas
            .iter()
            .map(|m| m.key.host.len() + m.display_path.len() + 1)
            .max()
            .unwrap_or(20);
        for m in &metas {
            let full = format!("{}/{}", m.key.host, m.display_path);
            let dir = match &m.origin {
                ProjectOrigin::LocalClone(p) => p.display().to_string(),
                ProjectOrigin::Api { project_id } => format!("api:{project_id}"),
            };
            println!("{full:<width$}  {dir}");
        }
        println!("{} project(s) indexed", metas.len());
    }
    for d in &setup.index_diags {
        eprintln!("warn  [{}] {}", d.code, d.message);
    }
    Ok(())
}
