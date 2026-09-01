//! What an instance API unlocks in the resolver, exercised offline with
//! in-memory project sources: the local index first, the API for projects
//! that are not cloned (`include:project`, multi-project `trigger`), instance
//! templates, `include:remote`, and catalog versions of components.

use std::collections::BTreeMap;
use std::sync::Arc;

use glpv_core::model::{IncludeKind, PipelineKind, UnresolvedReason};
use glpv_core::resolve::ResolveOpts;
use glpv_core::scan::scan_entry;
use glpv_core::source::{
    ChainLocator, InstanceApi, ProjectKey, ProjectLocator, ProjectMeta, ProjectOrigin,
    ProjectSource, RemoteFetcher, Sha, SourceError, Sources, TreeRef,
};
use glpv_core::vars::Scenario;

const HOST: &str = "gitlab.example.com";
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

struct MemProject {
    meta: ProjectMeta,
    tags: Vec<String>,
    files: BTreeMap<String, String>,
}

impl MemProject {
    fn new(path: &str, api: bool, tags: &[&str], files: &[(&str, &str)]) -> Arc<MemProject> {
        Arc::new(MemProject {
            meta: ProjectMeta {
                key: ProjectKey::new(HOST, path),
                display_path: path.to_string(),
                origin: if api {
                    ProjectOrigin::Api { project_id: 1 }
                } else {
                    ProjectOrigin::LocalClone(std::path::PathBuf::new())
                },
                ci_config_path: None,
            },
            tags: tags.iter().map(|t| t.to_string()).collect(),
            files: files
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })
    }
}

impl ProjectSource for MemProject {
    fn meta(&self) -> &ProjectMeta {
        &self.meta
    }
    fn default_branch(&self) -> Result<String, SourceError> {
        Ok("main".into())
    }
    fn resolve_ref(&self, r: &str) -> Result<Option<Sha>, SourceError> {
        Ok(
            (r == "main" || r == SHA || self.tags.iter().any(|t| t == r))
                .then(|| Sha(SHA.to_string())),
        )
    }
    fn read(&self, at: &TreeRef, path: &str) -> Result<Option<String>, SourceError> {
        assert_eq!(*at, TreeRef::Commit(Sha(SHA.to_string())));
        Ok(self.files.get(path).cloned())
    }
    fn list_tree(&self, _: &TreeRef) -> Result<Arc<[String]>, SourceError> {
        Ok(self.files.keys().cloned().collect::<Vec<_>>().into())
    }
    fn tags(&self) -> Result<Vec<String>, SourceError> {
        Ok(self.tags.clone())
    }
}

/// A locator over in-memory projects; `explain_misses` makes it behave like
/// the API locator, which reports why a project is not available.
struct MemLocator {
    projects: Vec<Arc<MemProject>>,
    explain_misses: bool,
}

impl ProjectLocator for MemLocator {
    fn locate(&self, key: &ProjectKey) -> Result<Option<Arc<dyn ProjectSource>>, SourceError> {
        if let Some(p) = self.projects.iter().find(|p| p.meta.key == *key) {
            return Ok(Some(p.clone()));
        }
        if self.explain_misses && key.host == HOST {
            return Err(SourceError::Api(format!(
                "{}/{} is not visible through the API (404)",
                key.host, key.path_lc
            )));
        }
        Ok(None)
    }
    fn all(&self) -> Vec<ProjectMeta> {
        self.projects.iter().map(|p| p.meta.clone()).collect()
    }
}

struct FakeApi {
    templates: BTreeMap<String, String>,
    releases: BTreeMap<String, Vec<String>>,
}

impl InstanceApi for FakeApi {
    fn host(&self) -> &str {
        HOST
    }
    fn template(&self, name: &str) -> Result<Option<String>, SourceError> {
        Ok(self.templates.get(name).cloned())
    }
    fn release_tags(&self, key: &ProjectKey) -> Result<Option<Vec<String>>, SourceError> {
        Ok(self.releases.get(&key.path_lc).cloned())
    }
}

struct FakeRemote {
    bodies: BTreeMap<String, String>,
}

impl RemoteFetcher for FakeRemote {
    fn fetch(&self, url: &str, _integrity: Option<&str>) -> Result<String, SourceError> {
        self.bodies
            .get(url)
            .cloned()
            .ok_or_else(|| SourceError::Api(format!("GET {url}: HTTP 404")))
    }
}

const ROOT_CI: &str = r#"
include:
  - project: acme/lib
    file: /ci/shared.yml
  - project: acme/missing
    file: /x.yml
  - template: Gradle.gitlab-ci.yml
  - template: Jobs/Nope.gitlab-ci.yml
  - remote: https://elsewhere.example.org/remote.yml
  - remote: https://elsewhere.example.org/absent.yml
  - component: gitlab.example.com/acme/comp/build@~latest
  - component: gitlab.example.com/acme/comp/build@1
  - component: gitlab.example.com/acme/comp/build@9

stages: [build, deploy]

fan-out:
  stage: deploy
  trigger:
    project: acme/downstream

absent:
  stage: deploy
  trigger:
    project: acme/absent
"#;

fn scan(api: bool, allow_remote: bool) -> glpv_core::model::Graph {
    let root = MemProject::new("acme/root", false, &[], &[(".gitlab-ci.yml", ROOT_CI)]);
    let local = Arc::new(MemLocator {
        projects: vec![root.clone()],
        explain_misses: false,
    });
    let remote_projects = vec![
        MemProject::new(
            "acme/lib",
            true,
            &[],
            &[("ci/shared.yml", "shared:\n  stage: build\n  script: s\n")],
        ),
        MemProject::new(
            "acme/comp",
            true,
            &["1.0.0", "1.2.0", "2.0.0"],
            &[(
                "templates/build.yml",
                "component-build:\n  stage: build\n  script: c\n",
            )],
        ),
        MemProject::new(
            "acme/downstream",
            true,
            &[],
            &[(".gitlab-ci.yml", "down:\n  script: d\n")],
        ),
    ];
    let api_locator = Arc::new(MemLocator {
        projects: remote_projects,
        explain_misses: true,
    });
    let locator: Arc<dyn ProjectLocator> = if api {
        Arc::new(ChainLocator::new(vec![local, api_locator]))
    } else {
        local
    };
    let sources = Sources {
        locator: Some(locator),
        templates_key: ProjectKey::new("gitlab.com", "gitlab-org/gitlab"),
        api: api.then(|| {
            Arc::new(FakeApi {
                templates: [(
                    "Gradle.gitlab-ci.yml".to_string(),
                    "gradle:\n  stage: build\n  script: g\n".to_string(),
                )]
                .into_iter()
                .collect(),
                releases: [(
                    "acme/comp".to_string(),
                    vec![
                        "2.0.0".to_string(),
                        "1.2.0".to_string(),
                        "1.0.0".to_string(),
                    ],
                )]
                .into_iter()
                .collect(),
            }) as Arc<dyn InstanceApi>
        }),
        remote: allow_remote.then(|| {
            Arc::new(FakeRemote {
                bodies: [(
                    "https://elsewhere.example.org/remote.yml".to_string(),
                    "remote-job:\n  stage: build\n  script: r\n".to_string(),
                )]
                .into_iter()
                .collect(),
            }) as Arc<dyn RemoteFetcher>
        }),
    };
    let opts = ResolveOpts {
        embed_sources: false,
        allow_remote,
        ..ResolveOpts::default()
    };
    scan_entry(
        &sources,
        root,
        TreeRef::Commit(Sha(SHA.to_string())),
        Some("main".to_string()),
        None,
        &Scenario::push_default(),
        &opts,
        vec![],
        Vec::new(),
    )
    .graph
}

#[test]
fn the_api_unlocks_uncloned_projects_templates_remotes_and_catalog_versions() {
    let g = scan(true, true);
    let root = &g.pipelines[0];
    assert_eq!(root.kind, PipelineKind::Root);
    let jobs: Vec<&str> = root.jobs.iter().map(|j| j.name.as_str()).collect();
    for expected in [
        "shared",
        "gradle",
        "remote-job",
        "component-build",
        "fan-out",
        "absent",
    ] {
        assert!(jobs.contains(&expected), "{expected} missing from {jobs:?}");
    }

    let resolved: Vec<(IncludeKind, &str, Option<&str>)> = g
        .include_files
        .iter()
        .filter(|f| f.unresolved.is_none() && !matches!(f.kind, IncludeKind::Entry))
        .map(|f| {
            (
                f.kind,
                f.path.as_str(),
                f.project.as_ref().map(|p| p.path.as_str()),
            )
        })
        .collect();
    assert_eq!(
        resolved,
        vec![
            (IncludeKind::Project, "ci/shared.yml", Some("acme/lib")),
            (
                IncludeKind::Template,
                "lib/gitlab/ci/templates/Gradle.gitlab-ci.yml",
                Some("gitlab-org/gitlab")
            ),
            (
                IncludeKind::Remote,
                "https://elsewhere.example.org/remote.yml",
                None
            ),
            (
                IncludeKind::Component,
                "templates/build.yml",
                Some("acme/comp")
            ),
            (
                IncludeKind::Component,
                "templates/build.yml",
                Some("acme/comp")
            ),
        ]
    );

    let unresolved: Vec<(&str, UnresolvedReason, &str)> = g
        .include_files
        .iter()
        .filter_map(|f| {
            f.unresolved
                .as_ref()
                .map(|u| (f.location.as_str(), u.reason, u.detail.as_str()))
        })
        .collect();
    assert_eq!(unresolved.len(), 4, "{unresolved:?}");
    assert_eq!(unresolved[0].1, UnresolvedReason::ProjectNotFound);
    assert!(
        unresolved[0].2.contains("acme/missing")
            && unresolved[0].2.contains("not visible through the API"),
        "{}",
        unresolved[0].2
    );
    assert_eq!(unresolved[1].1, UnresolvedReason::TemplateUnavailable);
    assert!(
        unresolved[1].2.contains("Jobs/Nope.gitlab-ci.yml")
            && unresolved[1].2.contains("top-level templates only"),
        "{}",
        unresolved[1].2
    );
    assert_eq!(unresolved[2].1, UnresolvedReason::RemoteFailed);
    assert!(
        unresolved[2].2.contains("absent.yml") && unresolved[2].2.contains("HTTP 404"),
        "{}",
        unresolved[2].2
    );
    assert_eq!(unresolved[3].1, UnresolvedReason::ComponentNeedsCatalog);
    assert!(
        unresolved[3].2.contains("`9`") && unresolved[3].2.contains("no release matches"),
        "{}",
        unresolved[3].2
    );

    // The downstream project came from the API; the absent one says why not.
    let downstream = g
        .pipelines
        .iter()
        .find(|p| p.project.path == "acme/downstream")
        .expect("downstream pipeline");
    assert_eq!(downstream.kind, PipelineKind::MultiProject);
    assert_eq!(downstream.jobs.len(), 1);
    let absent = g
        .pipelines
        .iter()
        .find(|p| p.project.path == "acme/absent")
        .expect("absent pipeline");
    assert_eq!(absent.kind, PipelineKind::Unresolved);
    let u = absent.unresolved.as_ref().unwrap();
    assert_eq!(u.reason, UnresolvedReason::ProjectNotFound);
    assert!(
        u.detail
            .contains("no clone of gitlab.example.com/acme/absent")
            && u.detail.contains("not visible through the API"),
        "{}",
        u.detail
    );
    let codes: Vec<&str> = g.diagnostics.iter().map(|d| d.code.as_str()).collect();
    assert!(codes.contains(&"include.project-not-found"), "{codes:?}");
    assert!(codes.contains(&"trigger.project-not-found"), "{codes:?}");
    assert!(codes.contains(&"include.remote-failed"), "{codes:?}");
    assert!(
        codes.contains(&"include.component-needs-catalog"),
        "{codes:?}"
    );
}

#[test]
fn without_the_api_everything_stays_a_first_class_gap() {
    let g = scan(false, false);
    let reasons: Vec<(&str, UnresolvedReason)> = g
        .include_files
        .iter()
        .filter_map(|f| {
            f.unresolved
                .as_ref()
                .map(|u| (f.location.as_str(), u.reason))
        })
        .collect();
    assert_eq!(
        reasons,
        vec![
            (
                "acme/lib//ci/shared.yml@HEAD",
                UnresolvedReason::ProjectNotFound
            ),
            (
                "acme/missing//x.yml@HEAD",
                UnresolvedReason::ProjectNotFound
            ),
            (
                "Gradle.gitlab-ci.yml",
                UnresolvedReason::TemplateUnavailable
            ),
            (
                "Jobs/Nope.gitlab-ci.yml",
                UnresolvedReason::TemplateUnavailable
            ),
            (
                "https://elsewhere.example.org/remote.yml",
                UnresolvedReason::RemoteDisabled
            ),
            (
                "https://elsewhere.example.org/absent.yml",
                UnresolvedReason::RemoteDisabled
            ),
            (
                "gitlab.example.com/acme/comp/build@~latest",
                UnresolvedReason::ProjectNotFound
            ),
            (
                "gitlab.example.com/acme/comp/build@1",
                UnresolvedReason::ProjectNotFound
            ),
            (
                "gitlab.example.com/acme/comp/build@9",
                UnresolvedReason::ProjectNotFound
            ),
        ]
    );
    let missing = g
        .include_files
        .iter()
        .find(|f| f.location.starts_with("acme/missing"))
        .unwrap();
    assert_eq!(
        missing.unresolved.as_ref().unwrap().detail,
        "no clone of gitlab.example.com/acme/missing in the project index"
    );
    assert!(
        g.pipelines
            .iter()
            .all(|p| p.project.path != "acme/downstream" || p.kind == PipelineKind::Unresolved)
    );
}
