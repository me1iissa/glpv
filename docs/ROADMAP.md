# Roadmap

Where things stand: milestones M1–M4 are done — span-preserving YAML layer,
cross-project include resolution, the full trigger walk, the GitLab-faithful
rules engine (native and compiled to WebAssembly for the viewer), the
GPU-rendered interactive viewer with live simulation, invocation chains, the
outcome explorer, and the enable-in-simulation solver. The whole stack is
stress-tested against gitlab-org/gitlab (58 pipelines / 3,167 jobs / 4
projects) and structurally verified against GitLab's own resolver on real
projects.

The phases below are ordered by value; each is meant to be picked up
independently.

## Phase A — real `rules:changes` evaluation (done)

`changes:` clauses are decided against a real diff (`docs/semantics.md`,
"rules:changes"):

- `glpv scan --diff <base>` computes `git diff --name-only --no-renames
  <base>...<sha>` in every root project (a working-tree scan also counts
  uncommitted and untracked files); `--changed-file <path>` supplies an
  explicit list. The list lives on the root pipeline in the graph JSON
  (`diff.files`); child pipelines inherit it, `compare_to` lists sit on the
  pipeline that uses them.
- Patterns match with Ruby `fnmatch` semantics (brace expansion,
  segment-bound `*`, `**/`, classes, `FNM_DOTMATCH`); variables in patterns
  and in `compare_to` expand like GitLab's `expand_existing`.
- `changes:compare_to` diffs against the merge base of the named ref and is
  decidable without `--diff`.
- GitLab's push-event rule: tag pushes, schedules, web/api/trigger runs and
  multi-project pipelines have no changed-paths set, so their plain
  `changes:` clauses always match.
- `include:rules:changes` is decided the same way, against the root's diff.
- The wasm evaluator reads the embedded diff and takes a `changed_files`
  override for the viewer's simulation; the viewer's JS mirror implements the
  same matching, and the two are held equal by the parity test.
- Viewer: a "changed files" list in the sim bar (prefilled from the scanned
  diff, editable as an override that also travels in the URL as `cf`); the
  panel checklist shows each `changes:` clause's verdict ("matched by …",
  "no match in N changed file(s)", "no push event …"); the assumption select
  is disabled while a list is in force, and the outcome explorer / scenario
  finder no longer branch on a decided clause.

Leftovers: `changes: {regexp: …}` is parsed but only decided by an empty
diff; legacy `only:changes`/`except:changes` stay *unknown*; a new-branch
push (no changed-paths set, like a tag) has no scenario flag of its own.

## Phase B — viewer navigation and shareable state

- **Job search** — done: a topbar search box (`/` focuses it) with fuzzy
  matching over job, pipeline and stage names; results ring their pills on
  the board, Enter jumps to a job (and selects it) or fits a pipeline card.
- **Shareable state** — done: the simulation (source/ref/tag/vars/
  assumptions), selection, edge mode and camera are encoded in the URL hash
  as base64url JSON (keys omitted at their defaults, unknown keys ignored, the
  camera as a world-space centre so a link survives another window size) and
  restored on load; "copy link" in the topbar. A scanned HTML plus a hash is
  a reproducible "look at this" link.
- **Stack collapse** — done (reduced scope): a topbar toggle folds runs of
  ≥3 near-identical *leaf* child pipelines (same parent, kind, trigger label,
  stages and job names — e.g. 37 per-gem children) into one stacked card;
  trigger branches route into it, clicking it (or selecting one of its jobs,
  e.g. from a link) expands the group in place with the camera preserved.
  The toggle travels in the URL (`stk`).

## Phase C — `glpv check`: the automated oracle (done)

- `glpv check --file|--entry [--ref]` resolves a project locally and asks
  the server's lint API (`POST /projects/:id/ci/lint` with `dry_run` and
  `include_jobs`) to do the same for the same entry file and ref. Two
  comparisons: the merged configuration (both sides normalised — key order,
  the expanded `include:`, the implicit `.pre`/`.post` stages) as a unified
  diff, and the jobs a push pipeline on that ref would create against the
  local rules evaluation (stage, effective `when`, `allow_failure`, script
  lines; locally undecided jobs are reported, not counted). Exit 0 / 1 / 2 =
  identical / differs / could not check. Transports: `glab api` (default) or
  `curl` with `GLPV_TOKEN`; `--save-oracle` / `--oracle-json` replay a
  response offline.
- CI: a `check` job runs it against reference projects named in the CI
  variable `GLPV_CHECK_PROJECTS` (with `GLPV_API_TOKEN`), so evaluator or
  merge drift fails the pipeline; `samples`/`ui-test` also run the parity
  suite against a wasm build made from source; a scheduled `corpus` job
  rescans gitlab-org/gitlab and asserts the known counts.

## Phase D — API source (scan without clones)

- `ApiProject` implementing the existing `ProjectSource` trait over the REST
  API (project metadata, refs, raw files, recursive tree, tags), with an
  on-disk immutable blob cache and short-TTL ref cache.
- Transports: direct HTTP with a token, and a `glab api` fallback for OAuth
  setups. Token discovery: flag > environment > glab config.
- Unlocks: `include:remote` fetching, `include:template` without a local
  gitlab clone, CI/CD catalog resolution for `~latest` and shorthand component
  versions, and `--clone-missing` to backfill local clones for offline reruns.

## Phase E — `glpv serve --watch` (done)

- `glpv serve` takes every `scan` flag, writes the scan under `--out`, serves
  it at `http://127.0.0.1:7070/` (`--bind`, `--port`) and watches the clone
  roots — or the entry file's repository — with `notify`; after a quiet
  period (`--debounce-ms`, 300 by default) it rescans, rewrites the output and
  pushes `reload` to every open viewer over `/events` (server-sent events).
  The served `index.html` carries a tiny reload script; `location.reload()`
  keeps the URL hash, so the simulation, selection and camera survive an edit.
  A failed rescan keeps serving the previous output and prints the error. The
  server is standard-library only (a thread per connection).

## Phase F — public mirror (done)

- <https://github.com/me1iissa/glpv> carries the same history as the primary
  repository (pushed from the release checklist, never by a bot: the primary
  stays the source of truth). It is the workspace `repository` and linked
  from the README.
- Releases exist on both sides: the primary CI builds and publishes on a
  version tag; `.github/workflows/release.yml` verifies the tag against the
  workspace version, runs the Rust and viewer test suites, builds the same
  two archives, and creates the GitHub Release with `SHA256SUMS` and the
  changelog section as its body (`workflow_dispatch` releases an existing
  tag). `CHANGELOG.md` is generated by git-cliff (`cliff.toml`) before a tag
  is cut.
- Standing policy, enforced by a local pre-push guard on every push:
  committed content — trees, messages, author identities and tag objects —
  contains no private hostnames or infrastructure references; demo fixtures
  and examples use `gitlab.example.com` and other neutral placeholders.

## Smaller known gaps

- Legacy `only:`/`except:` `kubernetes: active` is always *unknown* (it
  depends on the project's cluster integration, which is not static).
- Dynamic child pipelines remain opaque by design (their YAML does not exist
  until runtime); the card says exactly which job generates them.
- Scenario finder sentinel: "any other value" assumes it does not match the
  regex patterns present in the rules.
