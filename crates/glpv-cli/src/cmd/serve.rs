//! `glpv serve` — scan, serve the output over HTTP, and (with --watch, the
//! default) rescan whenever a file under the scanned roots changes, pushing a
//! reload to every open viewer. The viewer keeps its URL state across the
//! reload, so a simulation set up in the browser survives edits to the YAML.

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use glpv_cli::serve::{Generation, Server, is_ignored};

#[derive(clap::Args)]
pub struct ServeArgs {
    #[command(flatten)]
    pub scan: super::scan::ScanArgs,
    /// Address to bind.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: String,
    /// Port to listen on (0 = any free port).
    #[arg(long, default_value_t = 7070)]
    pub port: u16,
    /// Do not watch for changes; serve the scan as it is.
    #[arg(long)]
    pub no_watch: bool,
    /// Quiet period after the last change before rescanning (milliseconds).
    #[arg(long, default_value_t = 300)]
    pub debounce_ms: u64,
}

pub fn run(args: ServeArgs) -> anyhow::Result<()> {
    let tool_args: Vec<String> = std::env::args().skip(1).collect();
    let (scenario, opts) = super::scan::build_opts(&args.scan)?;
    let mut scan_args = args.scan;
    if scan_args.format == "html,json,dot,mermaid" {
        // the live loop only needs what the browser loads
        scan_args.format = "html,json".to_string();
    }
    let output = super::scan::run_scan(&scan_args, &scenario, &opts, tool_args.clone())?;
    super::scan::write_outputs(&scan_args, &output, &opts)?;

    let generation = Arc::new(Generation::default());
    let out_root = scan_args.out.clone();
    let listener = TcpListener::bind((args.bind.as_str(), args.port))?;
    let addr = listener.local_addr()?;
    println!("serving {} at http://{addr}/", out_root.display());

    if !args.no_watch {
        let roots = watch_roots(&scan_args);
        let (tx, rx) = mpsc::channel::<PathBuf>();
        let out_dir = std::fs::canonicalize(&scan_args.out).unwrap_or(scan_args.out.clone());
        let mut watcher = notify::recommended_watcher(move |ev: notify::Result<notify::Event>| {
            // Reads (the rescan's own git calls open files and directories)
            // arrive as access events; only writes are changes.
            if let Ok(ev) = ev
                && !matches!(ev.kind, notify::EventKind::Access(_))
            {
                for p in ev.paths {
                    let _ = tx.send(p);
                }
            }
        })?;
        for r in &roots {
            notify::Watcher::watch(&mut watcher, r, notify::RecursiveMode::Recursive)?;
            println!("watching {}", r.display());
        }
        let generation = generation.clone();
        let debounce = Duration::from_millis(args.debounce_ms);
        std::thread::spawn(move || {
            let _keep = watcher;
            loop {
                let Ok(first) = rx.recv() else { return };
                let mut changed = vec![first];
                let deadline = Instant::now() + debounce;
                while let Ok(p) =
                    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                {
                    changed.push(p);
                }
                changed.retain(|p| !is_ignored(p, &out_dir));
                if changed.is_empty() {
                    continue;
                }
                changed.sort();
                changed.dedup();
                println!(
                    "change: {}{} — rescanning",
                    changed[0].display(),
                    if changed.len() > 1 {
                        format!(" (+{} more)", changed.len() - 1)
                    } else {
                        String::new()
                    }
                );
                match super::scan::run_scan(&scan_args, &scenario, &opts, tool_args.clone())
                    .and_then(|o| super::scan::write_outputs(&scan_args, &o, &opts).map(|_| o))
                {
                    Ok(_) => {
                        let g = generation.bump();
                        println!("generation {g}: reload pushed");
                    }
                    Err(e) => eprintln!("rescan failed (serving the previous output): {e:#}"),
                }
            }
        });
    }

    Arc::new(Server::new(out_root, generation)).run(listener);
    Ok(())
}

/// What to watch: the clone roots, or the entry file's repository.
fn watch_roots(args: &super::scan::ScanArgs) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = args.index.projects.clone();
    if roots.is_empty()
        && let Some(f) = &args.file
    {
        let dir = f.parent().unwrap_or(std::path::Path::new("."));
        let top = std::process::Command::new("git")
            .args([
                "-C",
                &dir.display().to_string(),
                "rev-parse",
                "--show-toplevel",
            ])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
        roots.push(top.unwrap_or_else(|| dir.to_path_buf()));
    }
    if roots.is_empty() {
        roots.push(PathBuf::from("."));
    }
    roots
}
