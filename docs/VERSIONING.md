# Versioning

glpv follows [Semantic Versioning 2.0.0](https://semver.org) strictly.

## What the version covers

The public interface is everything a user or a script can depend on:

- **The CLI**: commands, flags and their meaning, exit codes, the names and
  layout of the files a scan writes (`index.html`, `graph.json`, `graph.dot`,
  `mermaid/`).
- **The graph JSON**: its structure and `schema_version`. Adding an optional
  field is compatible; renaming, removing or changing the meaning of a field
  is a breaking change and bumps `schema_version`.
- **The viewer's URL state** (`#` fragment, `v` key): links must keep working
  across compatible releases.
- **Evaluation semantics**: a change that makes glpv agree *more* closely with
  GitLab is a fix; a deliberate change of documented behaviour is a feature or
  a breaking change, and is called out in the changelog.

The Rust crates are not published to a registry; their APIs are internal.

## How a version is chosen

Commits follow the [Conventional Commits](https://www.conventionalcommits.org)
prefixes (`feat`, `fix`, `docs`, `test`, `ci`, `refactor`, `perf`, `chore`;
`ui:` counts as a feature). A breaking change is marked with `!` after the
type (`feat!: …`) or a `BREAKING CHANGE:` footer.

The next version is computed from the commits since the last tag — nobody
picks it by hand:

```console
$ git cliff --bumped-version      # e.g. v0.2.0
```

| since the last tag              | bump                         |
|---------------------------------|------------------------------|
| a breaking change               | MAJOR (MINOR while `0.y.z`)  |
| a feature (`feat`, `ui`)        | MINOR                        |
| anything else (`fix`, `docs`…) | PATCH                        |

While the major version is `0`, the public interface may still change between
minor versions (SemVer §4); every such change is listed under the version's
"Features" or as a breaking change in `CHANGELOG.md`. `1.0.0` will be cut when
the CLI, the graph JSON and the URL state are considered stable.

## Enforcement

- `git cliff --bumped-version` is the source of truth; the release checklist
  uses it, and both release pipelines (the primary CI's publish job and the
  GitHub release workflow) refuse a tag that differs from it or from the
  workspace `version` in `Cargo.toml`.
- `glpv --version` reports the workspace version.

## Release checklist

```console
$ V=$(git cliff --bumped-version)                 # vX.Y.Z
$ git cliff --tag "$V" -o CHANGELOG.md
$ sed -i "s/^version = \".*\"/version = \"${V#v}\"/" Cargo.toml && cargo check -q
$ git commit -am "release: $V"
$ git tag -a "$V" -m "glpv $V"
$ git push origin master "$V"                      # primary CI builds and publishes
$ git mirror-push                                  # GitHub release workflow runs on the tag
```
