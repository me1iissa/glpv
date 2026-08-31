//! Span-preserving YAML loader with Psych (Ruby/GitLab) scalar semantics.
//!
//! GitLab parses `.gitlab-ci.yml` with Psych over libyaml, which means YAML 1.1
//! scalar typing (`yes`/`on` are booleans, leading-zero integers are octal),
//! Ruby-flavoured `<<` merge keys, file-local anchors, tolerated duplicate keys
//! and custom tags such as `!reference`. This crate reproduces that behaviour
//! while attaching a source span to every node so consumers can map any piece
//! of merged configuration back to `file:line:col`.

mod emit;
mod loader;
mod psych;

pub use emit::{emit, emit_document};
pub use psych::resolve_plain;

use indexmap::IndexMap;

/// Index into a caller-owned table of source files.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FileId(pub u32);

/// 1-based line and column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub file: FileId,
    pub start: Pos,
    pub end: Pos,
}

impl Span {
    pub fn point(file: FileId, line: u32, col: u32) -> Self {
        let p = Pos { line, col };
        Span {
            file,
            start: p,
            end: p,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScalarStyle {
    Plain,
    SingleQuoted,
    DoubleQuoted,
    Literal,
    Folded,
}

/// Psych-typed scalar value.
#[derive(Clone, PartialEq, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl Value {
    /// Canonical text form, used for map keys and display.
    pub fn canonical(&self) -> String {
        match self {
            Value::Null => "~".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => format_float(*f),
            Value::Str(s) => s.clone(),
        }
    }
}

pub(crate) fn format_float(f: f64) -> String {
    if f.is_nan() {
        ".nan".to_string()
    } else if f.is_infinite() {
        if f > 0.0 {
            ".inf".to_string()
        } else {
            "-.inf".to_string()
        }
    } else if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

#[derive(Clone, Debug)]
pub struct Scalar {
    pub raw: String,
    pub style: ScalarStyle,
    pub value: Value,
    /// True when the scalar carried an explicit `!!str` tag (matters for `<<`).
    pub str_tagged: bool,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub key: Scalar,
    pub key_span: Span,
    pub value: Node,
}

#[derive(Clone, Debug, Default)]
pub struct Map {
    pub entries: IndexMap<String, Entry>,
    /// Keys that appeared more than once: (key, span of an overridden occurrence).
    pub dup_keys: Vec<(String, Span)>,
}

impl Map {
    pub fn get(&self, key: &str) -> Option<&Node> {
        self.entries.get(key).map(|e| &e.value)
    }
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Node> {
        self.entries.get_mut(key).map(|e| &mut e.value)
    }
    pub fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Entry)> {
        self.entries.iter().map(|(k, e)| (k.as_str(), e))
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Debug)]
pub enum Kind {
    Scalar(Scalar),
    Seq(Vec<Node>),
    Map(Map),
    /// Non-core tag (e.g. `!reference`) wrapping its content.
    Tagged {
        tag: String,
        inner: Box<Node>,
    },
}

#[derive(Clone, Debug)]
pub struct Node {
    pub kind: Kind,
    pub span: Span,
    pub anchor: Option<u32>,
    /// Set on the root of a node cloned through a `*alias`; the node's `span`
    /// stays at the anchor definition, this records where the alias was used.
    pub alias_at: Option<Span>,
}

impl Node {
    pub fn null(span: Span) -> Node {
        Node {
            kind: Kind::Scalar(Scalar {
                raw: String::new(),
                style: ScalarStyle::Plain,
                value: Value::Null,
                str_tagged: false,
            }),
            span,
            anchor: None,
            alias_at: None,
        }
    }

    pub fn str(s: impl Into<String>, span: Span) -> Node {
        let s = s.into();
        Node {
            kind: Kind::Scalar(Scalar {
                raw: s.clone(),
                style: ScalarStyle::DoubleQuoted,
                value: Value::Str(s),
                str_tagged: false,
            }),
            span,
            anchor: None,
            alias_at: None,
        }
    }

    pub fn seq(items: Vec<Node>, span: Span) -> Node {
        Node {
            kind: Kind::Seq(items),
            span,
            anchor: None,
            alias_at: None,
        }
    }

    pub fn map(map: Map, span: Span) -> Node {
        Node {
            kind: Kind::Map(map),
            span,
            anchor: None,
            alias_at: None,
        }
    }

    /// Look through a `Tagged` wrapper.
    pub fn untag(&self) -> &Node {
        match &self.kind {
            Kind::Tagged { inner, .. } => inner.untag(),
            _ => self,
        }
    }

    pub fn tag(&self) -> Option<&str> {
        match &self.kind {
            Kind::Tagged { tag, .. } => Some(tag),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&Map> {
        match &self.kind {
            Kind::Map(m) => Some(m),
            _ => None,
        }
    }
    pub fn as_map_mut(&mut self) -> Option<&mut Map> {
        match &mut self.kind {
            Kind::Map(m) => Some(m),
            _ => None,
        }
    }
    pub fn as_seq(&self) -> Option<&[Node]> {
        match &self.kind {
            Kind::Seq(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_seq_mut(&mut self) -> Option<&mut Vec<Node>> {
        match &mut self.kind {
            Kind::Seq(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_scalar(&self) -> Option<&Scalar> {
        match &self.kind {
            Kind::Scalar(s) => Some(s),
            _ => None,
        }
    }
    /// `Some(&str)` only for string scalars.
    pub fn as_str(&self) -> Option<&str> {
        match &self.kind {
            Kind::Scalar(Scalar {
                value: Value::Str(s),
                ..
            }) => Some(s),
            _ => None,
        }
    }
    /// Canonical text for any scalar kind.
    pub fn scalar_text(&self) -> Option<String> {
        self.as_scalar().map(|s| s.value.canonical())
    }
    pub fn as_bool(&self) -> Option<bool> {
        match &self.kind {
            Kind::Scalar(Scalar {
                value: Value::Bool(b),
                ..
            }) => Some(*b),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match &self.kind {
            Kind::Scalar(Scalar {
                value: Value::Int(i),
                ..
            }) => Some(*i),
            _ => None,
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(
            &self.kind,
            Kind::Scalar(Scalar {
                value: Value::Null,
                ..
            })
        )
    }
    /// Map member access; `None` for non-maps and missing keys.
    pub fn get(&self, key: &str) -> Option<&Node> {
        self.as_map().and_then(|m| m.get(key))
    }
}

/// Structural equality ignoring spans, anchors and styles (used by round-trip tests).
pub fn semantic_eq(a: &Node, b: &Node) -> bool {
    match (&a.kind, &b.kind) {
        (Kind::Scalar(x), Kind::Scalar(y)) => x.value == y.value,
        (Kind::Seq(x), Kind::Seq(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(i, j)| semantic_eq(i, j))
        }
        (Kind::Map(x), Kind::Map(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|((ka, ea), (kb, eb))| ka == kb && semantic_eq(&ea.value, &eb.value))
        }
        (Kind::Tagged { tag: ta, inner: ia }, Kind::Tagged { tag: tb, inner: ib }) => {
            ta == tb && semantic_eq(ia, ib)
        }
        _ => false,
    }
}

#[derive(Clone, Debug)]
pub struct Document {
    pub root: Option<Node>,
    pub explicit_start: bool,
    pub span: Span,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Clone, Debug)]
pub struct YamlDiag {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub span: Span,
    pub related: Vec<Span>,
}

#[derive(thiserror::Error, Debug)]
pub enum YamlError {
    #[error("YAML syntax error at line {line}, column {col}: {msg}")]
    Scan { msg: String, line: u32, col: u32 },
    #[error("YAML nesting exceeds the maximum depth of {0}")]
    TooDeep(usize),
}

/// Indent the continuation lines of the quoted scalar starting on `start_line`
/// (1-based) until its closing quote. Returns false when no repair applies.
fn indent_quoted_continuation(lines: &mut [String], start_line: usize) -> bool {
    let Some(idx0) = start_line.checked_sub(1) else {
        return false;
    };
    let Some(l0) = lines.get(idx0) else {
        return false;
    };
    let quote = if l0.matches('\'').count() % 2 == 1 {
        '\''
    } else if l0.matches('"').count() % 2 == 1 {
        '"'
    } else {
        return false;
    };
    let mut i = idx0 + 1;
    let limit = (idx0 + 200).min(lines.len());
    while i < limit {
        let line = &mut lines[i];
        line.insert_str(0, "  ");
        if line.matches(quote).count() % 2 == 1 {
            return true;
        }
        i += 1;
    }
    false
}

/// Parse every document in `text`. Scan errors abort the whole file (GitLab
/// treats such configs as invalid); everything recoverable becomes a
/// [`YamlDiag`] instead.
///
/// One deliberate leniency: libyaml (and therefore GitLab) accepts multi-line
/// quoted scalars whose continuation lines are under-indented, which stricter
/// parsers reject. Leading whitespace on such continuation lines is folded
/// away anyway, so re-indenting is semantics-preserving — we repair and retry,
/// recording an info diagnostic.
pub fn parse(file: FileId, text: &str) -> Result<(Vec<Document>, Vec<YamlDiag>), YamlError> {
    match loader::parse_impl(file, text) {
        Err(YamlError::Scan { msg, line, .. })
            if msg.contains("invalid indentation in quoted scalar") =>
        {
            // `line` points at the scalar's start; indent its continuation
            // lines (up to the closing quote) and retry.
            let mut fixed: Vec<String> = text.lines().map(|l| l.to_string()).collect();
            let mut start_line = line as usize;
            let mut first_repaired = 0usize;
            for _ in 0..16 {
                if !indent_quoted_continuation(&mut fixed, start_line) {
                    break;
                }
                if first_repaired == 0 {
                    first_repaired = start_line;
                }
                let candidate = fixed.join("\n");
                match loader::parse_impl(file, &candidate) {
                    Ok((docs, mut diags)) => {
                        diags.push(YamlDiag {
                            severity: Severity::Info,
                            code: "yaml.lenient-quoted-indent",
                            message: format!(
                                "under-indented quoted-scalar continuation (near line \
                                 {first_repaired}) accepted for libyaml/GitLab compatibility"
                            ),
                            span: Span::point(file, first_repaired as u32, 1),
                            related: Vec::new(),
                        });
                        return Ok((docs, diags));
                    }
                    Err(YamlError::Scan { msg, line, .. })
                        if msg.contains("invalid indentation in quoted scalar") =>
                    {
                        start_line = line as usize;
                    }
                    Err(_) => break,
                }
            }
            loader::parse_impl(file, text)
        }
        other => other,
    }
}
