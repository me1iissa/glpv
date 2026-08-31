# GitLab CI semantics contract

The rules `glpv` implements, pinned to **GitLab 18.x**. Each rule was verified
against docs.gitlab.com and/or the `gitlab-org/gitlab` implementation (paths
cited are in that repository). When behaviour is ambiguous, `glpv check`
(M5) diffs our merged output against `GET /projects/:id/ci/lint` as the oracle.

Verified end-to-end 2026-08-31: `glpv resolve` output is structurally identical
to `glab ci config compile` (the server-side resolver on GitLab 18.9.1-ee) for
four real single-file projects, modulo `.pre`/`.post` appearing in the server's
`stages` list.

## Processing order

From `lib/gitlab/ci/config.rb#build_config`:

1. `spec:inputs` interpolation (`$[[ inputs.x ]]`; the `spec` header is a
   separate first YAML document, `MAX_DOCUMENTS = 2`)
2. `include` expansion and merge (`Config::External::Processor`)
3. `extends` resolution (`Config::Extendable`)
4. `!reference` resolution (`Config::Yaml::Tags::Resolver`)
5. `.pre`/`.post` edge-stage injection (`Config::Stages`)

Then (our side): classify top level → parallel expansion → job building →
rules summarisation/evaluation.

## YAML (Psych) semantics

GitLab parses with `YAML.safe_load(permitted_classes: [Symbol], aliases: true)`
over Psych/libyaml (`lib/gitlab/config/loader/yaml.rb`):

- YAML 1.1 scalars with Psych quirks: `yes/no/on/off` are booleans
  **case-insensitively** (`yES` is true); leading-zero integers are octal
  (`017` → 15); `_` and `,` are digit separators (`1,000` → 1000); `1:30` is
  sexagesimal (90); floats require a signed exponent (`1.5e3` is a string).
- Unquoted dates/times raise `DisallowedClass` — the whole config is invalid.
  We type them as strings and emit an error diagnostic instead of dying.
- `:sym` scalars are Ruby Symbols; we keep the raw text (info diagnostic).
- `<<` merge keys use Psych `revive_hash` semantics, **not** the YAML spec:
  merged values are written at the position of `<<` and clobber keys written
  *before* it; keys after `<<` win; a sequence merges in reverse (earlier
  elements win among themselves).
- Duplicate keys are tolerated: last value wins, first position kept.
- Anchors/aliases are file-local. `!reference` is a custom sequence tag.
- Depth limit 100 (`max_yaml_depth`).

## `include`

Sources: <https://docs.gitlab.com/ci/yaml/includes/>,
`lib/gitlab/ci/config/external/mapper.rb` (Normalizer → Filter →
LocationExpander → VariablesExpander → Matcher → Verifier) and
`external/file/{local,project,remote,template,component,artifact}.rb`.

- Forms: bare string (`http(s)://…` → remote, else local), `local`,
  `project` + `file` (string or array) + optional `ref`, `remote`
  (+`integrity`, `cache`), `template`, `component`. Modifiers: `rules`
  (only `if`/`exists`/`changes`), `inputs`. Exactly one form per entry.
- `include:local` is **repository-root-relative** (leading `/` stripped),
  same ref. Globs `*`/`**` use GitLab's regex translation (escape, then
  `\*\*/` → `(.*?/)?`, `\*\*` → `.*?`, `\*` → `([^/])*?`), matched against the
  recursive tree listing in `ls-tree` order, excluding the including file.
- **Context switch**: inside a file fetched from another project or component,
  nested `local` includes and `rules:exists` resolve in *that* project at
  *that* sha. The resolver carries a `(project, sha)` frame stack.
- `include:project` default `ref` is `HEAD` (the target's default branch);
  project paths match case-insensitively; leading `/` stripped from `file`.
  The host is the including project's host.
- `include:template: X` → `lib/gitlab/ci/templates/X` at the instance's
  version (API: `GET /templates/gitlab_ci_ymls/:key`).
- `include:component: <fqdn>/<project-path>/<name>@<version>`: split at the
  **last** `/`; file `templates/<name>.yml`, else `templates/<name>/template.yml`;
  version precedence catalog release → tag → branch/sha; `~latest` and
  numeric shorthands (`1`, `1.2`) resolve only through the catalog API.
  Components are instance-local.
- Variables in include locations: predefined/project/group/instance only —
  never job variables. Unexpandable `$VAR` → first-class unresolved node.
- **No de-duplication**: the same file included twice merges twice (later
  wins) and consumes two of the budget. Budget: 150 fetched files per
  pipeline (`ci_max_includes`), 30 s resolution timeout, ~314 MB total YAML.
- Merge precedence: includes merge in order (later include wins), then the
  **including file wins over all its includes**. Deep-merge for hashes,
  **replacement for arrays** (`stages`, `script`, `rules`, `needs`, …).

## `extends` and `!reference`

- `extends`: reverse deep merge; multiple bases merge in order with later
  bases winning, then the extending job wins; arrays replaced; an explicit
  `null` **removes** the inherited key; ≤ 11 levels; works across included
  files. GitLab's merged output keeps the inert `extends` key — so do we.
- `!reference [a, b, …]`: resolved after `extends` against the merged root;
  transitive (≤ 10 levels) with cycle detection; a reference inside a
  sequence that resolves to a sequence is spliced flat; missing → error.

## Top level

Not jobs: `default include stages variables workflow spec image services
cache before_script after_script` (`entry/root.rb::ALLOWED_KEYS` + `spec`).
Top-level `image`/`services`/`cache`/`before_script`/`after_script` are
deprecated legacy globals folded into `default:`. `.`-prefixed keys are hidden
templates. `pages` **is** a job. Any other mapping-valued key is a job.

Stages: `.pre` + (`stages` or `build test deploy`) + `.post`; a job without
`stage` lands in `test`; a pipeline whose only jobs sit in `.pre`/`.post`
is not created.

## Jobs

- `needs` targets must exist (unless `optional: true`) and be in the **same
  or an earlier stage**; `needs: []` starts immediately; ≤ 50 needs.
  `needs:project` is a cross-project *artifact* edge (not a trigger);
  `needs:pipeline` reads artifacts from the parent/upstream pipeline.
- `parallel: N` names jobs `name I/N`; `parallel:matrix` names them
  `name: [v1, v2]` (cartesian per matrix entry, values joined `", "`).
  `needs` may target expanded names, a base name (= all expansions) or a
  `parallel:matrix` subset.
- Legacy `only`/`except`: a job with neither `rules` nor `only/except`
  implicitly has `only: [branches, tags]`; `rules` cannot be combined with
  `only/except`.
- `when: manual` **outside** `rules` → `allow_failure: true` (optional,
  non-blocking); **inside** `rules` → `allow_failure: false` (blocking).

## `trigger`

`entry/trigger.rb`, `entry/bridge.rb`, `models/ci/bridge.rb`:

- Grammar: bare string → `{project: …}`. Cross-project keys:
  `project branch strategy forward inputs`. Parent-child keys:
  `strategy include forward`, `include` ≤ 3 entries.
- The child pipeline's config is literally the synthetic document
  `include: <the trigger:include entries>`, resolved in the **parent project
  at the parent sha**. Child depth ≤ 2 (parent → child → grandchild). Child
  pipelines get a fresh include budget.
- `trigger:include: [{artifact: f.yml, job: gen}]` = dynamic child pipeline —
  statically unresolvable, modelled as an opaque node.
- Cross-project: `project`/`branch` expand with **job-scope** variables;
  `branch` defaults to the downstream project's default branch; the
  downstream project runs **its own** entry config (`ci_config_path`);
  `CI_PIPELINE_SOURCE` is `pipeline` downstream (`parent_pipeline` for
  children). No depth cap; cycles are legal — bounded only by the
  1000-pipeline hierarchy limit; we keep a visited set and render cycles.
- `strategy` ∈ {`depend`, `mirror`}. `forward` defaults:
  `yaml_variables: true`, `pipeline_variables: false`.
- A trigger job cannot have `script`; its `needs` may reference at most one
  other bridge and never `needs:project`.
- **No GitLab API resolves triggers statically** — traversal is always ours.

## Entry point

`.gitlab-ci.yml` at the repo root unless the project's `ci_config_path` says
otherwise; that setting takes the forms `path/file.yml`,
`path/file.yml@group/project[:ref]`, or an external `.yml` URL. No config file
+ Auto DevOps enabled ⇒ the effective config is the
`Auto-DevOps.gitlab-ci.yml` template.

## `CI_PIPELINE_SOURCE` values

`push schedule merge_request_event web api trigger pipeline chat webide
external external_pull_request_event parent_pipeline ondemand_dast_scan
ondemand_dast_validation security_orchestration_policy`

## `rules:if` expressions (M4)

Lexer per `lib/gitlab/ci/pipeline/expression/lexer.rb`: `( )`,
`$VAR` (`${}` is invalid here), `"…"`/`'…'` (no escapes),
`/regex/[ismU]` (RE2 via `Gitlab::UntrustedRegexp`), `null`, `true|false`,
`== != =~ !~` then `&&` then `||`, unary `!`; ≤ 100 tokens; syntax errors
evaluate to false. Ruby value semantics: `&&`/`||` return operands; the
statement result is `present?` (nil/false/blank-string are falsy). `=~`
coerces a nil left side to `""`; the right side must be a regex literal or a
variable whose value has `/…/flags` form.
