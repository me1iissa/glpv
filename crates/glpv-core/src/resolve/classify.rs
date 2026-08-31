//! Split the merged root into globals, hidden templates and jobs.

use glpv_yaml::{Entry, Kind, Node};

use crate::model::Severity;
use crate::resolve::context::ResolveState;
use crate::resolve::merge::merge;

/// Top-level keywords that are not jobs.
pub const RESERVED: [&str; 11] = [
    "default",
    "include",
    "stages",
    "variables",
    "workflow",
    "spec",
    "image",
    "services",
    "cache",
    "before_script",
    "after_script",
];

/// Root-level legacy globals that fold into `default:`.
const LEGACY_GLOBALS: [&str; 5] = [
    "image",
    "services",
    "cache",
    "before_script",
    "after_script",
];

pub struct Classified {
    /// `default:` with root-level legacy globals folded in.
    pub defaults: Option<Node>,
    pub workflow: Option<Node>,
    pub variables: Option<Node>,
    pub templates: Vec<(String, Entry)>,
    pub jobs: Vec<(String, Entry)>,
}

pub fn classify_top_level(st: &mut ResolveState<'_>, root: &Node) -> Classified {
    let mut out = Classified {
        defaults: None,
        workflow: None,
        variables: None,
        templates: Vec::new(),
        jobs: Vec::new(),
    };
    let Some(map) = root.as_map() else {
        st.diag(
            Severity::Error,
            "config.invalid",
            "the configuration root must be a mapping",
        );
        return out;
    };

    // Legacy root-level globals act as defaults; explicit `default:` wins.
    let mut legacy = glpv_yaml::Map::default();
    for key in LEGACY_GLOBALS {
        if let Some(entry) = map.entries.get(key) {
            st.diag_at(
                Severity::Info,
                "config.legacy-global",
                format!("top-level `{key}` is deprecated; prefer `default:{key}`"),
                Some(entry.key_span.into()),
            );
            legacy.entries.insert(key.to_string(), entry.clone());
        }
    }
    let legacy_node = (!legacy.is_empty()).then(|| {
        let span = legacy.entries.values().next().unwrap().key_span;
        Node::map(legacy, span)
    });
    out.defaults = match (legacy_node, map.get("default").cloned()) {
        (None, d) => d,
        (l @ Some(_), None) => l,
        (Some(l), Some(d)) => Some(merge(l, d)),
    };

    out.workflow = map.get("workflow").cloned();
    out.variables = map.get("variables").cloned();

    for (name, entry) in map.iter() {
        if RESERVED.contains(&name) {
            continue;
        }
        if let Some(stripped) = name.strip_prefix('.') {
            let _ = stripped;
            out.templates.push((name.to_string(), entry.clone()));
            continue;
        }
        match &entry.value.untag().kind {
            Kind::Map(_) => out.jobs.push((name.to_string(), entry.clone())),
            _ => st.diag_at(
                Severity::Error,
                "job.invalid",
                format!("`{name}` should be a job definition (a mapping) or a reserved keyword"),
                Some(entry.key_span.into()),
            ),
        }
    }
    out
}
