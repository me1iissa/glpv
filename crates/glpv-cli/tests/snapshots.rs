//! Golden snapshots of the graph JSON, DOT and Mermaid outputs over the
//! fixture repositories. Fixture commits are deterministic, so shas are stable.

mod support;

use glpv_core::diff::DiffSpec;
use glpv_core::model::{Outcome, Pipeline};
use glpv_core::resolve::ResolveOpts;
use glpv_core::scan::scan_file;
use glpv_core::vars::Scenario;

fn scan_fixture(name: &str) -> glpv_core::scan::ScanOutput {
    scan_fixture_with(name, ResolveOpts::default(), Scenario::push_default())
}

fn scan_fixture_with(
    name: &str,
    opts: ResolveOpts,
    scenario: Scenario,
) -> glpv_core::scan::ScanOutput {
    let clone = support::build_fixture(name);
    let spec_branch = match name {
        "legacy" => "master",
        _ => "main",
    };
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

// ---- rules:changes ----

fn outcome_of(p: &Pipeline, job: &str) -> Outcome {
    p.jobs
        .iter()
        .find(|j| j.name == job)
        .unwrap_or_else(|| panic!("job {job} missing"))
        .evaluations[0]
        .outcome
}

fn note_of(p: &Pipeline, job: &str) -> String {
    p.jobs.iter().find(|j| j.name == job).unwrap().evaluations[0]
        .trace
        .iter()
        .filter_map(|t| t.note.clone())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn has_job(p: &Pipeline, job: &str) -> bool {
    p.jobs.iter().any(|j| j.name == job)
}

fn codes(output: &glpv_core::scan::ScanOutput) -> Vec<&str> {
    output
        .graph
        .diagnostics
        .iter()
        .map(|d| d.code.as_str())
        .collect()
}

fn with_diff(spec: DiffSpec) -> ResolveOpts {
    ResolveOpts {
        diff: Some(spec),
        ..ResolveOpts::default()
    }
}

#[test]
fn fx_changes() {
    let output = scan_fixture_with(
        "changes",
        with_diff(DiffSpec::Base("v1".into())),
        Scenario::push_default(),
    );
    let p = &output.graph.pipelines[0];

    let diff = p.diff.as_ref().expect("root carries the diff");
    assert_eq!(diff.base.as_deref(), Some("v1"));
    assert_eq!(
        diff.files.as_deref().unwrap(),
        [
            ".hidden/config",
            "docs/sub/x.md",
            "scripts/build.sh",
            "src/main.rs",
            "src/util/helpers.rs",
        ]
    );
    assert_eq!(
        diff.compare_to["release"],
        [".hidden/config", "scripts/build.sh", "src/main.rs"]
    );

    assert_eq!(outcome_of(p, "build"), Outcome::Runs);
    assert_eq!(note_of(p, "build"), "changes: matched by src/main.rs");
    assert_eq!(outcome_of(p, "docs"), Outcome::Runs);
    assert_eq!(outcome_of(p, "lint-yaml"), Outcome::Skipped);
    assert_eq!(
        note_of(p, "lint-yaml"),
        "changes: no match in 5 changed file(s)"
    );
    assert_eq!(outcome_of(p, "never-first"), Outcome::Skipped);
    assert_eq!(outcome_of(p, "docs-vs-release"), Outcome::Skipped);
    assert_eq!(
        note_of(p, "docs-vs-release"),
        "changes: no match in 3 changed file(s)"
    );
    assert_eq!(outcome_of(p, "qmark"), Outcome::Runs);
    assert_eq!(outcome_of(p, "class"), Outcome::Runs);
    assert_eq!(outcome_of(p, "dotmatch"), Outcome::Runs);
    assert_eq!(outcome_of(p, "slash"), Outcome::Skipped);
    assert_eq!(outcome_of(p, "var-pattern"), Outcome::Runs);
    assert_eq!(outcome_of(p, "unknown-var"), Outcome::Unknown);
    assert_eq!(note_of(p, "unknown-var"), "changes: $UNKNOWN_DIR unknown");
    assert_eq!(outcome_of(p, "bare-star"), Outcome::Skipped);

    // include:rules:changes decided against the same diff.
    assert!(has_job(p, "docs-extra"));
    assert!(!has_job(p, "never-included"));

    let codes = codes(&output);
    assert!(codes.contains(&"rules.changes-leading-slash"));
    assert!(!codes.contains(&"include.rules-undecidable"));
    assert!(!codes.contains(&"diff.unavailable"));

    snapshot_graph("changes", &output);
}

#[test]
fn fx_changes_explicit_files() {
    let output = scan_fixture_with(
        "changes",
        with_diff(DiffSpec::Files(vec!["docs/index.md".into()])),
        Scenario::push_default(),
    );
    let p = &output.graph.pipelines[0];
    let diff = p.diff.as_ref().unwrap();
    assert_eq!(diff.base, None);
    assert_eq!(diff.files.as_deref().unwrap(), ["docs/index.md"]);

    assert_eq!(outcome_of(p, "build"), Outcome::Skipped);
    assert_eq!(outcome_of(p, "docs"), Outcome::Runs);
    assert_eq!(outcome_of(p, "bare-star"), Outcome::Runs);
    assert_eq!(outcome_of(p, "never-first"), Outcome::Runs);
    // compare_to diffs the clone regardless of the explicit list.
    assert_eq!(outcome_of(p, "docs-vs-release"), Outcome::Skipped);
    assert!(has_job(p, "docs-extra"));
    assert!(!has_job(p, "never-included"));
}

#[test]
fn fx_changes_tag_pipeline() {
    let scenario = Scenario {
        id: "push@v1".into(),
        source: "push".into(),
        git_ref: Some("v1".into()),
        is_tag: true,
        vars: Default::default(),
    };
    let output = scan_fixture_with("changes", with_diff(DiffSpec::Base("v1".into())), scenario);
    let p = &output.graph.pipelines[0];

    // A tag push has no changed-paths set: plain clauses always match …
    for job in [
        "build",
        "docs",
        "lint-yaml",
        "qmark",
        "slash",
        "unknown-var",
        "bare-star",
    ] {
        assert_eq!(outcome_of(p, job), Outcome::Runs, "{job}");
    }
    assert_eq!(
        note_of(p, "lint-yaml"),
        "changes: no push event for source push; always matches"
    );
    // … a matching `when: never` clause included …
    assert_eq!(outcome_of(p, "never-first"), Outcome::Skipped);
    // … while `compare_to` still diffs.
    assert_eq!(outcome_of(p, "docs-vs-release"), Outcome::Skipped);
    // include:rules:changes follow the same rule.
    assert!(has_job(p, "docs-extra"));
    assert!(has_job(p, "never-included"));
}

#[test]
fn fx_changes_no_diff() {
    let output = scan_fixture("changes");
    let p = &output.graph.pipelines[0];

    // Only the compare_to clause is decidable; workflow:rules is undecided
    // too, so everything else is unknown.
    assert_eq!(outcome_of(p, "docs-vs-release"), Outcome::Skipped);
    assert_eq!(outcome_of(p, "build"), Outcome::Unknown);
    assert_eq!(
        note_of(p, "build"),
        "changes: depends on the diff; undecidable statically"
    );
    assert_eq!(
        p.jobs
            .iter()
            .find(|j| j.name == "build")
            .unwrap()
            .evaluations[0]
            .blocked_by
            .as_deref(),
        Some("workflow:rules undecided")
    );
    assert!(has_job(p, "docs-extra"));
    assert!(has_job(p, "never-included"));
    let codes = codes(&output);
    assert!(codes.contains(&"include.rules-undecidable"));

    // Only the compare_to list is recorded.
    let diff = p.diff.as_ref().unwrap();
    assert_eq!(diff.base, None);
    assert_eq!(diff.files, None);
    assert_eq!(diff.compare_to.len(), 1);
}
