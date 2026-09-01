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
  override for the viewer's simulation.

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

## Phase C — `glpv check`: the automated oracle

- New command: for each project, resolve locally and diff (normalised) against
  the server lint API's `merged_yaml`; exit non-zero on divergence.
- Wire it into CI for a set of reference projects so evaluator/merge drift is
  caught as a regression, not a surprise.
- Also in CI: verify `ui/eval-wasm.b64` is fresh (rebuild and compare).

## Phase D — API source (scan without clones)

- `ApiProject` implementing the existing `ProjectSource` trait over the REST
  API (project metadata, refs, raw files, recursive tree, tags), with an
  on-disk immutable blob cache and short-TTL ref cache.
- Transports: direct HTTP with a token, and a `glab api` fallback for OAuth
  setups. Token discovery: flag > environment > glab config.
- Unlocks: `include:remote` fetching, `include:template` without a local
  gitlab clone, CI/CD catalog resolution for `~latest` and shorthand component
  versions, and `--clone-missing` to backfill local clones for offline reruns.

## Phase E — `glpv serve --watch`

- Small local server: re-scan on file change, push a reload (SSE or
  websocket) to the open viewer. Turns the report into a live companion while
  editing CI YAML.

## Phase F — public mirror

- Create the public GitHub repository and add it as a `mirror` remote; push
  the same history there (manually at first, or as a push mirror configured on
  the primary).
- Add the public URL as the workspace `repository` field and to the README.
- Standing policy, enforced before every push: committed content must contain
  no private hostnames or infrastructure references — demo fixtures and
  examples use `gitlab.example.com` and other neutral placeholders.

## Smaller known gaps

- Rule-level `variables:` set by a matching rule are not applied to the job's
  evaluation context.
- Legacy `only:`/`except:` with conditions beyond refs (`variables:`,
  `kubernetes`) evaluate to *unknown*.
- `trigger:forward` (`pipeline_variables: false`) is not simulated per edge —
  simulation variables apply globally.
- Dynamic child pipelines remain opaque by design (their YAML does not exist
  until runtime); the card says exactly which job generates them.
- Scenario finder sentinel: "any other value" assumes it does not match the
  regex patterns present in the rules.
