//! Small shared helpers.

use glpv_yaml::{Kind, Node, Value};

/// Lossy conversion of a YAML node to JSON (for display fields such as
/// `trigger.includes` and legacy only/except blocks). Tagged nodes become
/// `{"<tag>": inner}` objects.
pub fn node_to_json(node: &Node) -> serde_json::Value {
    match &node.kind {
        Kind::Scalar(s) => match &s.value {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(i) => serde_json::Value::from(*i),
            Value::Float(f) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Value::Str(s) => serde_json::Value::String(s.clone()),
        },
        Kind::Seq(items) => serde_json::Value::Array(items.iter().map(node_to_json).collect()),
        Kind::Map(m) => {
            let mut obj = serde_json::Map::new();
            for (k, e) in m.iter() {
                obj.insert(k.to_string(), node_to_json(&e.value));
            }
            serde_json::Value::Object(obj)
        }
        Kind::Tagged { tag, inner } => {
            let mut obj = serde_json::Map::new();
            obj.insert(tag.clone(), node_to_json(inner));
            serde_json::Value::Object(obj)
        }
    }
}

/// Collect `(key_path, span)` pairs for every leaf under `node`, up to `cap`.
pub fn leaf_spans(
    node: &Node,
    prefix: &str,
    out: &mut indexmap::IndexMap<String, crate::model::Span>,
    cap: usize,
) {
    if out.len() >= cap {
        return;
    }
    match &node.kind {
        Kind::Scalar(_) => {
            out.insert(prefix.to_string(), node.span.into());
        }
        Kind::Tagged { inner, .. } => leaf_spans(inner, prefix, out, cap),
        Kind::Seq(items) => {
            for (i, item) in items.iter().enumerate() {
                let p = if prefix.is_empty() {
                    i.to_string()
                } else {
                    format!("{prefix}/{i}")
                };
                leaf_spans(item, &p, out, cap);
            }
        }
        Kind::Map(m) => {
            for (k, e) in m.iter() {
                let p = if prefix.is_empty() {
                    k.to_string()
                } else {
                    format!("{prefix}/{k}")
                };
                leaf_spans(&e.value, &p, out, cap);
            }
        }
    }
}

/// Flatten a `variables:` node into name → value text (maps with a `value`
/// key use it; non-scalar oddities are skipped).
pub fn yaml_vars_map(node: Option<&Node>) -> indexmap::IndexMap<String, String> {
    let mut out = indexmap::IndexMap::new();
    let Some(map) = node.and_then(|n| n.untag().as_map()) else {
        return out;
    };
    for (k, e) in map.iter() {
        let value = match &e.value.untag().kind {
            Kind::Map(m) => m.get("value").and_then(|v| v.scalar_text()),
            _ => e.value.scalar_text(),
        };
        if let Some(v) = value {
            out.insert(k.to_string(), v);
        }
    }
    out
}
