//! Graphviz DOT export. Hierarchy: an outer cluster per **project** (the
//! boundary that matters when triggers cross repositories) → one cluster per
//! pipeline → one sub-cluster per stage (`rank=same`), with invisible weighted
//! edges keeping stage columns ordered. Trigger edges connect a bridge job to
//! the target pipeline's cluster — visibly crossing the project border when
//! the downstream pipeline lives in another repository.

use std::fmt::Write;

use glpv_core::model::{Graph, NeedKind, Pipeline, PipelineKind, RulesMode, When};

pub fn render_dot(graph: &Graph) -> String {
    let mut out = String::new();
    out.push_str("digraph glpv {\n");
    out.push_str("  rankdir=LR;\n  compound=true;\n  newrank=true;\n");
    out.push_str("  graph [fontname=\"Helvetica\"];\n");
    out.push_str(
        "  node [shape=box style=\"rounded,filled\" fillcolor=\"#f6f8fa\" \
         fontname=\"Helvetica\" fontsize=10];\n",
    );
    out.push_str("  edge [fontname=\"Helvetica\" fontsize=9];\n\n");

    // Group pipelines by project, preserving first-seen order.
    let mut projects: Vec<(String, Vec<&Pipeline>)> = Vec::new();
    for p in &graph.pipelines {
        let key = format!("{}/{}", p.project.host, p.project.path);
        match projects.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(p),
            None => projects.push((key, vec![p])),
        }
    }
    let draw_project_borders = projects.len() > 1;

    for (pi, (project_label, pipelines)) in projects.iter().enumerate() {
        if draw_project_borders {
            let _ = writeln!(out, "  subgraph \"cluster_prj_{pi}\" {{");
            let _ = writeln!(out, "    label=\"{}\";", esc(project_label));
            let _ = writeln!(
                out,
                "    style=\"filled\"; fillcolor=\"#f0f1f5\"; color=\"#57606a\"; \
                 penwidth=1.6; fontsize=13; labeljust=l;"
            );
        }
        for p in pipelines {
            pipeline_cluster(&mut out, p, draw_project_borders);
        }
        if draw_project_borders {
            out.push_str("  }\n\n");
        }
    }

    for e in &graph.trigger_edges {
        let target = graph.pipelines.iter().find(|p| p.id == e.to_pipeline);
        let to_node = match target {
            Some(p) if p.unresolved.is_some() && p.jobs.is_empty() => {
                if matches!(p.kind, PipelineKind::DynamicChild) {
                    format!("{}::dynamic", p.id.0)
                } else {
                    format!("{}::unresolved", p.id.0)
                }
            }
            Some(p) => p
                .jobs
                .first()
                .map(|j| j.id.0.clone())
                .unwrap_or_else(|| format!("{}::empty", p.id.0)),
            None => continue,
        };
        let style = crate::common::trigger_edge_style(graph, &e.from_job, e.strategy.as_deref());
        let mut attrs = vec![
            if style.conditional {
                "style=\"dashed,bold\"".to_string()
            } else {
                "style=bold".to_string()
            },
            "color=\"#8250df\"".to_string(),
            format!("lhead=\"cluster_{}\"", sanitize(&e.to_pipeline.0)),
            format!("label=\"{}\"", esc(&style.label)),
        ];
        if e.cycle {
            attrs.push("color=\"#cf222e\"".to_string());
            attrs.push("constraint=false".to_string());
            attrs.push(format!("label=\"{} (cycle)\"", esc(&style.label)));
        }
        let _ = writeln!(
            out,
            "  \"{}\" -> \"{}\" [{}];",
            esc(&e.from_job.0),
            esc(&to_node),
            attrs.join(" ")
        );
    }

    out.push_str("}\n");
    out
}

fn pipeline_cluster(out: &mut String, p: &Pipeline, inside_project: bool) {
    let cluster = format!("cluster_{}", sanitize(&p.id.0));
    // Inside a project border the project is already named; keep the
    // pipeline label to what varies (kind, ref, config).
    let kind_label = match p.kind {
        PipelineKind::Root => "pipeline",
        PipelineKind::MultiProject => "downstream pipeline",
        PipelineKind::Child => "child pipeline",
        PipelineKind::DynamicChild => "dynamic child pipeline",
        PipelineKind::Unresolved => "unresolved pipeline",
        PipelineKind::Detached => "detached pipeline",
    };
    let title = if inside_project {
        format!(
            "{kind_label} @ {} — {}",
            p.git_ref.as_deref().unwrap_or("worktree"),
            p.config_path
        )
    } else {
        format!(
            "{}/{} @ {} — {}",
            p.project.host,
            p.project.path,
            p.git_ref.as_deref().unwrap_or("worktree"),
            p.config_path
        )
    };
    let _ = writeln!(out, "  subgraph \"{cluster}\" {{");
    let _ = writeln!(out, "    label=\"{}\";", esc(&title));
    let _ = writeln!(
        out,
        "    style=\"rounded,filled\"; fillcolor=\"#ffffff\"; color=\"#8250df\"; fontsize=11;"
    );

    if let Some(u) = &p.unresolved {
        if matches!(p.kind, PipelineKind::DynamicChild) {
            let _ = writeln!(
                out,
                "    \"{}::dynamic\" [shape=note style=\"dashed,filled\" fillcolor=\"#fff8c5\" \
                 label=\"dynamic child pipeline\\n{}\"];",
                esc(&p.id.0),
                esc(&truncate(&u.detail, 60))
            );
        } else {
            let _ = writeln!(
                out,
                "    \"{}::unresolved\" [shape=octagon fillcolor=\"#ffebe9\" label=\"? {}\"];",
                esc(&p.id.0),
                esc(&format!("{:?}: {}", u.reason, truncate(&u.detail, 60)))
            );
        }
        out.push_str("  }\n\n");
        return;
    }

    let mut prev_stage_first: Option<String> = None;
    for (si, stage) in p.stages.iter().enumerate() {
        let jobs: Vec<_> = p.jobs.iter().filter(|j| &j.stage == stage).collect();
        if jobs.is_empty() {
            continue;
        }
        let _ = writeln!(out, "    subgraph \"{cluster}_s{si}\" {{");
        let _ = writeln!(
            out,
            "      label=\"{}\"; rank=same; style=dashed; color=\"#d0d7de\"; fontsize=10;",
            esc(stage)
        );
        for j in &jobs {
            let mut attrs = vec![format!("label=\"{}\"", esc(&j.name))];
            match j.rules.mode {
                RulesMode::Conditional | RulesMode::Legacy => {
                    attrs.push("fillcolor=\"#fff8c5\"".to_string())
                }
                RulesMode::Manual => {
                    attrs.push("fillcolor=\"#ffd8b5\"".to_string());
                    attrs.push("peripheries=2".to_string());
                }
                RulesMode::Never => {
                    attrs.push("fillcolor=\"#eeeeee\"".to_string());
                    attrs.push("fontcolor=\"#999999\"".to_string());
                }
                _ => {}
            }
            if j.when == When::Manual {
                attrs.push("peripheries=2".to_string());
            }
            if j.trigger.is_some() {
                attrs.push("color=\"#8250df\"".to_string());
                attrs.push("penwidth=2".to_string());
            }
            let tooltip = format!("stage: {}", j.stage);
            attrs.push(format!("tooltip=\"{}\"", esc(&tooltip)));
            let _ = writeln!(out, "      \"{}\" [{}];", esc(&j.id.0), attrs.join(" "));
        }
        let _ = writeln!(out, "    }}");

        if let (Some(prev), Some(first)) = (&prev_stage_first, jobs.first()) {
            let _ = writeln!(
                out,
                "    \"{}\" -> \"{}\" [style=invis weight=100];",
                esc(prev),
                esc(&first.id.0)
            );
        }
        prev_stage_first = jobs.first().map(|j| j.id.0.clone());
    }

    // needs edges (within the pipeline), deduplicated to base-job pairs so
    // matrix expansions do not repeat identical lines
    let base_of = |name: &str| -> String {
        p.jobs
            .iter()
            .find(|j| j.name == name)
            .map(|j| j.base_name.clone().unwrap_or_else(|| j.name.clone()))
            .unwrap_or_else(|| name.to_string())
    };
    let mut seen_pairs: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    for j in &p.jobs {
        let j_base = j.base_name.clone().unwrap_or_else(|| j.name.clone());
        for n in &j.needs {
            match n.kind {
                NeedKind::Normal => {
                    let t_base = base_of(&n.job);
                    if !seen_pairs.insert((t_base.clone(), j_base.clone())) {
                        continue;
                    }
                    let first_target = p
                        .jobs
                        .iter()
                        .find(|t| t.base_name.as_deref().unwrap_or(&t.name) == t_base);
                    let first_source = p
                        .jobs
                        .iter()
                        .find(|s| s.base_name.as_deref().unwrap_or(&s.name) == j_base);
                    let (Some(ft), Some(fs)) = (first_target, first_source) else {
                        continue;
                    };
                    let target_id = ft.id.0.clone();
                    let source_id = fs.id.0.clone();
                    if !p.jobs.iter().any(|t| t.id.0 == target_id) {
                        continue; // optional need on an absent job
                    }
                    let style = if n.optional { "dashed" } else { "solid" };
                    let _ = writeln!(
                        out,
                        "    \"{}\" -> \"{}\" [style={} color=\"#0969da\"];",
                        esc(&target_id),
                        esc(&source_id),
                        style
                    );
                }
                NeedKind::CrossProjectArtifact => {
                    let ext = format!(
                        "{}::ext::{}::{}",
                        p.id.0,
                        n.project.as_deref().unwrap_or(""),
                        n.job
                    );
                    let _ = writeln!(
                        out,
                        "    \"{}\" [shape=folder style=dotted label=\"{}\\n{}\"];",
                        esc(&ext),
                        esc(n.project.as_deref().unwrap_or("?")),
                        esc(&n.job)
                    );
                    let _ = writeln!(
                        out,
                        "    \"{}\" -> \"{}\" [style=dotted color=\"#1a7f37\" label=\"artifacts\"];",
                        esc(&ext),
                        esc(&j.id.0)
                    );
                }
                NeedKind::ParentPipeline => {}
                NeedKind::Unresolved => {
                    let ext = format!("{}::missing::{}", p.id.0, n.job);
                    let _ = writeln!(
                        out,
                        "    \"{}\" [shape=octagon fillcolor=\"#ffebe9\" label=\"? {}\"];",
                        esc(&ext),
                        esc(&n.job)
                    );
                    let _ = writeln!(
                        out,
                        "    \"{}\" -> \"{}\" [color=\"#cf222e\"];",
                        esc(&ext),
                        esc(&j.id.0)
                    );
                }
            }
        }
    }

    out.push_str("  }\n\n");
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n).collect();
        format!("{cut}…")
    }
}
