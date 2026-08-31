//! `include:` expansion across projects. Every include form resolves against
//! an explicit frame `(project, tree)`; fetching a file from another project
//! switches the frame, so nested `include:local` and `rules:exists` inside it
//! resolve in *that* project at *that* sha — GitLab's context rules.
//!
//! Whatever cannot be followed (unknown variables, missing clones, catalog-only
//! component versions, remote URLs, artifact-generated configs) becomes a
//! first-class unresolved node, never a silent drop.

use std::sync::Arc;

use glpv_yaml::{Kind, Node};
use indexmap::IndexMap;

use crate::glob::{glob_to_regex, is_glob};
use crate::model::{self, IncludeKind, Severity, Unresolved, UnresolvedReason};
use crate::resolve::context::{Frame, ResolveState, StackKey};
use crate::resolve::document::load_document;
use crate::resolve::merge::merge;
use crate::source::{FileOrigin, ProjectKey, ProjectSource, TreeRef};
use crate::vars::VarTable;

pub struct IncludeSpec {
    pub kind: SpecKind,
    pub rules: Option<Node>,
    pub inputs: IndexMap<String, Node>,
    pub span: glpv_yaml::Span,
    pub location: String,
}

pub enum SpecKind {
    Local(String),
    Project {
        project: String,
        git_ref: Option<String>,
        files: Vec<String>,
    },
    Remote(String),
    Template(String),
    Component(String),
    /// `trigger:include`-only form; unresolvable statically.
    Artifact {
        artifact: String,
        job: String,
    },
    Invalid(String),
}

/// Expand the `include:` key of `body` (removing it), merging included content
/// under the body per GitLab precedence. Returns the merged document.
pub fn expand_includes(
    st: &mut ResolveState<'_>,
    mut body: Node,
    frame: &Frame,
    vars: &VarTable,
) -> Node {
    let include_node = match body.as_map_mut() {
        Some(map) => match map.entries.shift_remove("include") {
            Some(entry) => entry.value,
            None => return body,
        },
        None => return body,
    };
    expand_include_node(st, body, &include_node, frame, vars)
}

/// Same as [`expand_includes`] but with the include list supplied directly —
/// used for the synthetic `include:` document of a `trigger:include` child.
pub fn expand_include_node(
    st: &mut ResolveState<'_>,
    body: Node,
    include_node: &Node,
    frame: &Frame,
    vars: &VarTable,
) -> Node {
    let specs = normalize(st, include_node);
    let mut acc: Option<Node> = None;
    for spec in specs {
        match evaluate_include_rules(st, frame, &spec) {
            RulesOutcome::Skip => continue,
            RulesOutcome::Unknown => st.diag_at(
                Severity::Info,
                "include.rules-undecidable",
                format!(
                    "rules on `include: {}` cannot be decided statically; the file is included",
                    spec.location
                ),
                Some(spec.span.into()),
            ),
            RulesOutcome::Include => {}
        }
        for node in expand_spec(st, frame, vars, &spec) {
            acc = Some(match acc {
                None => node,
                Some(a) => merge(a, node),
            });
        }
    }

    match acc {
        None => body,
        Some(a) => merge(a, body),
    }
}

fn normalize(st: &mut ResolveState<'_>, node: &Node) -> Vec<IncludeSpec> {
    let entries: Vec<&Node> = match &node.untag().kind {
        Kind::Seq(items) => items.iter().collect(),
        _ => vec![node],
    };
    entries.iter().map(|e| normalize_entry(st, e)).collect()
}

fn normalize_entry(st: &mut ResolveState<'_>, node: &Node) -> IncludeSpec {
    let span = node.span;
    if let Some(text) = node.untag().scalar_text() {
        let kind = if text.starts_with("http://") || text.starts_with("https://") {
            SpecKind::Remote(text.clone())
        } else {
            SpecKind::Local(text.clone())
        };
        return IncludeSpec {
            kind,
            rules: None,
            inputs: IndexMap::new(),
            span,
            location: text,
        };
    }

    let Some(map) = node.untag().as_map() else {
        st.diag_at(
            Severity::Error,
            "include.invalid",
            "an `include` entry must be a string or a mapping",
            Some(span.into()),
        );
        return IncludeSpec {
            kind: SpecKind::Invalid("not a string or mapping".into()),
            rules: None,
            inputs: IndexMap::new(),
            span,
            location: "<invalid>".into(),
        };
    };

    let rules = map.get("rules").cloned();
    let inputs = map
        .get("inputs")
        .and_then(|n| n.as_map())
        .map(|m| {
            m.iter()
                .map(|(k, e)| (k.to_string(), e.value.clone()))
                .collect::<IndexMap<String, Node>>()
        })
        .unwrap_or_default();

    let type_keys: Vec<&str> = [
        "local",
        "project",
        "remote",
        "template",
        "component",
        "artifact",
    ]
    .into_iter()
    .filter(|k| map.contains_key(k))
    .collect();
    if type_keys.len() != 1 {
        st.diag_at(
            Severity::Error,
            "include.ambiguous",
            "each include must use exactly one of: local, project, remote, template, component, artifact",
            Some(span.into()),
        );
        return IncludeSpec {
            kind: SpecKind::Invalid("ambiguous include type".into()),
            rules,
            inputs,
            span,
            location: "<invalid>".into(),
        };
    }

    let text_of = |key: &str| {
        map.get(key)
            .and_then(|n| n.scalar_text())
            .unwrap_or_default()
    };
    let (kind, location) = match type_keys[0] {
        "local" => {
            let p = text_of("local");
            (SpecKind::Local(p.clone()), p)
        }
        "remote" => {
            let u = text_of("remote");
            (SpecKind::Remote(u.clone()), u)
        }
        "template" => {
            let t = text_of("template");
            (SpecKind::Template(t.clone()), t)
        }
        "component" => {
            let c = text_of("component");
            (SpecKind::Component(c.clone()), c)
        }
        "artifact" => {
            let artifact = text_of("artifact");
            let job = text_of("job");
            let location = format!("artifact: {artifact} (job: {job})");
            (SpecKind::Artifact { artifact, job }, location)
        }
        "project" => {
            let project = text_of("project");
            let git_ref = map.get("ref").and_then(|n| n.scalar_text());
            let files: Vec<String> = match map.get("file").map(|f| &f.untag().kind) {
                Some(Kind::Seq(items)) => items.iter().filter_map(|i| i.scalar_text()).collect(),
                Some(_) => map
                    .get("file")
                    .and_then(|f| f.scalar_text())
                    .into_iter()
                    .collect(),
                None => {
                    st.diag_at(
                        Severity::Error,
                        "include.invalid",
                        "`include:project` requires a `file`",
                        Some(span.into()),
                    );
                    Vec::new()
                }
            };
            let location = format!(
                "{}/{}@{}",
                project,
                files.join(","),
                git_ref.as_deref().unwrap_or("HEAD")
            );
            (
                SpecKind::Project {
                    project,
                    git_ref,
                    files,
                },
                location,
            )
        }
        _ => unreachable!(),
    };
    IncludeSpec {
        kind,
        rules,
        inputs,
        span,
        location,
    }
}

enum RulesOutcome {
    Include,
    Skip,
    Unknown,
}

/// Static `include:rules` evaluation: `exists` clauses evaluate against the
/// current frame; `if`/`changes` are undecidable until the M4 evaluator and
/// make the outcome Unknown (conservatively included).
fn evaluate_include_rules(
    st: &mut ResolveState<'_>,
    frame: &Frame,
    spec: &IncludeSpec,
) -> RulesOutcome {
    let Some(rules) = &spec.rules else {
        return RulesOutcome::Include;
    };
    let Some(clauses) = rules.untag().as_seq() else {
        return RulesOutcome::Unknown;
    };

    for clause in clauses {
        let Some(m) = clause.untag().as_map() else {
            continue;
        };
        let mut result = Some(true); // None = unknown
        if m.contains_key("if") || m.contains_key("changes") {
            result = None;
        }
        if let Some(exists) = m.get("exists") {
            let patterns: Vec<String> = match &exists.untag().kind {
                Kind::Seq(items) => items.iter().filter_map(|i| i.scalar_text()).collect(),
                _ => exists.scalar_text().into_iter().collect(),
            };
            match eval_exists(st, frame, &patterns, spec) {
                Some(true) => {}
                Some(false) => result = Some(false),
                None => result = result.and(None),
            }
        }
        let when_never = m
            .get("when")
            .and_then(|w| w.scalar_text())
            .is_some_and(|w| w == "never");
        match result {
            Some(true) => {
                return if when_never {
                    RulesOutcome::Skip
                } else {
                    RulesOutcome::Include
                };
            }
            Some(false) => continue,
            None => return RulesOutcome::Unknown,
        }
    }
    RulesOutcome::Skip // no rule matched → the include does not happen
}

fn eval_exists(
    st: &mut ResolveState<'_>,
    frame: &Frame,
    patterns: &[String],
    spec: &IncludeSpec,
) -> Option<bool> {
    match frame.project.list_tree(&frame.tree) {
        Ok(listing) => Some(patterns.iter().any(|p| {
            let re = glob_to_regex(p.trim_start_matches('/'));
            listing.iter().any(|f| re.is_match(f))
        })),
        Err(e) => {
            st.diag_at(
                Severity::Warning,
                "source.error",
                format!(
                    "cannot evaluate `exists` for `include: {}`: {e}",
                    spec.location
                ),
                Some(spec.span.into()),
            );
            None
        }
    }
}

/// Resolve one include spec into zero or more fully-expanded documents.
fn expand_spec(
    st: &mut ResolveState<'_>,
    frame: &Frame,
    vars: &VarTable,
    spec: &IncludeSpec,
) -> Vec<Node> {
    match &spec.kind {
        SpecKind::Invalid(reason) => {
            record_unresolved(
                st,
                spec,
                IncludeKind::Local,
                UnresolvedReason::InvalidConfig,
                reason.clone(),
                None,
            );
            Vec::new()
        }
        SpecKind::Artifact { artifact, job } => {
            record_unresolved(
                st,
                spec,
                IncludeKind::Synthetic,
                UnresolvedReason::DynamicChild,
                format!(
                    "configuration is generated at runtime by job `{job}` (artifact `{artifact}`)"
                ),
                None,
            );
            Vec::new()
        }
        SpecKind::Local(path) => {
            let Some(rel) = expand_location(st, spec, vars, path, IncludeKind::Local) else {
                return Vec::new();
            };
            let rel = rel.trim_start_matches('/').to_string();
            let targets = glob_targets(
                st,
                frame.project.as_ref(),
                &frame.tree,
                &rel,
                Some(&frame.file_path),
                spec,
            );
            targets
                .into_iter()
                .filter_map(|t| {
                    fetch_file(
                        st,
                        frame,
                        vars,
                        frame.project.clone(),
                        frame.tree.clone(),
                        &t,
                        spec,
                        IncludeKind::Local,
                    )
                })
                .collect()
        }
        SpecKind::Project {
            project,
            git_ref,
            files,
        } => {
            let Some(project_path) = expand_location(st, spec, vars, project, IncludeKind::Project)
            else {
                return Vec::new();
            };
            let git_ref = match git_ref {
                Some(r) => match expand_location(st, spec, vars, r, IncludeKind::Project) {
                    Some(r) => Some(r),
                    None => return Vec::new(),
                },
                None => None,
            };
            let host = frame.project.meta().key.host.clone();
            let key = ProjectKey::new(&host, &project_path);
            let target = match st.sources.locate(&key) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    record_unresolved(
                        st,
                        spec,
                        IncludeKind::Project,
                        UnresolvedReason::ProjectNotFound,
                        format!("no clone of {host}/{project_path} in the project index"),
                        Some(format!(
                            "clone it into the projects folder, e.g. `git clone git@{host}:{project_path}.git`"
                        )),
                    );
                    return Vec::new();
                }
                Err(e) => {
                    record_unresolved(
                        st,
                        spec,
                        IncludeKind::Project,
                        UnresolvedReason::ProjectNotFound,
                        e.to_string(),
                        None,
                    );
                    return Vec::new();
                }
            };
            let Some(tree) = resolve_project_tree(
                st,
                spec,
                target.as_ref(),
                git_ref.as_deref(),
                IncludeKind::Project,
            ) else {
                return Vec::new();
            };
            let mut out = Vec::new();
            for file in files {
                let Some(file) = expand_location(st, spec, vars, file, IncludeKind::Project) else {
                    continue;
                };
                let rel = file.trim_start_matches('/').to_string();
                for t in glob_targets(st, target.as_ref(), &tree, &rel, None, spec) {
                    if let Some(node) = fetch_file(
                        st,
                        frame,
                        vars,
                        target.clone(),
                        tree.clone(),
                        &t,
                        spec,
                        IncludeKind::Project,
                    ) {
                        out.push(node);
                    }
                }
            }
            out
        }
        SpecKind::Template(name) => {
            let Some(name) = expand_location(st, spec, vars, name, IncludeKind::Template) else {
                return Vec::new();
            };
            let key = st.sources.templates_key.clone();
            let target = match st.sources.locate(&key) {
                Ok(Some(p)) => p,
                _ => {
                    record_unresolved(
                        st,
                        spec,
                        IncludeKind::Template,
                        UnresolvedReason::TemplateUnavailable,
                        format!(
                            "template `{name}` needs a clone of {}/{}",
                            key.host, key.path_lc
                        ),
                        Some(
                            "clone gitlab-org/gitlab into the projects folder (or set \
                             defaults.templates_from in glpv.toml); the API source in M5 \
                             removes this requirement"
                                .to_string(),
                        ),
                    );
                    return Vec::new();
                }
            };
            let Some(tree) =
                resolve_project_tree(st, spec, target.as_ref(), None, IncludeKind::Template)
            else {
                return Vec::new();
            };
            let path = format!("lib/gitlab/ci/templates/{name}");
            fetch_file(
                st,
                frame,
                vars,
                target,
                tree,
                &path,
                spec,
                IncludeKind::Template,
            )
            .into_iter()
            .collect()
        }
        SpecKind::Component(addr) => {
            let Some(addr) = expand_location(st, spec, vars, addr, IncludeKind::Component) else {
                return Vec::new();
            };
            expand_component(st, frame, vars, spec, &addr)
                .into_iter()
                .collect()
        }
        SpecKind::Remote(url) => {
            let (reason, detail, hint) = if st.opts.allow_remote {
                (
                    UnresolvedReason::NotYetImplemented,
                    format!("remote include `{url}` (HTTP fetching arrives in M5)"),
                    None,
                )
            } else {
                (
                    UnresolvedReason::RemoteDisabled,
                    format!("remote include `{url}`"),
                    Some("pass --allow-remote to fetch remote includes (M5)".to_string()),
                )
            };
            record_unresolved(st, spec, IncludeKind::Remote, reason, detail, hint);
            Vec::new()
        }
    }
}

/// `<host>/<project-path>/<component-name>@<version>` → resolved template file.
fn expand_component(
    st: &mut ResolveState<'_>,
    frame: &Frame,
    vars: &VarTable,
    spec: &IncludeSpec,
    addr: &str,
) -> Option<Node> {
    let Some((address, version)) = addr.rsplit_once('@') else {
        record_unresolved(
            st,
            spec,
            IncludeKind::Component,
            UnresolvedReason::InvalidConfig,
            format!("component `{addr}` is missing an `@version`"),
            None,
        );
        return None;
    };
    let Some((host, rest)) = address.split_once('/') else {
        record_unresolved(
            st,
            spec,
            IncludeKind::Component,
            UnresolvedReason::InvalidConfig,
            format!("component `{addr}` must look like <host>/<project>/<name>@<version>"),
            None,
        );
        return None;
    };
    let Some((project_path, name)) = rest.rsplit_once('/') else {
        record_unresolved(
            st,
            spec,
            IncludeKind::Component,
            UnresolvedReason::InvalidConfig,
            format!("component `{addr}` has no project path"),
            None,
        );
        return None;
    };

    let key = ProjectKey::new(host, project_path);
    let target = match st.sources.locate(&key) {
        Ok(Some(p)) => p,
        _ => {
            record_unresolved(
                st,
                spec,
                IncludeKind::Component,
                UnresolvedReason::ProjectNotFound,
                format!("no clone of {host}/{project_path} in the project index"),
                Some(format!(
                    "clone it: `git clone git@{host}:{project_path}.git`"
                )),
            );
            return None;
        }
    };

    // Version precedence: catalog release → tag → branch/sha. Locally we can
    // resolve exact refs; `~latest` and numeric shorthands need the catalog.
    let sha = match target.resolve_ref(version) {
        Ok(Some(sha)) => sha,
        _ => {
            let is_catalog_form = version == "~latest"
                || (version.chars().all(|c| c.is_ascii_digit() || c == '.')
                    && version.split('.').count() <= 2);
            let (reason, hint) = if is_catalog_form {
                (
                    UnresolvedReason::ComponentNeedsCatalog,
                    Some(
                        "catalog versions (`~latest`, `1`, `1.2`) resolve through the \
                         CI/CD Catalog API (M5); pin an exact tag to resolve locally"
                            .to_string(),
                    ),
                )
            } else {
                (UnresolvedReason::RefNotFound, None)
            };
            record_unresolved(
                st,
                spec,
                IncludeKind::Component,
                reason,
                format!("cannot resolve component version `{version}` of {host}/{project_path}"),
                hint,
            );
            return None;
        }
    };
    let tree = TreeRef::Commit(sha);

    let candidates = [
        format!("templates/{name}.yml"),
        format!("templates/{name}/template.yml"),
    ];
    for path in &candidates {
        match target.exists(&tree, path) {
            Ok(true) => {
                return fetch_file(
                    st,
                    frame,
                    vars,
                    target.clone(),
                    tree,
                    path,
                    spec,
                    IncludeKind::Component,
                );
            }
            _ => continue,
        }
    }
    record_unresolved(
        st,
        spec,
        IncludeKind::Component,
        UnresolvedReason::FileNotFound,
        format!(
            "{host}/{project_path}@{version} has neither templates/{name}.yml nor templates/{name}/template.yml"
        ),
        None,
    );
    None
}

/// Expand `$VARS` in an include location; unknown variables make the whole
/// include unresolved (GitLab only allows the pre-pipeline variable set here).
fn expand_location(
    st: &mut ResolveState<'_>,
    spec: &IncludeSpec,
    vars: &VarTable,
    text: &str,
    kind: IncludeKind,
) -> Option<String> {
    match vars.expand(text) {
        Ok(t) => Some(t),
        Err(missing) => {
            record_unresolved(
                st,
                spec,
                kind,
                UnresolvedReason::VariableInLocation,
                format!("cannot expand ${} in `{text}`", missing.join(", $")),
                Some(
                    "project/group CI variables are invisible to a static crawl; \
                     pass --var NAME=value to supply one"
                        .to_string(),
                ),
            );
            None
        }
    }
}

fn glob_targets(
    st: &mut ResolveState<'_>,
    project: &dyn ProjectSource,
    tree: &TreeRef,
    rel: &str,
    exclude: Option<&str>,
    spec: &IncludeSpec,
) -> Vec<String> {
    if !is_glob(rel) {
        return vec![rel.to_string()];
    }
    match project.list_tree(tree) {
        Ok(listing) => {
            let re = glob_to_regex(rel);
            listing
                .iter()
                .filter(|p| re.is_match(p) && Some(p.as_str()) != exclude)
                .cloned()
                .collect()
        }
        Err(e) => {
            st.diag_at(
                Severity::Error,
                "source.error",
                format!("cannot list files for glob `{rel}`: {e}"),
                Some(spec.span.into()),
            );
            Vec::new()
        }
    }
}

fn resolve_project_tree(
    st: &mut ResolveState<'_>,
    spec: &IncludeSpec,
    project: &dyn ProjectSource,
    git_ref: Option<&str>,
    kind: IncludeKind,
) -> Option<TreeRef> {
    let ref_name = match git_ref {
        Some(r) if r != "HEAD" => r.to_string(),
        _ => match project.default_branch() {
            Ok(b) => b,
            Err(e) => {
                record_unresolved(
                    st,
                    spec,
                    kind,
                    UnresolvedReason::RefNotFound,
                    e.to_string(),
                    None,
                );
                return None;
            }
        },
    };
    match project.resolve_ref(&ref_name) {
        Ok(Some(sha)) => Some(TreeRef::Commit(sha)),
        _ => {
            record_unresolved(
                st,
                spec,
                kind,
                UnresolvedReason::RefNotFound,
                format!(
                    "ref `{ref_name}` not found in {} (fetch the clone?)",
                    project.meta().display_path
                ),
                None,
            );
            None
        }
    }
}

/// Fetch one file in `(project, tree)`, register it, recurse into its own
/// includes with the switched frame, and return the fully-expanded document.
#[allow(clippy::too_many_arguments)]
fn fetch_file(
    st: &mut ResolveState<'_>,
    parent_frame: &Frame,
    vars: &VarTable,
    project: Arc<dyn ProjectSource>,
    tree: TreeRef,
    path: &str,
    spec: &IncludeSpec,
    kind: IncludeKind,
) -> Option<Node> {
    st.budget_used += 1;
    if st.budget_used > st.opts.max_includes {
        record_unresolved_at(
            st,
            spec,
            kind,
            path,
            UnresolvedReason::IncludeBudgetExceeded,
            format!(
                "maximum of {} includes exceeded at `{path}`",
                st.opts.max_includes
            ),
            None,
        );
        return None;
    }

    let meta = project.meta().clone();
    let key = StackKey {
        host: meta.key.host.clone(),
        path_lc: meta.key.path_lc.clone(),
        tree: tree.clone(),
        file_path: path.to_string(),
    };
    if st.stack.contains(&key) {
        record_unresolved_at(
            st,
            spec,
            kind,
            path,
            UnresolvedReason::Cycle,
            format!(
                "`{path}` of {} is already being included (include cycle)",
                meta.display_path
            ),
            None,
        );
        return None;
    }

    let text = match project.read(&tree, path) {
        Ok(Some(t)) => t,
        Ok(None) => {
            record_unresolved_at(
                st,
                spec,
                kind,
                path,
                UnresolvedReason::FileNotFound,
                format!("`{path}` does not exist in {}", meta.display_path),
                None,
            );
            return None;
        }
        Err(e) => {
            record_unresolved_at(
                st,
                spec,
                kind,
                path,
                UnresolvedReason::FileNotFound,
                format!("cannot read `{path}`: {e}"),
                None,
            );
            return None;
        }
    };

    let sha = match &tree {
        TreeRef::Commit(s) => Some(s.0.clone()),
        TreeRef::Worktree => None,
    };
    let file_id = st.files.insert(
        FileOrigin {
            project: Some(meta.project_ref()),
            sha: sha.clone(),
            path: path.to_string(),
        },
        text.clone(),
    );
    st.include_files.push(model::IncludeFile {
        file: file_id.0,
        project: Some(meta.project_ref()),
        sha,
        path: path.to_string(),
        kind,
        location: spec.location.clone(),
        unresolved: None,
    });
    record_edge(st, parent_frame, spec, Some(file_id.0), false);

    let child_frame = Frame {
        project,
        tree,
        file: file_id,
        file_path: path.to_string(),
    };
    // Predefined variables are pipeline-scoped: nested include locations still
    // see the root project's CI_PROJECT_* values even in a switched frame.
    st.stack.push(key);
    let body = load_document(st, file_id, &text, &spec.inputs);
    let result = body.map(|b| expand_includes(st, b, &child_frame, vars));
    st.stack.pop();
    result
}

fn record_edge(
    st: &mut ResolveState<'_>,
    frame: &Frame,
    spec: &IncludeSpec,
    to: Option<u32>,
    cycle: bool,
) {
    let order = st.order_counter;
    st.order_counter += 1;
    st.include_edges.push(model::IncludeEdge {
        from: Some(frame.file.0),
        to,
        location: spec.location.clone(),
        order,
        span: spec.span.into(),
        pipeline: st.pipeline_id.clone(),
        cycle,
    });
}

fn record_unresolved(
    st: &mut ResolveState<'_>,
    spec: &IncludeSpec,
    kind: IncludeKind,
    reason: UnresolvedReason,
    detail: String,
    hint: Option<String>,
) {
    record_unresolved_at(st, spec, kind, &spec.location.clone(), reason, detail, hint);
}

fn record_unresolved_at(
    st: &mut ResolveState<'_>,
    spec: &IncludeSpec,
    kind: IncludeKind,
    path: &str,
    reason: UnresolvedReason,
    detail: String,
    hint: Option<String>,
) {
    let severity = match reason {
        UnresolvedReason::NotYetImplemented
        | UnresolvedReason::TemplateUnavailable
        | UnresolvedReason::RemoteDisabled
        | UnresolvedReason::ComponentNeedsCatalog
        | UnresolvedReason::DynamicChild => Severity::Warning,
        _ => Severity::Error,
    };
    match &hint {
        Some(h) => st.diag_hint(
            severity,
            unresolved_code(reason),
            detail.clone(),
            Some(spec.span.into()),
            h.clone(),
        ),
        None => st.diag_at(
            severity,
            unresolved_code(reason),
            detail.clone(),
            Some(spec.span.into()),
        ),
    }
    st.include_files.push(model::IncludeFile {
        file: u32::MAX,
        project: None,
        sha: None,
        path: path.to_string(),
        kind,
        location: spec.location.clone(),
        unresolved: Some(Unresolved {
            reason,
            detail,
            span: Some(spec.span.into()),
        }),
    });
    let order = st.order_counter;
    st.order_counter += 1;
    st.include_edges.push(model::IncludeEdge {
        from: None,
        to: None,
        location: spec.location.clone(),
        order,
        span: spec.span.into(),
        pipeline: st.pipeline_id.clone(),
        cycle: matches!(reason, UnresolvedReason::Cycle),
    });
}

fn unresolved_code(reason: UnresolvedReason) -> &'static str {
    match reason {
        UnresolvedReason::VariableInLocation => "include.variable-unresolved",
        UnresolvedReason::ProjectNotFound => "include.project-not-found",
        UnresolvedReason::RefNotFound => "include.ref-not-found",
        UnresolvedReason::FileNotFound => "include.file-not-found",
        UnresolvedReason::ComponentNeedsCatalog => "include.component-needs-catalog",
        UnresolvedReason::TemplateUnavailable => "include.template-unavailable",
        UnresolvedReason::RemoteDisabled => "include.remote-disabled",
        UnresolvedReason::RemoteFailed => "include.remote-failed",
        UnresolvedReason::DynamicChild => "trigger.dynamic-child",
        UnresolvedReason::IncludeBudgetExceeded => "include.budget-exceeded",
        UnresolvedReason::ChildDepthExceeded => "trigger.child-depth",
        UnresolvedReason::Cycle => "include.cycle",
        UnresolvedReason::ExtendsDepth => "extends.too-deep",
        UnresolvedReason::ReferenceDepth => "reference.too-deep",
        UnresolvedReason::InvalidConfig => "include.invalid",
        UnresolvedReason::NotYetImplemented => "include.not-yet-implemented",
    }
}
