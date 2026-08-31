//! Mermaid export.
//!
//! - `overview.mmd`: pipelines grouped inside **project** subgraphs, with
//!   trigger edges — the project borders are the outermost boxes.
//! - `combined.mmd`: the whole flow nested project → pipeline → stage → job
//!   (skipped above a size guard; Mermaid's default `maxEdges` is 500).
//! - one `<pipeline-id>.mmd` per pipeline for pasting into MRs.

use std::collections::HashMap;
use std::fmt::Write;

use glpv_core::model::{Graph, NeedKind, Pipeline, PipelineKind, RulesMode};

const COMBINED_MAX_JOBS: usize = 250;

/// Returns `(file_name, content)` pairs.
pub fn render_mermaid(graph: &Graph) -> Vec<(String, String)> {
    let mut out = Vec::new();
    out.push(("overview.mmd".to_string(), overview(graph)));
    out.push(("combined.mmd".to_string(), combined(graph)));
    for p in &graph.pipelines {
        out.push((format!("{}.mmd", p.id.0), pipeline(p)));
    }
    out
}

/// Pipelines grouped by project; each project is a bordered subgraph.
fn overview(graph: &Graph) -> String {
    let mut s = String::from("flowchart LR\n");
    let groups = group_by_project(graph);
    let mut node_ids: HashMap<&str, String> = HashMap::new();

    for (gi, (project, pipelines)) in groups.iter().enumerate() {
        let _ = writeln!(s, "    subgraph prj{gi}[\"{}\"]", esc(project));
        s.push_str("        direction TB\n");
        for p in pipelines {
            let id = format!("ov{}", node_ids.len());
            let label = pipeline_short_label(p);
            let line = if p.unresolved.is_some() {
                format!("        {id}{{{{\"{label}\"}}}}")
            } else {
                format!("        {id}[\"{label}\"]")
            };
            s.push_str(&line);
            s.push('\n');
            node_ids.insert(p.id.0.as_str(), id);
        }
        s.push_str("    end\n");
    }

    for e in &graph.trigger_edges {
        let from = graph
            .pipelines
            .iter()
            .find(|p| e.from_job.0.starts_with(&format!("{}/", p.id.0)))
            .and_then(|p| node_ids.get(p.id.0.as_str()));
        let to = node_ids.get(e.to_pipeline.0.as_str());
        if let (Some(f), Some(t)) = (from, to) {
            let style =
                crate::common::trigger_edge_style(graph, &e.from_job, e.strategy.as_deref());
            let job = e.from_job.0.rsplit('/').next().unwrap_or("");
            let label = if style.label == "trigger" {
                job.to_string()
            } else {
                format!("{job}: {}", style.label)
            };
            if e.cycle {
                let _ = writeln!(s, "    {f} -. \"{} (cycle)\" .-> {t}", esc(&label));
            } else if style.conditional {
                let _ = writeln!(s, "    {f} -. \"{}\" .-> {t}", esc(&label));
            } else {
                let _ = writeln!(s, "    {f} == \"{}\" ==> {t}", esc(&label));
            }
        }
    }
    s
}

/// The whole flow with jobs, nested project → pipeline → stage.
fn combined(graph: &Graph) -> String {
    let total_jobs: usize = graph.pipelines.iter().map(|p| p.jobs.len()).sum();
    if total_jobs > COMBINED_MAX_JOBS {
        return format!(
            "%% combined view skipped: {total_jobs} jobs exceed the {COMBINED_MAX_JOBS}-job guard\n\
             %% (use graph.dot for the full picture)\n"
        );
    }

    let mut s = String::new();
    if total_jobs > 40 {
        s.push_str("%%{init: {\"flowchart\": {\"defaultRenderer\": \"elk\"}} }%%\n");
    }
    s.push_str("flowchart LR\n");

    let groups = group_by_project(graph);
    let mut job_ids: HashMap<&str, String> = HashMap::new();
    let mut pipe_anchor: HashMap<&str, String> = HashMap::new(); // pipeline id → subgraph or node id
    let mut counter = 0usize;

    for (gi, (project, pipelines)) in groups.iter().enumerate() {
        let _ = writeln!(s, "    subgraph prj{gi}[\"{}\"]", esc(project));
        s.push_str("        direction LR\n");
        for (pi, p) in pipelines.iter().enumerate() {
            let sub = format!("prj{gi}p{pi}");
            let _ = writeln!(
                s,
                "        subgraph {sub}[\"{}\"]",
                esc(&pipeline_kind_label(p))
            );
            s.push_str("            direction LR\n");
            if let Some(u) = &p.unresolved {
                let id = format!("n{counter}");
                counter += 1;
                let _ = writeln!(
                    s,
                    "            {id}{{{{\"{}\"}}}}",
                    esc(&truncate(&u.detail, 60))
                );
                pipe_anchor.insert(p.id.0.as_str(), sub.clone());
            } else {
                for (si, stage) in p.stages.iter().enumerate() {
                    let jobs: Vec<_> = p.jobs.iter().filter(|j| &j.stage == stage).collect();
                    if jobs.is_empty() {
                        continue;
                    }
                    let _ = writeln!(s, "            subgraph {sub}s{si}[\"{}\"]", esc(stage));
                    s.push_str("                direction TB\n");
                    for j in jobs {
                        let id = format!("n{counter}");
                        counter += 1;
                        let label = esc(&j.name);
                        let line = match j.rules.mode {
                            RulesMode::Manual => format!("                {id}[/\"{label}\"/]"),
                            RulesMode::Conditional | RulesMode::Legacy => {
                                format!("                {id}([\"{label}\"])")
                            }
                            _ => format!("                {id}[\"{label}\"]"),
                        };
                        s.push_str(&line);
                        s.push('\n');
                        job_ids.insert(j.id.0.as_str(), id);
                    }
                    s.push_str("            end\n");
                }
                pipe_anchor.insert(p.id.0.as_str(), sub.clone());
            }
            s.push_str("        end\n");
        }
        s.push_str("    end\n");
    }

    for p in &graph.pipelines {
        for j in &p.jobs {
            for n in &j.needs {
                if n.kind != NeedKind::Normal {
                    continue;
                }
                let target_id = format!("{}/{}", p.id.0, n.job);
                if let (Some(from), Some(to)) = (
                    job_ids.get(target_id.as_str()),
                    job_ids.get(j.id.0.as_str()),
                ) {
                    let arrow = if n.optional { "-.->" } else { "-->" };
                    let _ = writeln!(s, "    {from} {arrow} {to}");
                }
            }
        }
    }
    for e in &graph.trigger_edges {
        let from = job_ids.get(e.from_job.0.as_str());
        let to = pipe_anchor.get(e.to_pipeline.0.as_str());
        if let (Some(f), Some(t)) = (from, to) {
            let style =
                crate::common::trigger_edge_style(graph, &e.from_job, e.strategy.as_deref());
            if e.cycle {
                let _ = writeln!(s, "    {f} -. \"{} (cycle)\" .-> {t}", esc(&style.label));
            } else if style.conditional {
                let _ = writeln!(s, "    {f} -. \"{}\" .-> {t}", esc(&style.label));
            } else {
                let _ = writeln!(s, "    {f} == \"{}\" ==> {t}", esc(&style.label));
            }
        }
    }
    s
}

fn pipeline(p: &Pipeline) -> String {
    let mut s = String::new();
    if p.jobs.len() > 40 {
        s.push_str("%%{init: {\"flowchart\": {\"defaultRenderer\": \"elk\"}} }%%\n");
    }
    s.push_str("flowchart LR\n");
    let _ = writeln!(
        s,
        "    %% {}/{} @ {} — {}",
        p.project.host,
        p.project.path,
        p.git_ref.as_deref().unwrap_or("worktree"),
        p.config_path
    );

    let mut ids: HashMap<&str, String> = HashMap::new();
    for (i, j) in p.jobs.iter().enumerate() {
        ids.insert(j.id.0.as_str(), format!("n{i}"));
        let _ = writeln!(s, "    %% n{i} = {}", j.name);
    }

    for (si, stage) in p.stages.iter().enumerate() {
        let jobs: Vec<_> = p.jobs.iter().filter(|j| &j.stage == stage).collect();
        if jobs.is_empty() {
            continue;
        }
        let _ = writeln!(s, "    subgraph s{si}[\"{}\"]", esc(stage));
        s.push_str("        direction TB\n");
        for j in jobs {
            let id = &ids[j.id.0.as_str()];
            let label = esc(&j.name);
            let line = match j.rules.mode {
                RulesMode::Manual => format!("        {id}[/\"{label}\"/]"),
                RulesMode::Conditional | RulesMode::Legacy => {
                    format!("        {id}([\"{label}\"])")
                }
                _ => format!("        {id}[\"{label}\"]"),
            };
            s.push_str(&line);
            s.push('\n');
        }
        s.push_str("    end\n");
    }

    for j in &p.jobs {
        for n in &j.needs {
            if n.kind != NeedKind::Normal {
                continue;
            }
            let target_id = format!("{}/{}", p.id.0, n.job);
            if let (Some(from), Some(to)) = (ids.get(target_id.as_str()), ids.get(j.id.0.as_str()))
            {
                let arrow = if n.optional { "-.->" } else { "-->" };
                let _ = writeln!(s, "    {from} {arrow} {to}");
            }
        }
    }
    s
}

fn group_by_project(graph: &Graph) -> Vec<(String, Vec<&Pipeline>)> {
    let mut groups: Vec<(String, Vec<&Pipeline>)> = Vec::new();
    for p in &graph.pipelines {
        let key = format!("{}/{}", p.project.host, p.project.path);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(p),
            None => groups.push((key, vec![p])),
        }
    }
    groups
}

fn pipeline_short_label(p: &Pipeline) -> String {
    if let Some(u) = &p.unresolved {
        return truncate(&u.detail, 50);
    }
    format!(
        "{} @ {} ({} jobs)",
        pipeline_kind_word(p.kind),
        p.git_ref.as_deref().unwrap_or("worktree"),
        p.jobs.len()
    )
}

fn pipeline_kind_label(p: &Pipeline) -> String {
    format!(
        "{} @ {} — {}",
        pipeline_kind_word(p.kind),
        p.git_ref.as_deref().unwrap_or("worktree"),
        p.config_path
    )
}

fn pipeline_kind_word(kind: PipelineKind) -> &'static str {
    match kind {
        PipelineKind::Root => "pipeline",
        PipelineKind::MultiProject => "downstream",
        PipelineKind::Child => "child",
        PipelineKind::DynamicChild => "dynamic child",
        PipelineKind::Unresolved => "unresolved",
        PipelineKind::Detached => "detached",
    }
}

fn esc(s: &str) -> String {
    s.replace('"', "#quot;")
        .replace('<', "#lt;")
        .replace('>', "#gt;")
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n).collect();
        format!("{cut}…")
    }
}
