//! Loading one YAML document: the two-document `spec:` header split and
//! `$[[ inputs.x ]]` interpolation.

use std::sync::LazyLock;

use glpv_yaml::{FileId, Kind, Node, Scalar, ScalarStyle, Value};
use indexmap::IndexMap;
use regex::Regex;

use crate::model::Severity;
use crate::resolve::context::ResolveState;

static INPUT_REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\[\[\s*inputs\.([A-Za-z0-9_-]+)\s*((?:\|[^\]]*)?)\]\]").unwrap()
});

/// Parse `text`, apply the spec header and inputs, return the body root.
pub fn load_document(
    st: &mut ResolveState<'_>,
    file: FileId,
    text: &str,
    provided_inputs: &IndexMap<String, Node>,
) -> Option<Node> {
    let (docs, yaml_diags) = match glpv_yaml::parse(file, text) {
        Ok(v) => v,
        Err(e) => {
            let path = st.files.origin(file).path.clone();
            st.diag(Severity::Error, "yaml.syntax", format!("{path}: {e}"));
            return None;
        }
    };
    st.import_yaml_diags(yaml_diags);

    let mut docs = docs
        .into_iter()
        .filter(|d| d.root.is_some())
        .collect::<Vec<_>>();
    match docs.len() {
        0 => {
            st.diag(
                Severity::Error,
                "yaml.empty",
                "the configuration file is empty",
            );
            None
        }
        1 => {
            let body = docs.remove(0).root.unwrap();
            check_unused_inputs(st, provided_inputs, &body);
            Some(interpolate(
                st,
                body,
                &IndexMap::new(),
                provided_inputs,
                false,
            ))
        }
        n => {
            if n > 2 {
                st.diag(
                    Severity::Error,
                    "yaml.too-many-documents",
                    format!("{n} YAML documents found; GitLab allows at most 2 (a `spec` header plus the configuration)"),
                );
            }
            let body = docs.remove(1).root.unwrap();
            let header = docs.remove(0).root.unwrap();
            let spec_inputs = parse_spec_inputs(st, &header);
            match spec_inputs {
                Some(spec) => {
                    let resolved = resolve_inputs(st, &spec, provided_inputs);
                    Some(interpolate(st, body, &resolved, provided_inputs, true))
                }
                None => {
                    st.diag(
                        Severity::Error,
                        "spec.invalid",
                        "the first YAML document must contain only a `spec` section",
                    );
                    Some(body)
                }
            }
        }
    }
}

pub struct SpecInput {
    pub name: String,
    pub default: Option<Node>,
    pub input_type: Option<String>,
    pub span: crate::model::Span,
}

fn parse_spec_inputs(st: &mut ResolveState<'_>, header: &Node) -> Option<Vec<SpecInput>> {
    let map = header.as_map()?;
    if map.len() != 1 || !map.contains_key("spec") {
        return None;
    }
    let spec = map.get("spec")?;
    let mut out = Vec::new();
    let Some(inputs) = spec.get("inputs") else {
        return Some(out);
    };
    let Some(inputs_map) = inputs.as_map() else {
        st.diag_at(
            Severity::Error,
            "spec.invalid",
            "`spec:inputs` must be a mapping",
            Some(inputs.span.into()),
        );
        return Some(out);
    };
    for (name, entry) in inputs_map.iter() {
        let (default, input_type) = match entry.value.as_map() {
            Some(opts) => (
                opts.get("default").cloned(),
                opts.get("type").and_then(|t| t.scalar_text()),
            ),
            None => (None, None),
        };
        out.push(SpecInput {
            name: name.to_string(),
            default,
            input_type,
            span: entry.key_span.into(),
        });
    }
    Some(out)
}

fn resolve_inputs(
    st: &mut ResolveState<'_>,
    spec: &[SpecInput],
    provided: &IndexMap<String, Node>,
) -> IndexMap<String, Node> {
    let mut out = IndexMap::new();
    for input in spec {
        match provided
            .get(&input.name)
            .cloned()
            .or_else(|| input.default.clone())
        {
            Some(v) => {
                out.insert(input.name.clone(), v);
            }
            None => st.diag_at(
                Severity::Error,
                "inputs.missing",
                format!("input `{}` has no value and no default", input.name),
                Some(input.span),
            ),
        }
    }
    for name in provided.keys() {
        if !spec.iter().any(|s| s.name == *name) {
            st.diag(
                Severity::Warning,
                "inputs.unknown",
                format!("input `{name}` is not declared in the file's `spec:inputs`"),
            );
        }
    }
    out
}

fn check_unused_inputs(st: &mut ResolveState<'_>, provided: &IndexMap<String, Node>, _body: &Node) {
    if !provided.is_empty() {
        st.diag(
            Severity::Warning,
            "inputs.no-spec",
            "inputs were provided but the file has no `spec:inputs` header",
        );
    }
}

/// Replace `$[[ inputs.x ]]` in every scalar. A scalar that is exactly one
/// interpolation keeps the input's type; anything else becomes a spliced string.
fn interpolate(
    st: &mut ResolveState<'_>,
    node: Node,
    inputs: &IndexMap<String, Node>,
    _provided: &IndexMap<String, Node>,
    has_spec: bool,
) -> Node {
    match node.kind {
        Kind::Scalar(ref s) => {
            let raw = match &s.value {
                Value::Str(v) => v.clone(),
                _ => return node,
            };
            if !raw.contains("$[[") {
                return node;
            }
            if !has_spec {
                // Interpolation only happens for files with a spec header.
                return node;
            }
            let full = INPUT_REF
                .captures(raw.trim())
                .is_some_and(|c| c.get(0).unwrap().as_str() == raw.trim());
            if full {
                let caps = INPUT_REF.captures(raw.trim()).unwrap();
                let name = caps.get(1).unwrap().as_str();
                warn_functions(st, &caps, node.span);
                if let Some(value) = inputs.get(name) {
                    let mut replacement = value.clone();
                    replacement.span = node.span; // provenance: the use site
                    return replacement;
                }
                st.diag_at(
                    Severity::Error,
                    "inputs.undeclared",
                    format!("`$[[ inputs.{name} ]]` refers to an undeclared input"),
                    Some(node.span.into()),
                );
                return node;
            }
            let mut missing = Vec::new();
            let replaced = INPUT_REF
                .replace_all(&raw, |caps: &regex::Captures<'_>| {
                    let name = caps.get(1).unwrap().as_str();
                    match inputs.get(name) {
                        Some(v) => v
                            .scalar_text()
                            .unwrap_or_else(|| "[[non-scalar input]]".to_string()),
                        None => {
                            missing.push(name.to_string());
                            caps.get(0).unwrap().as_str().to_string()
                        }
                    }
                })
                .into_owned();
            for name in missing {
                st.diag_at(
                    Severity::Error,
                    "inputs.undeclared",
                    format!("`$[[ inputs.{name} ]]` refers to an undeclared input"),
                    Some(node.span.into()),
                );
            }
            Node {
                kind: Kind::Scalar(Scalar {
                    raw: replaced.clone(),
                    style: ScalarStyle::DoubleQuoted,
                    value: Value::Str(replaced),
                    str_tagged: false,
                }),
                span: node.span,
                anchor: node.anchor,
                alias_at: node.alias_at,
            }
        }
        Kind::Seq(items) => Node {
            kind: Kind::Seq(
                items
                    .into_iter()
                    .map(|i| interpolate(st, i, inputs, _provided, has_spec))
                    .collect(),
            ),
            span: node.span,
            anchor: node.anchor,
            alias_at: node.alias_at,
        },
        Kind::Map(m) => {
            // Keys interpolate too: job names like `gems $[[inputs.gem_name]]`
            // are how gitlab-org/gitlab stamps per-gem child pipelines.
            let mut rebuilt = glpv_yaml::Map {
                entries: Default::default(),
                dup_keys: m.dup_keys,
            };
            for (key_text, mut entry) in m.entries {
                entry.value = interpolate(st, entry.value, inputs, _provided, has_spec);
                let new_key = if has_spec && key_text.contains("$[[") {
                    let replaced = INPUT_REF
                        .replace_all(&key_text, |caps: &regex::Captures<'_>| {
                            let name = caps.get(1).unwrap().as_str();
                            inputs
                                .get(name)
                                .and_then(|v| v.scalar_text())
                                .unwrap_or_else(|| caps.get(0).unwrap().as_str().to_string())
                        })
                        .into_owned();
                    if replaced != key_text {
                        entry.key.raw = replaced.clone();
                        entry.key.value = glpv_yaml::Value::Str(replaced.clone());
                    }
                    replaced
                } else {
                    key_text
                };
                rebuilt.entries.insert(new_key, entry);
            }
            Node {
                kind: Kind::Map(rebuilt),
                span: node.span,
                anchor: node.anchor,
                alias_at: node.alias_at,
            }
        }
        Kind::Tagged { tag, inner } => Node {
            kind: Kind::Tagged {
                tag,
                inner: Box::new(interpolate(st, *inner, inputs, _provided, has_spec)),
            },
            span: node.span,
            anchor: node.anchor,
            alias_at: node.alias_at,
        },
    }
}

fn warn_functions(st: &mut ResolveState<'_>, caps: &regex::Captures<'_>, span: glpv_yaml::Span) {
    if let Some(fns) = caps.get(2)
        && !fns.as_str().trim().is_empty()
    {
        st.diag_at(
            Severity::Info,
            "inputs.functions-unsupported",
            format!(
                "input functions (`{}`) are not applied yet; the raw input value is used",
                fns.as_str().trim()
            ),
            Some(span.into()),
        );
    }
}
