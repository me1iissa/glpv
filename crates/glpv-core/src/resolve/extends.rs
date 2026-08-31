//! `extends:` resolution — reverse deep merge across the merged root, up to 11
//! levels, arrays replaced, explicit `null` removing inherited keys.

use std::collections::HashMap;

use glpv_yaml::{Kind, Node};
use indexmap::IndexMap;

use crate::model::{Contribution, ContributionKind, Diagnostic, Severity};
use crate::resolve::merge::merge_extends;

const MAX_EXTENDS_DEPTH: u32 = 11;

/// Contributions recorded per top-level key.
pub type Contributions = IndexMap<String, Vec<Contribution>>;

pub fn resolve_extends(root: &mut Node, diags: &mut Vec<Diagnostic>) -> Contributions {
    let mut contributions: Contributions = IndexMap::new();
    let Some(map) = root.as_map() else {
        return contributions;
    };

    let names: Vec<String> = map
        .iter()
        .filter(|(_, e)| e.value.untag().get("extends").is_some())
        .map(|(k, _)| k.to_string())
        .collect();
    if names.is_empty() {
        return contributions;
    }

    let mut resolver = Resolver {
        root: root.clone(),
        cache: HashMap::new(),
        diags,
    };
    let mut results: Vec<(String, Node, Vec<Contribution>)> = Vec::new();
    for name in map.iter().map(|(k, _)| k.to_string()).collect::<Vec<_>>() {
        let mut visiting = Vec::new();
        // The resolution (and its contributions) may already be cached from
        // serving as another job's base — write back regardless.
        if let Some((node, contribs)) = resolver.resolve(&name, 0, &mut visiting)
            && !contribs.is_empty()
        {
            results.push((name.clone(), node, contribs));
        }
    }

    let map = root.as_map_mut().unwrap();
    for (name, node, contribs) in results {
        if let Some(entry) = map.entries.get_mut(&name) {
            entry.value = node;
        }
        contributions.insert(name, contribs);
    }
    contributions
}

struct Resolver<'a> {
    root: Node,
    cache: HashMap<String, Option<(Node, Vec<Contribution>)>>,
    diags: &'a mut Vec<Diagnostic>,
}

impl Resolver<'_> {
    /// Fully resolved body of `name` (its own `extends` chain applied),
    /// together with the direct-base contributions. `None` when the key is
    /// missing or not a map.
    fn resolve(
        &mut self,
        name: &str,
        depth: u32,
        visiting: &mut Vec<String>,
    ) -> Option<(Node, Vec<Contribution>)> {
        if let Some(cached) = self.cache.get(name) {
            return cached.clone();
        }
        let node = self.root.get(name)?.clone();
        if node.untag().as_map().is_none() {
            return Some((node, Vec::new()));
        }
        let bases = extends_names(&node);
        if bases.is_empty() {
            return Some((node, Vec::new()));
        }
        let mut contribs: Vec<Contribution> = Vec::new();

        if visiting.iter().any(|v| v == name) {
            self.diags.push(diag(
                Severity::Error,
                "extends.cycle",
                format!("`extends` cycle involving `{name}`"),
                Some(node.span.into()),
            ));
            return Some((node, Vec::new()));
        }
        if depth >= MAX_EXTENDS_DEPTH {
            self.diags.push(diag(
                Severity::Error,
                "extends.too-deep",
                format!("`extends` nesting exceeds {MAX_EXTENDS_DEPTH} levels at `{name}`"),
                Some(node.span.into()),
            ));
            return Some((node, Vec::new()));
        }
        visiting.push(name.to_string());

        // "Keys from the last member always override": merge bases in order,
        // later bases over earlier, then the extending job over the result.
        let mut acc: Option<Node> = None;
        for base_name in &bases {
            match self.root.get(base_name) {
                None => self.diags.push(diag(
                    Severity::Error,
                    "extends.missing",
                    format!("`{name}` extends `{base_name}`, which is not defined"),
                    Some(node.span.into()),
                )),
                Some(base_entry_node) => {
                    let base_span = base_entry_node.span;
                    let resolved = self.resolve(base_name, depth + 1, visiting);
                    if let Some((resolved, _)) = resolved {
                        contribs.push(Contribution {
                            kind: ContributionKind::Extends(base_name.clone()),
                            span: base_span.into(),
                        });
                        acc = Some(match acc {
                            None => resolved,
                            Some(a) => merge_extends(a, resolved),
                        });
                    }
                }
            }
        }
        visiting.pop();

        // GitLab's merged_yaml keeps the (now inert) `extends` key; so do we,
        // which makes oracle comparisons against the lint API exact.
        let result = match acc {
            None => node,
            Some(a) => merge_extends(a, node),
        };
        self.cache
            .insert(name.to_string(), Some((result.clone(), contribs.clone())));
        Some((result, contribs))
    }
}

fn extends_names(node: &Node) -> Vec<String> {
    match node.untag().get("extends") {
        None => Vec::new(),
        Some(v) => match &v.untag().kind {
            Kind::Seq(items) => items.iter().filter_map(|i| i.scalar_text()).collect(),
            _ => v.scalar_text().into_iter().collect(),
        },
    }
}

fn diag(
    severity: Severity,
    code: &str,
    message: String,
    span: Option<crate::model::Span>,
) -> Diagnostic {
    Diagnostic {
        severity,
        code: code.to_string(),
        message,
        span,
        related: Vec::new(),
        hint: None,
        pipeline: None,
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
    fn multi_level_extends() {
        let mut r = root(
            "\
.base:\n  image: ruby\n  variables:\n    A: base\n    B: base\n\
.mid:\n  extends: .base\n  variables:\n    B: mid\n\
job:\n  extends: .mid\n  script: [run]\n",
        );
        let mut diags = Vec::new();
        let contribs = resolve_extends(&mut r, &mut diags);
        let job = r.get("job").unwrap();
        assert_eq!(job.get("image").unwrap().as_str(), Some("ruby"));
        assert_eq!(
            job.get("variables").unwrap().get("A").unwrap().as_str(),
            Some("base")
        );
        assert_eq!(
            job.get("variables").unwrap().get("B").unwrap().as_str(),
            Some("mid")
        );
        assert_eq!(job.get("extends").unwrap().as_str(), Some(".mid"));
        assert!(diags.is_empty());
        assert_eq!(contribs["job"].len(), 1);
        // The hidden template resolved too (kept for provenance / future use).
        assert_eq!(
            r.get(".mid").unwrap().get("image").unwrap().as_str(),
            Some("ruby")
        );
    }

    #[test]
    fn last_base_wins_and_null_removes() {
        let mut r = root(
            "\
.a:\n  variables:\n    X: a\n  cache: {k: a}\n\
.b:\n  variables:\n    X: b\n\
job:\n  extends: [.a, .b]\n  cache: null\n  script: [x]\n",
        );
        let mut diags = Vec::new();
        resolve_extends(&mut r, &mut diags);
        let job = r.get("job").unwrap();
        assert_eq!(
            job.get("variables").unwrap().get("X").unwrap().as_str(),
            Some("b")
        );
        assert!(
            job.get("cache").is_none(),
            "explicit null removes inherited key"
        );
    }

    #[test]
    fn cycle_and_missing_are_diagnosed() {
        let mut r = root(
            ".a:\n  extends: .b\n.b:\n  extends: .a\njob:\n  extends: .missing\n  script: [x]\n",
        );
        let mut diags = Vec::new();
        resolve_extends(&mut r, &mut diags);
        assert!(diags.iter().any(|d| d.code == "extends.cycle"));
        assert!(diags.iter().any(|d| d.code == "extends.missing"));
    }
}
