//! Event-driven loader over `saphyr-parser`, producing span-annotated [`Node`]s
//! with Psych semantics for merge keys, duplicate keys, anchors and tags.

use std::collections::HashMap;

use saphyr_parser::{Event, Parser, ScalarStyle as SStyle, Span as SSpan, Tag as STag};

use crate::psych::{self, PsychNote};
use crate::{
    Document, Entry, FileId, Kind, Map, Node, Pos, Scalar, ScalarStyle, Severity, Span, Value,
    YamlDiag, YamlError,
};

/// GitLab's `max_yaml_depth` instance limit.
const MAX_DEPTH: usize = 100;

const CORE_TAG_PREFIX: &str = "tag:yaml.org,2002:";

struct SeqFrame {
    items: Vec<Node>,
    start: Pos,
    anchor: usize,
    tag: Option<String>,
}

struct MapFrame {
    map: Map,
    pending_key: Option<PendingKey>,
    start: Pos,
    anchor: usize,
    tag: Option<String>,
}

enum PendingKey {
    Key(Scalar, Span),
    /// A non-scalar key was seen; swallow the following value.
    Invalid,
}

enum Frame {
    Seq(SeqFrame),
    Map(MapFrame),
}

struct Loader {
    file: FileId,
    diags: Vec<YamlDiag>,
    docs: Vec<Document>,
    anchors: HashMap<usize, Node>,
    stack: Vec<Frame>,
    /// (explicit_start, start_pos, root)
    doc: Option<(bool, Pos, Option<Node>)>,
}

pub(crate) fn parse_impl(
    file: FileId,
    text: &str,
) -> Result<(Vec<Document>, Vec<YamlDiag>), YamlError> {
    let mut l = Loader {
        file,
        diags: Vec::new(),
        docs: Vec::new(),
        anchors: HashMap::new(),
        stack: Vec::new(),
        doc: None,
    };

    for item in Parser::new_from_str(text) {
        let (ev, sspan) = item.map_err(|e| {
            let m = *e.marker();
            YamlError::Scan {
                msg: e.info().to_string(),
                line: m.line() as u32,
                col: m.col() as u32 + 1,
            }
        })?;
        let span = l.conv_span(sspan);
        l.event(ev, span)?;
    }

    // A truncated stream can leave an open document behind; close it defensively.
    if let Some((explicit, start, root)) = l.doc.take() {
        let end = root.as_ref().map(|n| n.span.end).unwrap_or(start);
        l.docs.push(Document {
            root,
            explicit_start: explicit,
            span: Span { file, start, end },
        });
    }
    Ok((l.docs, l.diags))
}

impl Loader {
    fn conv_span(&self, s: SSpan) -> Span {
        Span {
            file: self.file,
            // saphyr markers: line is 1-based, col is 0-based.
            start: Pos {
                line: s.start.line() as u32,
                col: s.start.col() as u32 + 1,
            },
            end: Pos {
                line: s.end.line() as u32,
                col: s.end.col() as u32 + 1,
            },
        }
    }

    fn diag(&mut self, severity: Severity, code: &'static str, message: String, span: Span) {
        self.diags.push(YamlDiag {
            severity,
            code,
            message,
            span,
            related: Vec::new(),
        });
    }

    fn event(&mut self, ev: Event<'_>, span: Span) -> Result<(), YamlError> {
        match ev {
            Event::StreamStart | Event::StreamEnd | Event::Nothing => {}
            Event::DocumentStart(explicit) => {
                self.anchors.clear();
                self.doc = Some((explicit, span.start, None));
            }
            Event::DocumentEnd => {
                let (explicit, start, root) = self.doc.take().unwrap_or((false, span.start, None));
                self.docs.push(Document {
                    root,
                    explicit_start: explicit,
                    span: Span {
                        file: self.file,
                        start,
                        end: span.end,
                    },
                });
            }
            Event::Alias(id) => {
                let node = match self.anchors.get(&id) {
                    Some(n) => {
                        let mut clone = n.clone();
                        clone.alias_at = Some(span);
                        clone
                    }
                    None => {
                        self.diag(
                            Severity::Error,
                            "yaml.unknown-alias",
                            "alias refers to an unknown anchor".to_string(),
                            span,
                        );
                        Node::null(span)
                    }
                };
                self.push_value(node, span);
            }
            Event::Scalar(raw, style, anchor, tag) => {
                let node = self.make_scalar(raw.into_owned(), style, tag.as_deref(), span);
                if anchor != 0 {
                    self.anchors.insert(anchor, node.clone());
                }
                self.push_value(node, span);
            }
            Event::SequenceStart(anchor, tag) => {
                self.check_depth()?;
                self.stack.push(Frame::Seq(SeqFrame {
                    items: Vec::new(),
                    start: span.start,
                    anchor,
                    tag: tag.as_deref().map(tag_text),
                }));
            }
            Event::SequenceEnd => {
                let Some(Frame::Seq(f)) = self.stack.pop() else {
                    unreachable!("parser guarantees balanced events");
                };
                let full = Span {
                    file: self.file,
                    start: f.start,
                    end: span.end,
                };
                let node = self.finish_container(Kind::Seq(f.items), full, f.anchor, f.tag);
                self.push_value(node, full);
            }
            Event::MappingStart(anchor, tag) => {
                self.check_depth()?;
                self.stack.push(Frame::Map(MapFrame {
                    map: Map::default(),
                    pending_key: None,
                    start: span.start,
                    anchor,
                    tag: tag.as_deref().map(tag_text),
                }));
            }
            Event::MappingEnd => {
                let Some(Frame::Map(f)) = self.stack.pop() else {
                    unreachable!("parser guarantees balanced events");
                };
                if matches!(f.pending_key, Some(PendingKey::Key(..))) {
                    // `key:` with no value parses as key + null scalar from libyaml,
                    // so a leftover pending key should not occur; guard anyway.
                    self.diag(
                        Severity::Error,
                        "yaml.dangling-key",
                        "mapping ended while expecting a value".to_string(),
                        span,
                    );
                }
                let full = Span {
                    file: self.file,
                    start: f.start,
                    end: span.end,
                };
                let node = self.finish_container(Kind::Map(f.map), full, f.anchor, f.tag);
                self.push_value(node, full);
            }
        }
        Ok(())
    }

    fn check_depth(&mut self) -> Result<(), YamlError> {
        if self.stack.len() >= MAX_DEPTH {
            return Err(YamlError::TooDeep(MAX_DEPTH));
        }
        Ok(())
    }

    fn finish_container(
        &mut self,
        kind: Kind,
        span: Span,
        anchor: usize,
        tag: Option<String>,
    ) -> Node {
        let mut node = Node {
            kind,
            span,
            anchor: None,
            alias_at: None,
        };
        if let Some(tag) = tag {
            if let Some(suffix) = tag.strip_prefix(CORE_TAG_PREFIX) {
                match suffix {
                    "map" | "seq" => {}
                    other => self.diag(
                        Severity::Info,
                        "yaml.unsupported-collection-tag",
                        format!("`!!{other}` collections are treated as plain collections"),
                        span,
                    ),
                }
            } else {
                node = Node {
                    kind: Kind::Tagged {
                        tag,
                        inner: Box::new(node),
                    },
                    span,
                    anchor: None,
                    alias_at: None,
                };
            }
        }
        if anchor != 0 {
            node.anchor = Some(anchor as u32);
            self.anchors.insert(anchor, node.clone());
        }
        node
    }

    fn make_scalar(&mut self, raw: String, style: SStyle, tag: Option<&STag>, span: Span) -> Node {
        let style = conv_style(style);
        let mut str_tagged = false;

        if let Some(tag) = tag {
            let text = tag_text(tag);
            if let Some(suffix) = text.strip_prefix(CORE_TAG_PREFIX) {
                let value = match suffix {
                    "str" => {
                        str_tagged = true;
                        Value::Str(raw.clone())
                    }
                    "null" => Value::Null,
                    "bool" => match psych::parse_bool(&raw) {
                        Some(b) => Value::Bool(b),
                        None => {
                            self.diag(
                                Severity::Warning,
                                "yaml.invalid-tagged-scalar",
                                format!("`!!bool {raw}` is not a valid boolean"),
                                span,
                            );
                            Value::Str(raw.clone())
                        }
                    },
                    "int" => match psych::parse_int(&raw) {
                        Some(i) => Value::Int(i),
                        None => {
                            self.diag(
                                Severity::Warning,
                                "yaml.invalid-tagged-scalar",
                                format!("`!!int {raw}` is not a valid integer"),
                                span,
                            );
                            Value::Str(raw.clone())
                        }
                    },
                    "float" => match psych::parse_float(&raw) {
                        Some(f) => Value::Float(f),
                        None => {
                            self.diag(
                                Severity::Warning,
                                "yaml.invalid-tagged-scalar",
                                format!("`!!float {raw}` is not a valid float"),
                                span,
                            );
                            Value::Str(raw.clone())
                        }
                    },
                    "binary" => {
                        self.diag(
                            Severity::Info,
                            "yaml.binary-scalar",
                            "`!!binary` content is kept as its base64 text".to_string(),
                            span,
                        );
                        Value::Str(raw.clone())
                    }
                    "timestamp" => {
                        self.diag(
                            Severity::Error,
                            "yaml.disallowed-class",
                            "timestamps are rejected by GitLab's YAML loader".to_string(),
                            span,
                        );
                        Value::Str(raw.clone())
                    }
                    other => {
                        self.diag(
                            Severity::Warning,
                            "yaml.unsupported-scalar-tag",
                            format!("`!!{other}` scalars are treated as strings"),
                            span,
                        );
                        Value::Str(raw.clone())
                    }
                };
                return Node {
                    kind: Kind::Scalar(Scalar {
                        raw,
                        style,
                        value,
                        str_tagged,
                    }),
                    span,
                    anchor: None,
                    alias_at: None,
                };
            }
            // Custom tag on a scalar: keep the text untyped inside a Tagged node.
            let inner = Node {
                kind: Kind::Scalar(Scalar {
                    raw: raw.clone(),
                    style,
                    value: Value::Str(raw),
                    str_tagged: false,
                }),
                span,
                anchor: None,
                alias_at: None,
            };
            return Node {
                kind: Kind::Tagged {
                    tag: text,
                    inner: Box::new(inner),
                },
                span,
                anchor: None,
                alias_at: None,
            };
        }

        let value = if style == ScalarStyle::Plain {
            let (value, note) = psych::resolve_plain(&raw);
            match note {
                Some(PsychNote::SymbolScalar) => self.diag(
                    Severity::Info,
                    "yaml.symbol-scalar",
                    format!("`{raw}` is a Ruby symbol in GitLab's parser; treated as a string"),
                    span,
                ),
                Some(PsychNote::DisallowedDate) => self.diag(
                    Severity::Error,
                    "yaml.disallowed-class",
                    format!(
                        "`{raw}` parses as a date/time, which GitLab's YAML loader rejects; \
                         quote it to make it a string"
                    ),
                    span,
                ),
                Some(PsychNote::IntOverflow) => self.diag(
                    Severity::Warning,
                    "yaml.int-overflow",
                    format!("`{raw}` overflows a 64-bit integer; kept as a string"),
                    span,
                ),
                None => {}
            }
            value
        } else {
            Value::Str(raw.clone())
        };

        Node {
            kind: Kind::Scalar(Scalar {
                raw,
                style,
                value,
                str_tagged,
            }),
            span,
            anchor: None,
            alias_at: None,
        }
    }

    fn push_value(&mut self, node: Node, span: Span) {
        match self.stack.last_mut() {
            None => {
                if let Some((_, _, root)) = self.doc.as_mut() {
                    *root = Some(node);
                }
            }
            Some(Frame::Seq(f)) => f.items.push(node),
            Some(Frame::Map(f)) => match f.pending_key.take() {
                None => match node.kind {
                    Kind::Scalar(s) => f.pending_key = Some(PendingKey::Key(s, span)),
                    _ => {
                        let d = YamlDiag {
                            severity: Severity::Error,
                            code: "yaml.complex-key",
                            message: "non-scalar mapping keys are not supported".to_string(),
                            span,
                            related: Vec::new(),
                        };
                        self.diags.push(d);
                        f.pending_key = Some(PendingKey::Invalid);
                    }
                },
                Some(PendingKey::Invalid) => {}
                Some(PendingKey::Key(key, key_span)) => {
                    // Borrow gymnastics: take the frame's map out to call helpers on self.
                    let is_merge = key.raw == "<<" && !key.str_tagged;
                    if is_merge {
                        let mut diags = Vec::new();
                        apply_merge_key(&mut f.map, &node, key_span, &mut diags);
                        self.diags.extend(diags);
                    } else {
                        let text = key.value.canonical();
                        if let Some(prev) = f.map.entries.get(&text) {
                            let prev_span = prev.key_span;
                            f.map.dup_keys.push((text.clone(), prev_span));
                            self.diags.push(YamlDiag {
                                severity: Severity::Warning,
                                code: "yaml.duplicate-key",
                                message: format!("duplicate key `{text}`: the later value wins"),
                                span: key_span,
                                related: vec![prev_span],
                            });
                        }
                        f.map.entries.insert(
                            text,
                            Entry {
                                key,
                                key_span,
                                value: node,
                            },
                        );
                    }
                }
            },
        }
    }
}

/// Psych `revive_hash` merge-key semantics: the merged values are written at
/// the position of `<<`, overwriting keys defined *before* it; keys after it
/// overwrite merged ones. A sequence merges its elements in reverse (earlier
/// elements win among themselves). Anything else keeps `<<` as a literal key.
fn apply_merge_key(map: &mut Map, value: &Node, merge_span: Span, diags: &mut Vec<YamlDiag>) {
    let merge_from = |map: &mut Map, src: &Map, diags: &mut Vec<YamlDiag>| {
        for (text, entry) in &src.entries {
            if let Some(prev) = map.entries.get(text) {
                diags.push(YamlDiag {
                    severity: Severity::Info,
                    code: "yaml.merge-key-clobber",
                    message: format!(
                        "`<<` overrides the explicitly written key `{text}` (Psych semantics)"
                    ),
                    span: merge_span,
                    related: vec![prev.key_span],
                });
            }
            map.entries.insert(text.clone(), entry.clone());
        }
    };

    match &value.untag().kind {
        Kind::Map(src) => merge_from(map, src, diags),
        Kind::Seq(items) if items.iter().all(|i| i.untag().as_map().is_some()) => {
            let mut combined = Map::default();
            for item in items.iter().rev() {
                if let Kind::Map(src) = &item.untag().kind {
                    for (text, entry) in &src.entries {
                        combined.entries.insert(text.clone(), entry.clone());
                    }
                }
            }
            merge_from(map, &combined, diags);
        }
        _ => {
            // Psych keeps `<<` as an ordinary key when the value is not mergeable.
            let key = Scalar {
                raw: "<<".to_string(),
                style: ScalarStyle::Plain,
                value: Value::Str("<<".to_string()),
                str_tagged: false,
            };
            map.entries.insert(
                "<<".to_string(),
                Entry {
                    key,
                    key_span: merge_span,
                    value: value.clone(),
                },
            );
        }
    }
}

fn conv_style(s: SStyle) -> ScalarStyle {
    match s {
        SStyle::Plain => ScalarStyle::Plain,
        SStyle::SingleQuoted => ScalarStyle::SingleQuoted,
        SStyle::DoubleQuoted => ScalarStyle::DoubleQuoted,
        SStyle::Literal => ScalarStyle::Literal,
        SStyle::Folded => ScalarStyle::Folded,
    }
}

fn tag_text(tag: &STag) -> String {
    format!("{}{}", tag.handle, tag.suffix)
}
