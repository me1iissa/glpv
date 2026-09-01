//! `glpv check` offline: the comparison functions over a real fixture scan
//! and a synthetic oracle. Identity must be clean; every kind of divergence
//! must be reported.

mod support;

use glpv_cli::check::{self, OracleJob};
use glpv_core::model::Outcome;
use glpv_core::resolve::ResolveOpts;
use glpv_core::scan::scan_file;
use glpv_core::source::Sources;
use glpv_core::util::node_to_json;
use glpv_core::vars::Scenario;

fn scan_app() -> glpv_core::scan::ScanOutput {
    let clone = support::build_fixture("app");
    scan_file(
        &clone.join(".gitlab-ci.yml"),
        Some("main"),
        &Scenario::push_default(),
        &ResolveOpts::default(),
        &Sources::without_index(),
        vec![],
    )
    .expect("the app fixture scans")
}

/// An oracle response reconstructed from glpv's own result, the way the
/// server would phrase it (merged YAML text with `.pre`/`.post` listed; the
/// jobs a push pipeline creates).
fn oracle_from(output: &glpv_core::scan::ScanOutput) -> (String, Vec<OracleJob>) {
    let root = &output.graph.pipelines[0];
    let merged = output.merged_root.as_ref().unwrap();
    // normalisation drops the implicit stages wherever they sit
    let text =
        glpv_yaml::emit_document(merged).replacen("stages:\n", "stages:\n  - .pre\n  - .post\n", 1);
    let jobs = check::local_jobs(root)
        .into_iter()
        .filter(|j| {
            matches!(
                j.outcome,
                Outcome::Runs | Outcome::Manual | Outcome::Delayed
            )
        })
        .map(|j| OracleJob {
            name: j.name,
            stage: j.stage,
            script: j.script,
            before_script: j.before_script,
            after_script: j.after_script,
            when: j.when,
            allow_failure: j.allow_failure,
        })
        .collect();
    (text, jobs)
}

#[test]
fn identity_oracle_is_clean() {
    let output = scan_app();
    let root = &output.graph.pipelines[0];
    let (server_yaml, server_jobs) = oracle_from(&output);

    let local = check::normalize_root(node_to_json(output.merged_root.as_ref().unwrap()));
    let server = check::normalize_root(check::yaml_to_json(&server_yaml).unwrap());
    assert_eq!(
        check::unified_diff(&local, &server),
        None,
        "merged configuration round-trips"
    );

    let report = check::compare_jobs(&check::local_jobs(root), &server_jobs);
    assert!(report.is_clean(), "{report:?}");
    assert!(
        report.compared >= 5,
        "the app fixture runs several jobs: {}",
        report.compared
    );
    // the fixture's `deploy` is gated on a tag: undecided under push? no — unset
    // variable ⇒ skipped, so it must not be counted as compared
    assert!(
        check::local_jobs(root)
            .iter()
            .any(|j| j.outcome == Outcome::Skipped)
    );
}

#[test]
fn divergences_are_reported() {
    let output = scan_app();
    let root = &output.graph.pipelines[0];
    let (server_yaml, mut server_jobs) = oracle_from(&output);

    // a changed merged value shows up in the diff
    let mut server = check::yaml_to_json(&server_yaml).unwrap();
    let job = server
        .as_object_mut()
        .unwrap()
        .values_mut()
        .find(|v| v.get("script").is_some())
        .expect("a job with a script");
    job["script"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::Value::String("echo injected".into()));
    let local = check::normalize_root(node_to_json(output.merged_root.as_ref().unwrap()));
    let server = check::normalize_root(server);
    let diff = check::unified_diff(&local, &server).expect("a difference");
    assert!(diff.contains("echo injected"), "{diff}");

    // a job the server would not create, one it would create that glpv skips,
    // and a field mismatch
    let removed = server_jobs.remove(0).name;
    let skipped = check::local_jobs(root)
        .into_iter()
        .find(|j| j.outcome == Outcome::Skipped)
        .expect("a skipped job")
        .name;
    server_jobs.push(OracleJob {
        name: skipped.clone(),
        stage: "deploy".into(),
        script: vec![],
        before_script: vec![],
        after_script: vec![],
        when: "on_success".into(),
        allow_failure: false,
    });
    server_jobs[0].allow_failure = !server_jobs[0].allow_failure;
    let report = check::compare_jobs(&check::local_jobs(root), &server_jobs);
    assert_eq!(report.missing_on_server, vec![removed]);
    assert_eq!(report.unexpected_on_server, vec![skipped]);
    assert_eq!(report.field_mismatches.len(), 1);
    assert_eq!(report.field_mismatches[0].1, "allow_failure");
    assert!(!report.is_clean());
}
