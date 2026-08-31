//! Test-side wrappers around the fixture builder.
#![allow(dead_code)] // each test binary uses a different subset

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn spec_path(name: &str) -> PathBuf {
    workspace_root().join(format!("tests/fixtures/projects/{name}/spec.toml"))
}

/// Build (or reuse) one fixture on its own; returns the clone directory.
pub fn build_fixture(name: &str) -> PathBuf {
    let root = workspace_root().join("target/glpv-fixtures/single");
    std::fs::create_dir_all(&root).unwrap();
    glpv_cli::fixtures::materialize(&spec_path(name), &root)
}

/// Build a set of fixtures into one shared root (the `--projects` folder).
/// Returns the root; clone dirs are `<root>/<spec dir>`.
pub fn fixture_root(names: &[&str]) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    names.hash(&mut hasher);
    let root = workspace_root()
        .join("target/glpv-fixtures")
        .join(format!("set-{:08x}", hasher.finish() as u32));
    let specs: Vec<PathBuf> = names.iter().map(|n| spec_path(n)).collect();
    glpv_cli::fixtures::build_set(&specs, &root);
    root
}

/// Build the demo projects (from `demo/projects/`) into a shared root.
pub fn demo_root() -> PathBuf {
    let root = workspace_root().join("target/glpv-demo");
    let specs = glpv_cli::fixtures::collect_specs(&workspace_root().join("demo/projects"));
    assert!(!specs.is_empty(), "demo/projects has no specs");
    glpv_cli::fixtures::build_set(&specs, &root);
    root
}
