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

## Phase A — real `rules:changes` evaluation

Today `changes:` clauses are decided by a simulation-wide match / no-match
assumption. Replace that with a real diff:

- `glpv scan --diff <base>[..<head>]` (per root pipeline): compute the changed
  file list via `git diff --name-only` in the pipeline's project at its sha;
  store it on the pipeline in the graph JSON.
- Evaluate each `changes:` clause against its actual glob patterns with the
  existing `glob_to_regex`; the `ChangesChecker` hook already exists in
  `EvalContext` (native and wasm both consume it).
- `changes:compare_to` semantics: resolve the named ref and diff against it
  instead of the default base.
- Viewer: a "diff" control in the sim bar — pick a base ref (re-diffed at scan
  time and embedded, or entered as an explicit changed-files list); per-clause
  ✓/✗ replaces the global assumption wherever a diff is available. The
  assumption selects stay as the fallback when no diff was provided.
- The outcome explorer and scenario finder then branch on real per-clause
  results instead of one global atom.

## Phase B — viewer navigation and shareable state

- **Job search**: a topbar search box with fuzzy matching over job and
  pipeline names; enter jumps (flyTo + select), with a result dropdown for
  ambiguous matches. This is the last everyday gap on very large maps.
- **Shareable state**: encode simulation (source/ref/tag/vars/assumptions),
  selection, edge mode, and camera into the URL hash; restore on load. A
  scanned HTML plus a hash becomes a reproducible "look at this" link.
- **Stack collapse**: an optional toggle that collapses runs of near-identical
  sibling child pipelines (e.g. 37 per-gem children) into one expandable
  stacked card.

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
