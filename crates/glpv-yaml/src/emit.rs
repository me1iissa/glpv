//! Minimal block-style YAML emitter for resolved configuration output.
//!
//! Output is meant for humans and for re-parsing with this crate (round-trip
//! safety), not byte-compatibility with any other emitter. Strings are kept
//! plain only when re-parsing them yields the identical string.

use crate::{Kind, Node, Value, format_float, psych};

/// Emit a node as a YAML document (with trailing newline).
pub fn emit_document(node: &Node) -> String {
    let mut out = String::new();
    match &node.kind {
        Kind::Map(m) if m.is_empty() => out.push_str("{}\n"),
        Kind::Seq(s) if s.is_empty() => out.push_str("[]\n"),
        Kind::Map(_) | Kind::Seq(_) => {
            emit_block(node, 0, &mut out);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        _ => {
            out.push_str(&inline(node));
            out.push('\n');
        }
    }
    out
}

/// Emit a node without the trailing newline (top-level convenience).
pub fn emit(node: &Node) -> String {
    let mut s = emit_document(node);
    while s.ends_with('\n') {
        s.pop();
    }
    s
}

fn indent_str(n: usize) -> String {
    "  ".repeat(n)
}

/// True when the node renders on a single line.
fn is_inline(node: &Node) -> bool {
    match &node.kind {
        Kind::Scalar(s) => !matches!(&s.value, Value::Str(v) if v.contains('\n')),
        Kind::Seq(s) => s.is_empty(),
        Kind::Map(m) => m.is_empty(),
        Kind::Tagged { inner, .. } => is_inline_flow(inner),
    }
}

fn is_inline_flow(node: &Node) -> bool {
    match &node.kind {
        Kind::Scalar(s) => !matches!(&s.value, Value::Str(v) if v.contains('\n')),
        Kind::Seq(s) => s.iter().all(is_inline_flow),
        Kind::Map(m) => m.iter().all(|(_, e)| is_inline_flow(&e.value)),
        Kind::Tagged { inner, .. } => is_inline_flow(inner),
    }
}

fn inline(node: &Node) -> String {
    match &node.kind {
        Kind::Scalar(s) => scalar_text(&s.value),
        Kind::Seq(items) => {
            let parts: Vec<String> = items.iter().map(inline).collect();
            format!("[{}]", parts.join(", "))
        }
        Kind::Map(m) => {
            let parts: Vec<String> = m
                .iter()
                .map(|(k, e)| format!("{}: {}", key_text(k), inline(&e.value)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        Kind::Tagged { tag, inner } => format!("{tag} {}", inline(inner)),
    }
}

fn emit_block(node: &Node, level: usize, out: &mut String) {
    match &node.kind {
        Kind::Map(m) => {
            for (k, e) in m.iter() {
                out.push_str(&indent_str(level));
                out.push_str(&key_text(k));
                out.push(':');
                emit_value(&e.value, level, out);
            }
        }
        Kind::Seq(items) => {
            for item in items {
                out.push_str(&indent_str(level));
                out.push('-');
                match &item.kind {
                    // Hanging style: `- key: value` with following keys indented.
                    Kind::Map(m) if !m.is_empty() => {
                        let mut first = true;
                        for (k, e) in m.iter() {
                            if first {
                                out.push(' ');
                                first = false;
                            } else {
                                out.push_str(&indent_str(level + 1));
                            }
                            out.push_str(&key_text(k));
                            out.push(':');
                            emit_value(&e.value, level + 1, out);
                        }
                    }
                    _ => emit_value(item, level, out),
                }
            }
        }
        _ => {
            out.push_str(&indent_str(level));
            out.push_str(&inline(node));
            out.push('\n');
        }
    }
}

/// Emit the value part after `key:` or `-` that is already written to `out`.
fn emit_value(value: &Node, level: usize, out: &mut String) {
    match &value.kind {
        Kind::Scalar(s) => {
            if let Value::Str(v) = &s.value
                && v.contains('\n')
                && let Some(block) = literal_block(v, level + 1)
            {
                out.push(' ');
                out.push_str(&block);
                return;
            }
            out.push(' ');
            out.push_str(&scalar_text(&s.value));
            out.push('\n');
        }
        Kind::Seq(items) if items.is_empty() => out.push_str(" []\n"),
        Kind::Map(m) if m.is_empty() => out.push_str(" {}\n"),
        Kind::Tagged { .. } if is_inline(value) => {
            out.push(' ');
            out.push_str(&inline(value));
            out.push('\n');
        }
        _ => {
            out.push('\n');
            emit_block(value, level + 1, out);
        }
    }
}

/// Literal block scalar (`|` / `|-`) when the content is representable that way.
fn literal_block(v: &str, level: usize) -> Option<String> {
    let (body, header) = if let Some(stripped) = v.strip_suffix('\n') {
        if stripped.ends_with('\n') {
            return None; // multiple trailing newlines → fall back to quoting
        }
        (stripped, "|")
    } else {
        (v, "|-")
    };
    let lines: Vec<&str> = body.split('\n').collect();
    let ok = lines.iter().all(|l| {
        !l.starts_with(' ') && !l.ends_with(' ') && !l.chars().any(|c| c.is_control() && c != '\t')
    });
    if !ok || lines.is_empty() {
        return None;
    }
    let ind = indent_str(level);
    let mut s = String::from(header);
    for l in &lines {
        s.push('\n');
        if !l.is_empty() {
            s.push_str(&ind);
            s.push_str(l);
        }
    }
    s.push('\n');
    Some(s)
}

fn key_text(k: &str) -> String {
    if plain_safe(k) {
        k.to_string()
    } else {
        quote(k)
    }
}

fn scalar_text(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => format_float(*f),
        Value::Str(s) => {
            if plain_safe(s) {
                s.clone()
            } else {
                quote(s)
            }
        }
    }
}

fn plain_safe(s: &str) -> bool {
    if s.is_empty() || s.starts_with(' ') || s.ends_with(' ') {
        return false;
    }
    if s.chars().any(|c| c.is_control()) {
        return false;
    }
    let first = s.chars().next().unwrap();
    if "!&*?#|>%@`\"'{}[],:- ".contains(first) {
        return false;
    }
    if s.contains(": ") || s.ends_with(':') || s.contains(" #") {
        return false;
    }
    // The text must re-parse as the very same string under Psych rules.
    matches!(psych::resolve_plain(s).0, Value::Str(v) if v == s)
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
