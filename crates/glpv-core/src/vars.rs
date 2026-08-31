//! The include-time CI variable table. Three-valued: a variable is known,
//! known-unset, or unknown (e.g. project/group variables a local crawl cannot
//! see). Unknown values must never be silently guessed — they surface as
//! unresolved nodes or `Unknown` rule outcomes.

use indexmap::IndexMap;

use crate::source::ProjectMeta;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum VarState {
    Known(String),
    Unset,
    Unknown,
}

#[derive(Clone, Debug, Default)]
pub struct VarTable {
    vars: IndexMap<String, VarState>,
}

impl VarTable {
    pub fn get(&self, name: &str) -> VarState {
        self.vars.get(name).cloned().unwrap_or(VarState::Unknown)
    }

    pub fn set(&mut self, name: impl Into<String>, state: VarState) {
        self.vars.insert(name.into(), state);
    }

    pub fn set_known(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.set(name, VarState::Known(value.into()));
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &VarState)> {
        self.vars.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Expand `$VAR` / `${VAR}` occurrences (GitLab's internal expansion; `$$`
    /// escapes a dollar). Returns `Err(missing_names)` when any referenced
    /// variable is unknown or unset.
    pub fn expand(&self, text: &str) -> Result<String, Vec<String>> {
        let mut out = String::with_capacity(text.len());
        let mut missing = Vec::new();
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'$' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
                    out.push('$');
                    i += 2;
                    continue;
                }
                let (name, next) = if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                    match text[i + 2..].find('}') {
                        Some(end) => (&text[i + 2..i + 2 + end], i + 2 + end + 1),
                        None => {
                            out.push('$');
                            i += 1;
                            continue;
                        }
                    }
                } else {
                    let start = i + 1;
                    let mut end = start;
                    while end < bytes.len()
                        && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                    {
                        end += 1;
                    }
                    (&text[start..end], end)
                };
                if name.is_empty() {
                    out.push('$');
                    i += 1;
                    continue;
                }
                match self.get(name) {
                    VarState::Known(v) => out.push_str(&v),
                    VarState::Unset | VarState::Unknown => missing.push(name.to_string()),
                }
                i = next;
            } else {
                let ch = text[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
        if missing.is_empty() {
            Ok(out)
        } else {
            Err(missing)
        }
    }
}

/// A pipeline scenario: what kind of pipeline we are pretending to run.
#[derive(Clone, Debug)]
pub struct Scenario {
    pub id: String,
    /// A `CI_PIPELINE_SOURCE` value.
    pub source: String,
    /// Branch or tag name; `None` = the project default branch.
    pub git_ref: Option<String>,
    pub is_tag: bool,
    pub vars: IndexMap<String, String>,
}

impl Scenario {
    pub fn push_default() -> Self {
        Scenario {
            id: "push@default".to_string(),
            source: "push".to_string(),
            git_ref: None,
            is_tag: false,
            vars: IndexMap::new(),
        }
    }
}

pub fn slugify(name: &str) -> String {
    let mut s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    s.truncate(63);
    s.trim_matches('-').to_string()
}

/// Build the pre-pipeline predefined variable set for a project + scenario.
pub fn predefined_vars(
    meta: &ProjectMeta,
    default_branch: &str,
    sha: Option<&str>,
    config_path: &str,
    scenario: &Scenario,
) -> VarTable {
    let mut t = VarTable::default();
    let host = &meta.key.host;
    let path = &meta.display_path;
    let name = path.rsplit('/').next().unwrap_or(path);
    let namespace = path.rsplit_once('/').map(|(ns, _)| ns).unwrap_or("");
    let root_ns = path.split('/').next().unwrap_or("");
    let ref_name = scenario
        .git_ref
        .clone()
        .unwrap_or_else(|| default_branch.to_string());

    t.set_known("CI", "true");
    t.set_known("GITLAB_CI", "true");
    t.set_known("CI_SERVER_HOST", host);
    t.set_known("CI_SERVER_FQDN", host);
    t.set_known("CI_SERVER_URL", format!("https://{host}"));
    t.set_known("CI_API_V4_URL", format!("https://{host}/api/v4"));
    t.set_known("CI_PROJECT_PATH", path);
    t.set_known("CI_PROJECT_NAME", name);
    t.set_known("CI_PROJECT_NAMESPACE", namespace);
    t.set_known("CI_PROJECT_ROOT_NAMESPACE", root_ns);
    t.set_known("CI_PROJECT_PATH_SLUG", slugify(path));
    t.set_known("CI_PROJECT_URL", format!("https://{host}/{path}"));
    t.set_known("CI_DEFAULT_BRANCH", default_branch);
    t.set_known("CI_CONFIG_PATH", config_path);
    t.set_known("CI_PIPELINE_SOURCE", &scenario.source);
    t.set_known("CI_COMMIT_REF_NAME", &ref_name);
    t.set_known("CI_COMMIT_REF_SLUG", slugify(&ref_name));
    if let Some(sha) = sha {
        t.set_known("CI_COMMIT_SHA", sha);
        t.set_known("CI_COMMIT_SHORT_SHA", &sha[..sha.len().min(8)]);
    } else {
        t.set("CI_COMMIT_SHA", VarState::Unknown);
        t.set("CI_COMMIT_SHORT_SHA", VarState::Unknown);
    }
    if scenario.is_tag {
        t.set_known("CI_COMMIT_TAG", &ref_name);
        t.set("CI_COMMIT_BRANCH", VarState::Unset);
    } else if scenario.source == "merge_request_event" {
        t.set("CI_COMMIT_TAG", VarState::Unset);
        t.set("CI_COMMIT_BRANCH", VarState::Unset);
        t.set_known("CI_MERGE_REQUEST_SOURCE_BRANCH_NAME", &ref_name);
        t.set(
            "CI_MERGE_REQUEST_TARGET_BRANCH_NAME",
            VarState::Known(default_branch.to_string()),
        );
        t.set("CI_MERGE_REQUEST_IID", VarState::Unknown);
        t.set("CI_OPEN_MERGE_REQUESTS", VarState::Unknown);
    } else {
        t.set("CI_COMMIT_TAG", VarState::Unset);
        t.set_known("CI_COMMIT_BRANCH", &ref_name);
    }
    // Project/group/instance variables are invisible to a static crawl; any
    // name not present in the table reads as Unknown, which is exactly right.
    for (k, v) in &scenario.vars {
        t.set_known(k.clone(), v.clone());
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expansion() {
        let mut t = VarTable::default();
        t.set_known("A", "x");
        t.set("B", VarState::Unset);
        assert_eq!(t.expand("$A/file"), Ok("x/file".to_string()));
        assert_eq!(t.expand("${A}y"), Ok("xy".to_string()));
        assert_eq!(t.expand("$$A"), Ok("$A".to_string()));
        assert_eq!(t.expand("no vars"), Ok("no vars".to_string()));
        assert_eq!(t.expand("$B"), Err(vec!["B".to_string()]));
        assert_eq!(t.expand("$UNKNOWN"), Err(vec!["UNKNOWN".to_string()]));
        assert_eq!(t.expand("$ alone"), Ok("$ alone".to_string()));
    }

    #[test]
    fn slug() {
        assert_eq!(slugify("feature/My_Branch"), "feature-my-branch");
    }
}
