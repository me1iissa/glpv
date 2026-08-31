//! `!reference [path, to, value]` resolution. Runs after `extends`, resolves
//! against the merged root, transitively (a referenced value may itself
//! contain `!reference`) up to 10 levels, with cycle detection. A reference
//! that resolves to a sequence while sitting inside a sequence is spliced.

use glpv_yaml::{Kind, Node};
use indexmap::IndexMap;

use crate::model::{Contribution, ContributionKind, Diagnostic, Severity};

const MAX_REFERENCE_DEPTH: u32 = 10;

pub type Contributions = IndexMap<String, Vec<Contribution>>;

pub fn resolve_references(root: &mut Node, diags: &mut Vec<Diagnostic>) -> Contributions {
    let snapshot = root.clone();
    let mut contributions: Contributions = IndexMap::new();

    if let Some(map) = root.as_map_mut() {
        for (key, entry) in map.entries.iter_mut() {
            let mut ctx = Ctx {
                root: &snapshot,
                diags,
                contribs: Vec::new(),
                top_key: key.clone(),
            };
            entry.value = ctx.walk(entry.value.clone(), 0, &mut Vec::new());
            if !ctx.contribs.is_empty() {
                contributions.insert(key.clone(), ctx.contribs);
            }
        }
    }
    contributions
}

struct Ctx<'a> {
    root: &'a Node,
    diags: &'a mut Vec<Diagnostic>,
    contribs: Vec<Contribution>,
    top_key: String,
}

impl Ctx<'_> {
    fn walk(&mut self, node: Node, depth: u32, path_stack: &mut Vec<String>) -> Node {
        match node.kind {
            Kind::Tagged { ref tag, .. } if tag == "!reference" => {
                self.resolve_reference(&node, depth, path_stack)
            }
            Kind::Seq(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    let is_ref =
                        matches!(&item.kind, Kind::Tagged { tag, .. } if tag == "!reference");
                    let resolved = self.walk(item, depth, path_stack);
                    // Splice: a reference inside a sequence that produced a
                    // sequence contributes its items, not a nested list.
                    if is_ref && let Kind::Seq(inner) = resolved.kind {
                        out.extend(inner);
                        continue;
                    }
                    out.push(resolved);
                }
                Node {
                    kind: Kind::Seq(out),
                    ..node_parts(node.span, node.anchor, node.alias_at)
                }
            }
            Kind::Map(mut m) => {
                for (_, entry) in m.entries.iter_mut() {
                    entry.value = self.walk(entry.value.clone(), depth, path_stack);
                }
                Node {
                    kind: Kind::Map(m),
                    ..node_parts(node.span, node.anchor, node.alias_at)
                }
            }
            _ => node,
        }
    }

    fn resolve_reference(
        &mut self,
        reference: &Node,
        depth: u32,
        path_stack: &mut Vec<String>,
    ) -> Node {
        let span = reference.span;
        let Kind::Tagged { inner, .. } = &reference.kind else {
            unreachable!()
        };

        let parts: Option<Vec<String>> = inner
            .as_seq()
            .map(|items| items.iter().filter_map(|i| i.scalar_text()).collect());
        let parts = match parts {
            Some(p) if !p.is_empty() && inner.as_seq().map(|s| s.len()) == Some(p.len()) => p,
            _ => {
                self.push_diag(
                    Severity::Error,
                    "reference.invalid",
                    "`!reference` takes a sequence of strings, e.g. `!reference [.job, script]`"
                        .to_string(),
                    span,
                );
                return Node::null(span);
            }
        };
        let path_text = parts.join(".");

        if depth >= MAX_REFERENCE_DEPTH {
            self.push_diag(
                Severity::Error,
                "reference.too-deep",
                format!(
                    "`!reference` nesting exceeds {MAX_REFERENCE_DEPTH} levels at [{path_text}]"
                ),
                span,
            );
            return Node::null(span);
        }
        if path_stack.contains(&path_text) {
            self.push_diag(
                Severity::Error,
                "reference.cycle",
                format!("`!reference` cycle through [{path_text}]"),
                span,
            );
            return Node::null(span);
        }

        let mut target = self.root;
        for part in &parts {
            match target.untag().get(part) {
                Some(next) => target = next,
                None => {
                    self.push_diag(
                        Severity::Error,
                        "reference.missing",
                        format!(
                            "`!reference [{path_text}]` in `{}` points at nothing",
                            self.top_key
                        ),
                        span,
                    );
                    return Node::null(span);
                }
            }
        }

        self.contribs.push(Contribution {
            kind: ContributionKind::Reference(path_text.clone()),
            span: span.into(),
        });

        path_stack.push(path_text);
        let resolved = self.walk(target.clone(), depth + 1, path_stack);
        path_stack.pop();
        resolved
    }

    fn push_diag(
        &mut self,
        severity: Severity,
        code: &str,
        message: String,
        span: glpv_yaml::Span,
    ) {
        self.diags.push(Diagnostic {
            severity,
            code: code.to_string(),
            message,
            span: Some(span.into()),
            related: Vec::new(),
            hint: None,
            pipeline: None,
        });
    }
}

fn node_parts(
    span: glpv_yaml::Span,
    anchor: Option<u32>,
    alias_at: Option<glpv_yaml::Span>,
) -> Node {
    Node {
        kind: Kind::Seq(Vec::new()),
        span,
        anchor,
        alias_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glpv_yaml::{FileId, parse};

    fn root(text: &str) -> Node {
        parse(FileId(0), text).unwrap().0.remove(0).root.unwrap()
    }

    #[test]
    fn splices_into_sequences() {
        let mut r = root(
            "\
.setup:\n  script: [a, b]\n\
job:\n  script:\n    - !reference [.setup, script]\n    - c\n",
        );
        let mut diags = Vec::new();
        resolve_references(&mut r, &mut diags);
        let script = r
            .get("job")
            .unwrap()
            .get("script")
            .unwrap()
            .as_seq()
            .unwrap();
        let texts: Vec<_> = script.iter().map(|n| n.as_str().unwrap()).collect();
        assert_eq!(texts, vec!["a", "b", "c"]);
        assert!(diags.is_empty());
    }

    #[test]
    fn nested_value_reference() {
        let mut r = root(
            "\
.vars:\n  variables:\n    URL: prod\n\
job:\n  variables:\n    COPY: !reference [.vars, variables, URL]\n",
        );
        let mut diags = Vec::new();
        resolve_references(&mut r, &mut diags);
        let v = r
            .get("job")
            .unwrap()
            .get("variables")
            .unwrap()
            .get("COPY")
            .unwrap();
        assert_eq!(v.as_str(), Some("prod"));
    }

    #[test]
    fn transitive_and_inside_rules() {
        let mut r = root(
            "\
.never:\n  rules:\n    - if: $X\n      when: never\n\
.chain:\n  rules:\n    - !reference [.never, rules]\n\
job:\n  rules:\n    - !reference [.chain, rules]\n    - when: manual\n",
        );
        let mut diags = Vec::new();
        resolve_references(&mut r, &mut diags);
        let rules = r
            .get("job")
            .unwrap()
            .get("rules")
            .unwrap()
            .as_seq()
            .unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].get("when").unwrap().as_str(), Some("never"));
        assert!(diags.is_empty());
    }

    #[test]
    fn cycle_and_missing() {
        let mut r = root(
            "\
.a:\n  script:\n    - !reference [.b, script]\n\
.b:\n  script:\n    - !reference [.a, script]\n\
job:\n  script:\n    - !reference [.a, script]\n  missing: !reference [.nope, x]\n",
        );
        let mut diags = Vec::new();
        resolve_references(&mut r, &mut diags);
        assert!(diags.iter().any(|d| d.code == "reference.cycle"));
        assert!(diags.iter().any(|d| d.code == "reference.missing"));
    }

    #[test]
    fn provenance_spans_point_at_origin() {
        let mut r = root(
            ".setup:\n  script: [from-setup]\njob:\n  script:\n    - !reference [.setup, script]\n",
        );
        let mut diags = Vec::new();
        resolve_references(&mut r, &mut diags);
        let item = &r
            .get("job")
            .unwrap()
            .get("script")
            .unwrap()
            .as_seq()
            .unwrap()[0];
        // The spliced item keeps the span of `.setup`'s definition (line 2).
        assert_eq!(item.span.start.line, 2);
    }
}
