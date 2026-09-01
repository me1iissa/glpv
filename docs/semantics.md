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
  `only/except`. Evaluated keys: `refs` (or a bare list of refs/keywords),
  `variables` (the same expression language as `rules:if`), `changes` (the
  same glob semantics and push-event rule as `rules:changes`) and
  `kubernetes: active`, which depends on the project's cluster integration
  and is always *unknown*. Per the documented combination rule
  (https://docs.gitlab.com/ci/yaml/#only--except), `only` includes the job
  when every key has a matching entry and `except` excludes it when any key
  has one; both are three-valued here, so an undecidable key makes the job
  *unknown* rather than skipped.
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

## `rules:changes`

Sources: <https://docs.gitlab.com/ci/yaml/#ruleschanges>,
<https://docs.gitlab.com/ci/yaml/#ruleschangescompare_to>,
<https://docs.gitlab.com/ci/yaml/#ruleschangesregexp>,
<https://docs.gitlab.com/ci/jobs/job_rules/>,
<https://docs.gitlab.com/ci/yaml/includes/#include-with-ruleschanges>;
`lib/gitlab/ci/build/rules/rule/clause/changes.rb`,
`lib/gitlab/ci/config/entry/rules/rule/changes.rb`, `app/models/ci/pipeline.rb`
(`changed_paths`, `modified_paths_since`), `app/models/ci/bridge.rb`
(`child_params`).

1. **Evaluation order** (`Clause::Changes#satisfied_by?`): resolve
   `compare_to` (variables expanded; an unresolvable ref rejects the whole
   pipeline); take the modified paths — the diff since the merge base of
   `compare_to`, else the pipeline's `changed_paths`; **no path set ⇒ the
   clause is true**; an empty set ⇒ false; expand variables in the patterns
   with `ExpandVariables.expand_existing` (a variable that does not exist
   stays literal `$NAME`); no patterns ⇒ false; more than 50 000
   `paths × patterns` comparisons ⇒ true; otherwise any path matches any
   pattern via `File.fnmatch?(glob, path, FNM_PATHNAME | FNM_DOTMATCH |
   FNM_EXTGLOB)`. Paths are repository-relative and compared whole: a
   leading `/` in a pattern never matches (glpv warns,
   `rules.changes-leading-slash`).
2. **Push event.** `changed_paths` exists only for a push to an existing
   branch, a merge request pipeline or an external pull request
   (`CI_PIPELINE_SOURCE` ∈ `push` (not a tag), `merge_request_event`,
   `external_pull_request_event`). Tag pushes, new branches (all-zero
   before-sha), `schedule`, `web`, `api`, `trigger`, `pipeline` and every
   other source have none ⇒ every `changes:` clause without `compare_to`
   matches — a `when: never` one included.
3. **Child pipelines inherit the parent's diff** (`Bridge#child_params`
   forwards the before/after shas and the merge request); multi-project
   pipelines get none. `--diff` therefore applies to root pipelines and
   their children; a reclassified `--all` root (found to be triggered) is
   treated as downstream.
4. **`compare_to`** bypasses the push-event rule: the diff is
   `merge_base(compare_to, sha)..sha` (three-dot), so such clauses are
   decidable whenever the clone has the ref — no `--diff` needed. The ref
   may contain variables.
5. **The push diff** is taken without rename detection (a rename is its old
   plus its new path); the merge request diff is against the merge base.
6. **`include:rules`** accept `if`, `exists` and `changes`; `changes` is
   evaluated against the root pipeline's diff, never the include frame's.
7. **`changes: {regexp: …}`** (GitLab 19.2) is exclusive with `paths`. glpv
   parses it but does not match it: an empty diff (or a blanket
   assumption in the viewer) decides it, anything else stays *unknown*.
8. **Ruby `fnmatch`** under those flags, as implemented in
   `glob::changes_glob_to_regex`:

   | pattern element | matches |
   | --- | --- |
   | `**/` at the start of a segment | zero or more directories, hidden ones included; consecutive `**/` collapse |
   | any other run of `*` | within one segment (`[^/]*`): `src/**` is `src/*`, `a**/b` is `a*/b` |
   | `?` | one character other than `/` |
   | `[…]`, `[!…]`, `[^…]` | a class that never matches `/`; reversed ranges match nothing; an unterminated `[` makes the pattern unmatchable |
   | `{a,b}` | brace alternatives, nested, empty ones allowed (`{,jh/}x`); an unmatched `{` matches nothing |
   | `\x` | the literal `x` |
   | anything else | literal; whole-path, anchored |

What `glpv` does (`diff.rs`, `source/local.rs`): `--diff <base>` runs
`git diff --name-only --no-renames <base>...<sha>` in every root project
(for a working-tree scan: `git diff` against the merge base plus
`git ls-files --others --exclude-standard`, so uncommitted and untracked
files count as changed); `--changed-file` supplies the list directly.
`compare_to` refs are diffed lazily and cached per ref. Outcomes carry a
note — `matched by <file>`, `no match in N changed file(s)`, `no push event
for source X; always matches`, `$X unknown` — and the graph JSON records
the lists under `pipelines[].diff` (`base`, `files` on the owning pipeline;
`compare_to` per ref).

Deviations: without `--diff` a plain `changes:` clause under a push /
merge-request scenario stays *unknown* (GitLab always has a diff); a
new-branch push is not simulated as such; the worktree diff counts
untracked files, which a real push would only see once committed; a
`compare_to` ref that does not resolve yields *unknown* plus
`diff.compare-to-unresolved` where GitLab rejects the pipeline. Legacy
`only:changes`/`except:changes` follow the same rules as `rules:changes`
(without `compare_to`).

## `rules:if` expressions (M4)

Lexer per `lib/gitlab/ci/pipeline/expression/lexer.rb`: `( )`,
`$VAR` (`${}` is invalid here), `"…"`/`'…'` (no escapes),
`/regex/[ismU]` (RE2 via `Gitlab::UntrustedRegexp`), `null`, `true|false`,
`== != =~ !~` then `&&` then `||`, unary `!`; ≤ 100 tokens; syntax errors
evaluate to false. Ruby value semantics: `&&`/`||` return operands; the
statement result is `present?` (nil/false/blank-string are falsy). `=~`
coerces a nil left side to `""`; the right side must be a regex literal or a
variable whose value has `/…/flags` form.

## `trigger:forward` and `rules:variables`

Sources: <https://docs.gitlab.com/ci/yaml/#triggerforward>,
<https://docs.gitlab.com/ci/yaml/#rulesvariables>,
<https://docs.gitlab.com/ci/variables/#cicd-variable-precedence>.

- A matched rule's `variables:` become variables of that job (they do not
  influence the evaluation of the job's own rules, which happens first).
  The graph records them per scenario in `evaluations[].variables`.
- A bridge forwards to the pipeline it triggers, per its `trigger:forward`
  (defaults `yaml_variables: true`, `pipeline_variables: false`):
  - with `yaml_variables`: the parent's top-level `variables:`, the bridge's
    own `variables:` and the bridge's matched `rules:variables`;
  - with `pipeline_variables`: the parent's *pipeline* variables — for a root
    pipeline the ones supplied when it was created (the simulation's
    variables here), for a downstream pipeline what it received itself.
- Forwarded variables are pipeline variables of the downstream pipeline: they
  take precedence over that pipeline's YAML variables, as in GitLab.
- The native evaluator, the WebAssembly build and the viewer's JS mirror all
  compute the same inheritance (`trigger_edges[].forward` carries the flags
  in the graph JSON); the parity suite holds them equal.

