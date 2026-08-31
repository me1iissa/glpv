//! GitLab CI configuration resolver and pipeline-graph builder.
//!
//! See `docs/semantics.md` in the repository for the semantics contract this
//! crate implements (GitLab 18.x, verified against docs.gitlab.com and the
//! `gitlab-org/gitlab` implementation).

pub mod config;
pub mod glob;
pub mod model;
pub mod resolve;
pub mod rules;
pub mod scan;
pub mod source;
pub mod util;
pub mod vars;
