# pipelines-demo — a multi-project CI playground

Five interlinked projects that exercise everything the crawler follows:

```
shop ──include:project──────────────▶ ci-templates (templates/rust.yml → nested local)
 │ ├─ trigger:include ▶ child deploy pipeline ─ trigger:include ▶ grandchild smoke pipeline
 │ ├─ trigger:project ▶ infra ──include:project@v2──▶ ci-templates
 │ │                      └─ trigger ▶ monitoring ─ manual rollback ─▶ infra   (cycle)
 │ ├─ trigger:project branch:$ANALYTICS_BRANCH ▶ analytics
 │ │                      ├─ include:component rust@1.0.0 (resolves via tag)
 │ │                      ├─ include:component rust@~latest (needs catalog → unresolved)
 │ │                      └─ dynamic child from a generated artifact (opaque)
 │ └─ trigger pipelines-demo/$REGION-stack   (unknown variable → unresolved)
```

Build the repos and scan:

```console
$ cargo run -p glpv-cli --example build_fixtures -- demo/projects target/glpv-demo
$ cargo run -p glpv-cli -- scan --projects target/glpv-demo --entry pipelines-demo/shop -o out/
```

The specs in `projects/*.toml` are materialised as real git repositories with
deterministic commits; the remotes point at `gitlab.example.com`, so the same
repos can be pushed there as real projects to watch GitLab run them for real.
