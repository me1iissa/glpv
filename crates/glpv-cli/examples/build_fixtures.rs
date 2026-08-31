//! Materialise fixture/demo repositories from their specs.
//!
//! Usage: `cargo run -p glpv-cli --example build_fixtures -- <spec-dir> <out-dir>`
//! e.g.   `cargo run -p glpv-cli --example build_fixtures -- demo/projects target/glpv-demo`

fn main() {
    let mut args = std::env::args().skip(1);
    let spec_dir = std::path::PathBuf::from(
        args.next()
            .expect("usage: build_fixtures <spec-dir> <out-dir>"),
    );
    let out_dir = std::path::PathBuf::from(
        args.next()
            .expect("usage: build_fixtures <spec-dir> <out-dir>"),
    );
    let specs = glpv_cli::fixtures::collect_specs(&spec_dir);
    assert!(
        !specs.is_empty(),
        "no specs found in {}",
        spec_dir.display()
    );
    for dir in glpv_cli::fixtures::build_set(&specs, &out_dir) {
        println!("built {}", dir.display());
    }
}
