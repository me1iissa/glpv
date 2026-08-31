//! Golden snapshots of the graph JSON, DOT and Mermaid outputs over the
//! fixture repositories. Fixture commits are deterministic, so shas are stable.

mod support;

use glpv_core::resolve::ResolveOpts;
use glpv_core::scan::scan_file;
use glpv_core::vars::Scenario;

fn scan_fixture(name: &str) -> glpv_core::scan::ScanOutput {
    let clone = support::build_fixture(name);
    let spec_branch = match name {
        "legacy" => "master",
        _ => "main",
    };
    let scenario = Scenario::push_default();
    let opts = ResolveOpts::default();
    let sources = glpv_core::source::Sources::without_index();
    scan_file(
        &clone.join(".gitlab-ci.yml"),
        Some(spec_branch),
        &scenario,
        &opts,
        &sources,
        vec![],
    )
    .expect("scan succeeds")
}

fn snapshot_graph(name: &str, output: &glpv_core::scan::ScanOutput) {
    let value = serde_json::to_value(&output.graph).unwrap();
    insta::with_settings!({snapshot_path => "snapshots"}, {
        insta::assert_json_snapshot!(format!("{name}_graph"), value, {
            ".generated_at" => "[timestamp]",
            ".tool.version" => "[version]",
        });
        insta::assert_snapshot!(
            format!("{name}_dot"),
            glpv_render::render_dot(&output.graph)
        );
        let mermaid = glpv_render::render_mermaid(&output.graph)
            .into_iter()
            .map(|(f, c)| format!("### {f}\n{c}"))
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(format!("{name}_mermaid"), mermaid);
    });
}

#[test]
fn fx_app() {
    let output = scan_fixture("app");
    let p = &output.graph.pipelines[0];

    // Merge order: globbed job files + base are all present.
    let names: Vec<&str> = p.jobs.iter().map(|j| j.name.as_str()).collect();
    assert!(names.contains(&"build"));
    assert!(names.contains(&"test-matrix: [linux, x64]"));
    assert!(names.contains(&"test-matrix: [linux, arm64]"));
    assert!(names.contains(&"test-count 1/2"));
    assert!(names.contains(&"pages"));
    assert!(names.contains(&"setup"));

    // extends chain with null removal: deploy inherits cache but not variables.
    let deploy = p.jobs.iter().find(|j| j.name == "deploy").unwrap();
    assert!(deploy.merged_yaml.contains("cache"));
    assert!(!deploy.merged_yaml.contains("FROM_BASE"));
    // !reference through an included file spliced the script.
    assert!(deploy.merged_yaml.contains("echo step-one"));

    // Diagnostics we designed for: duplicate key, later-stage need, date scalar.
    let codes: Vec<&str> = output
        .graph
        .diagnostics
        .iter()
        .map(|d| d.code.as_str())
        .collect();
    assert!(codes.contains(&"yaml.duplicate-key"));
    assert!(codes.contains(&"needs.later-stage"));
    assert!(codes.contains(&"yaml.disallowed-class"));

    snapshot_graph("app", &output);
}

#[test]
fn fx_legacy() {
    let output = scan_fixture("legacy");
    let p = &output.graph.pipelines[0];

    let release = p.jobs.iter().find(|j| j.name == "release").unwrap();
    assert_eq!(release.when, glpv_core::model::When::Manual);
    // Manual outside rules → non-blocking by default.
    assert_eq!(
        release.allow_failure,
        glpv_core::model::AllowFailure::ExitCodes(vec![42, 137])
    );
    let compile = p.jobs.iter().find(|j| j.name == "compile").unwrap();
    assert_eq!(compile.rules.mode, glpv_core::model::RulesMode::Legacy);

    snapshot_graph("legacy", &output);
}
