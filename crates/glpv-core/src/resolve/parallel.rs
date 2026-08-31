//! `parallel` / `parallel:matrix` job-name expansion.

use glpv_yaml::{Kind, Node};
use indexmap::IndexMap;

use crate::model::{Parallel, Severity};
use crate::resolve::context::ResolveState;

pub fn parse_parallel(st: &mut ResolveState<'_>, node: &Node) -> Option<Parallel> {
    match &node.untag().kind {
        Kind::Scalar(_) => match node.as_int() {
            Some(n) if (1..=200).contains(&n) => Some(Parallel::Count(n as u32)),
            _ => {
                st.diag_at(
                    Severity::Error,
                    "parallel.invalid",
                    "`parallel` must be an integer between 1 and 200 or a `matrix`",
                    Some(node.span.into()),
                );
                None
            }
        },
        Kind::Map(m) => {
            let matrix = m.get("matrix")?;
            let Some(entries) = matrix.untag().as_seq() else {
                st.diag_at(
                    Severity::Error,
                    "parallel.invalid",
                    "`parallel:matrix` must be a list of variable mappings",
                    Some(matrix.span.into()),
                );
                return None;
            };
            let mut out = Vec::new();
            for entry in entries {
                let Some(vars) = entry.untag().as_map() else {
                    continue;
                };
                let mut dims: IndexMap<String, Vec<String>> = IndexMap::new();
                for (k, e) in vars.iter() {
                    let values: Vec<String> = match &e.value.untag().kind {
                        Kind::Seq(items) => items.iter().filter_map(|i| i.scalar_text()).collect(),
                        _ => e.value.scalar_text().into_iter().collect(),
                    };
                    dims.insert(k.to_string(), values);
                }
                out.push(dims);
            }
            Some(Parallel::Matrix(out))
        }
        _ => None,
    }
}

/// Expanded job names, in GitLab's format: `job 1/3` or `job: [a, b]`.
pub fn expand_names(base: &str, parallel: &Parallel) -> Vec<String> {
    match parallel {
        // `parallel: 1` runs a single instance under the plain job name.
        Parallel::Count(1) => vec![base.to_string()],
        Parallel::Count(n) => (1..=*n).map(|i| format!("{base} {i}/{n}")).collect(),
        Parallel::Matrix(entries) => {
            let mut names = Vec::new();
            for dims in entries {
                let mut combos: Vec<Vec<String>> = vec![Vec::new()];
                for values in dims.values() {
                    let mut next = Vec::new();
                    for combo in &combos {
                        for v in values {
                            let mut c = combo.clone();
                            c.push(v.clone());
                            next.push(c);
                        }
                    }
                    combos = next;
                }
                for combo in combos {
                    names.push(format!("{base}: [{}]", combo.join(", ")));
                }
            }
            names
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_names() {
        let m = Parallel::Matrix(vec![IndexMap::from([
            ("PROVIDER".to_string(), vec!["aws".to_string()]),
            (
                "STACK".to_string(),
                vec!["app1".to_string(), "app2".to_string()],
            ),
        ])]);
        assert_eq!(
            expand_names("deploystacks", &m),
            vec!["deploystacks: [aws, app1]", "deploystacks: [aws, app2]"]
        );
    }

    #[test]
    fn count_names() {
        assert_eq!(
            expand_names("test", &Parallel::Count(3)),
            vec!["test 1/3", "test 2/3", "test 3/3"]
        );
    }
}
