//! Library surface of the glpv CLI crate: the fixture-repo builder (shared by
//! the integration tests and the `build_fixtures` example), the oracle
//! comparison behind `glpv check`, and the HTTP server behind `glpv serve`.

pub mod check;
pub mod fixtures;
pub mod serve;
