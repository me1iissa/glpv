//! Final stage list: `.pre` + (declared `stages` or the default trio) + `.post`.

use glpv_yaml::Node;

use crate::model::Severity;
use crate::resolve::context::ResolveState;

pub const DEFAULT_STAGES: [&str; 3] = ["build", "test", "deploy"];

pub fn final_stages(st: &mut ResolveState<'_>, root: &Node) -> Vec<String> {
    let declared: Vec<String> = match root.get("stages") {
        None => DEFAULT_STAGES.iter().map(|s| s.to_string()).collect(),
        Some(node) => match node.untag().as_seq() {
            Some(items) => items.iter().filter_map(|i| i.scalar_text()).collect(),
            None => {
                st.diag_at(
                    Severity::Error,
                    "stages.invalid",
                    "`stages` must be a list of stage names",
                    Some(node.span.into()),
                );
                DEFAULT_STAGES.iter().map(|s| s.to_string()).collect()
            }
        },
    };
    let mut out = vec![".pre".to_string()];
    for s in declared {
        if s != ".pre" && s != ".post" && !out.contains(&s) {
            out.push(s);
        }
    }
    out.push(".post".to_string());
    out
}
