//! The GitLab REST API as a [`ProjectSource`]: scan projects that are not
//! cloned locally. Every read of immutable data (a file at a sha, a tree, a
//! compare between two shas) lands in the on-disk cache; ref lookups and
//! other mutable answers are cached for ten minutes (`--refresh` bypasses
//! them). See [`cache`] for the layout.
//!
//! Endpoints (all relative to `/api/v4`, documented at
//! <https://docs.gitlab.com/api/>):
//!
//! | what | endpoint |
//! |---|---|
//! | project metadata | `GET /projects/:path` |
//! | ref → sha | `GET /projects/:id/repository/commits/:ref` |
//! | file | `GET /projects/:id/repository/files/:path/raw?ref=:sha` |
//! | tree | `GET /projects/:id/repository/tree?recursive=true&ref=:sha&per_page=100&pagination=keyset` |
//! | tags | `GET /projects/:id/repository/tags?per_page=100` |
//! | changed files | `GET /projects/:id/repository/compare?from=:base&to=:head&straight=false` |
//! | CI templates | `GET /templates/gitlab_ci_ymls/:key` |
//! | catalog versions | `GET /projects/:id/releases?per_page=100` |

pub mod auth;
pub mod cache;
pub mod transport;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use self::cache::ApiCache;
use self::transport::{HttpResponse, Transport};
use super::{
    InstanceApi, ProjectKey, ProjectLocator, ProjectMeta, ProjectOrigin, ProjectSource,
    RemoteFetcher, Sha, SourceError, TreeRef,
};

const PER_PAGE: &str = "100";
/// Hard stop for a pagination loop (a million tree entries).
const MAX_PAGES: usize = 10_000;
/// The longest `Retry-After` honoured before giving up on a 429.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// `GET /projects/:path`, the part we keep.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub id: u64,
    pub path_with_namespace: String,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub ci_config_path: Option<String>,
}

pub struct ApiClient {
    /// Lower-cased host name, the project-key identity (no port).
    host: String,
    /// `https://host[:port]` — where `include:remote` URLs count as ours.
    origin: String,
    /// `https://host[:port]/api/v4`
    base: String,
    transport: Box<dyn Transport>,
    cache: ApiCache,
}

impl ApiClient {
    /// `host_or_url`: `gitlab.example.com`, `https://gitlab.example.com` or
    /// `http://localhost:8080`.
    pub fn new(host_or_url: &str, transport: Box<dyn Transport>, cache: ApiCache) -> ApiClient {
        let (origin, host) = split_origin(host_or_url);
        ApiClient {
            host,
            base: format!("{origin}/api/v4"),
            origin,
            transport,
            cache,
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn api_base(&self) -> &str {
        &self.base
    }

    pub fn cache(&self) -> &ApiCache {
        &self.cache
    }

    pub fn credentials(&self) -> String {
        self.transport.describe()
    }

    /// The `Authorization` header git needs to clone a private project with
    /// the token in hand (`None` for glab-managed or anonymous access).
    pub fn git_auth_header(&self) -> Option<String> {
        self.transport
            .auth()
            .map(|a| format!("Authorization: {}", a.git_basic_header()))
    }

    /// `https://host/group/project.git`
    pub fn git_http_url(&self, project_path: &str) -> String {
        format!("{}/{}.git", self.origin, project_path.trim_matches('/'))
    }

    fn url(&self, path: &str, query: &[(&str, &str)]) -> String {
        let mut url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        let mut sep = '?';
        for (k, v) in query {
            url.push(sep);
            url.push_str(k);
            url.push('=');
            url.push_str(&encode(v));
            sep = '&';
        }
        url
    }

    /// One GET on the configured host, with rate limiting honoured once.
    fn get_raw(&self, url: &str, authenticated: bool) -> Result<HttpResponse, SourceError> {
        let resp = self.transport.get(url, authenticated)?;
        if resp.status != 429 {
            return Ok(resp);
        }
        let wait = resp
            .header("retry-after")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .filter(|d| *d <= MAX_RETRY_AFTER);
        let Some(wait) = wait else {
            return Err(SourceError::Api(format!(
                "GET {}: HTTP 429 (rate limited by {}; no usable Retry-After)",
                describe_url(url),
                self.host
            )));
        };
        std::thread::sleep(wait);
        let again = self.transport.get(url, authenticated)?;
        if again.status == 429 {
            return Err(SourceError::Api(format!(
                "GET {}: HTTP 429 (still rate limited by {} after waiting {}s)",
                describe_url(url),
                self.host,
                wait.as_secs()
            )));
        }
        Ok(again)
    }

    /// GET with the status classified: `Ok(None)` for 404, an error naming
    /// the cause (never the token) for 401/403/5xx.
    fn get(&self, url: &str) -> Result<Option<HttpResponse>, SourceError> {
        let resp = self.get_raw(url, true)?;
        self.classify(url, resp)
    }

    fn classify(&self, url: &str, resp: HttpResponse) -> Result<Option<HttpResponse>, SourceError> {
        match resp.status {
            200..=299 => Ok(Some(resp)),
            404 => Ok(None),
            401 => Err(SourceError::Api(format!(
                "GET {}: HTTP 401 (unauthorized with {}); pass --token / GLPV_TOKEN, set \
                 GITLAB_TOKEN, or run `glab auth login --hostname {}`",
                describe_url(url),
                self.transport.describe(),
                self.host
            ))),
            403 => Err(SourceError::Api(format!(
                "GET {}: HTTP 403 (forbidden with {}; the token needs the read_api scope \
                 and access to the project)",
                describe_url(url),
                self.transport.describe()
            ))),
            status => Err(SourceError::Api(format!(
                "GET {}: HTTP {status}{}",
                describe_url(url),
                body_excerpt(&resp.body)
            ))),
        }
    }

    fn get_json(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Option<serde_json::Value>, SourceError> {
        let url = self.url(path, query);
        let Some(resp) = self.get(&url)? else {
            return Ok(None);
        };
        serde_json::from_slice(&resp.body)
            .map(Some)
            .map_err(|e| SourceError::Api(format!("GET {}: invalid JSON: {e}", describe_url(&url))))
    }

    /// Every page of a list endpoint: follow `Link: rel="next"` (keyset and
    /// offset pagination both send it), else `x-next-page`.
    fn get_all_pages(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Option<Vec<serde_json::Value>>, SourceError> {
        let mut url = self.url(path, query);
        let mut items = Vec::new();
        for _ in 0..MAX_PAGES {
            let Some(resp) = self.get(&url)? else {
                return if items.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(items))
                };
            };
            let page: Vec<serde_json::Value> = serde_json::from_slice(&resp.body).map_err(|e| {
                SourceError::Api(format!("GET {}: invalid JSON: {e}", describe_url(&url)))
            })?;
            items.extend(page);
            match next_page_url(&url, &resp) {
                Some(next) => url = next,
                None => return Ok(Some(items)),
            }
        }
        Err(SourceError::Api(format!(
            "GET {}: more than {MAX_PAGES} pages",
            describe_url(&url)
        )))
    }

    // ----- project-level reads -------------------------------------------

    /// `GET /projects/:path` (cached ten minutes).
    pub fn project_info(&self, project_path: &str) -> Result<Option<ProjectInfo>, SourceError> {
        let rel = cache::project_meta_path(&self.host, project_path);
        if let Some(info) = self.cache.get_fresh::<ProjectInfo>(&rel) {
            return Ok(Some(info));
        }
        let Some(v) = self.get_json(&format!("projects/{}", encode(project_path)), &[])? else {
            return Ok(None);
        };
        let mut info: ProjectInfo = serde_json::from_value(v).map_err(|e| {
            SourceError::Api(format!(
                "GET /projects/{project_path}: unexpected shape: {e}"
            ))
        })?;
        info.ci_config_path = info.ci_config_path.filter(|c| !c.trim().is_empty());
        info.default_branch = info.default_branch.filter(|b| !b.trim().is_empty());
        self.cache.put_dated(&rel, &info);
        Ok(Some(info))
    }

    /// Open a project through the API.
    pub fn project(
        self: &Arc<Self>,
        key: &ProjectKey,
    ) -> Result<Option<Arc<ApiProject>>, SourceError> {
        let Some(info) = self.project_info(&key.path_lc)? else {
            return Ok(None);
        };
        Ok(Some(Arc::new(ApiProject::new(self.clone(), info))))
    }

    /// `GET /projects/:id/repository/commits/:ref` → sha (cached ten minutes).
    pub fn commit_sha(&self, project_id: u64, r: &str) -> Result<Option<String>, SourceError> {
        let rel = cache::ref_path(&self.host, project_id, r);
        if let Some(sha) = self.cache.get_fresh::<String>(&rel) {
            return Ok(Some(sha));
        }
        let path = format!("projects/{project_id}/repository/commits/{}", encode(r));
        let Some(v) = self.get_json(&path, &[])? else {
            return Ok(None);
        };
        let Some(sha) = v.get("id").and_then(|s| s.as_str()).map(str::to_string) else {
            return Err(SourceError::Api(format!("GET /{path}: no commit id")));
        };
        self.cache.put_dated(&rel, &sha);
        Ok(Some(sha))
    }

    /// `GET /projects/:id/repository/files/:path/raw?ref=:sha` (cached forever).
    pub fn raw_file(
        &self,
        project_id: u64,
        sha: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>, SourceError> {
        let rel = cache::blob_path(&self.host, project_id, sha, path);
        if let Some(bytes) = self.cache.get_bytes(&rel) {
            return Ok(Some(bytes));
        }
        let url = self.url(
            &format!(
                "projects/{project_id}/repository/files/{}/raw",
                encode(path.trim_start_matches('/'))
            ),
            &[("ref", sha)],
        );
        let Some(resp) = self.get(&url)? else {
            return Ok(None);
        };
        self.cache.put_bytes(&rel, &resp.body);
        Ok(Some(resp.body))
    }

    /// Recursive blob listing at `sha` — of the whole tree, or of the
    /// directory `prefix` — in tree order (cached forever).
    pub fn tree(
        &self,
        project_id: u64,
        sha: &str,
        prefix: Option<&str>,
    ) -> Result<Arc<[String]>, SourceError> {
        let rel = cache::tree_path(&self.host, project_id, sha, prefix);
        if let Some(v) = self.cache.get_json::<Vec<String>>(&rel) {
            return Ok(v.into());
        }
        let mut query = vec![
            ("recursive", "true"),
            ("ref", sha),
            ("per_page", PER_PAGE),
            ("pagination", "keyset"),
        ];
        if let Some(p) = prefix {
            query.push(("path", p));
        }
        let items = self
            .get_all_pages(&format!("projects/{project_id}/repository/tree"), &query)?
            .unwrap_or_default();
        let paths: Vec<String> = items
            .iter()
            .filter(|e| e.get("type").and_then(|t| t.as_str()) != Some("tree"))
            .filter_map(|e| e.get("path").and_then(|p| p.as_str()))
            .map(str::to_string)
            .collect();
        self.cache.put_json(&rel, &paths);
        Ok(paths.into())
    }

    /// `GET /projects/:id/repository/tags` names (cached ten minutes).
    pub fn tags(&self, project_id: u64) -> Result<Vec<String>, SourceError> {
        let rel = cache::tags_path(&self.host, project_id);
        if let Some(v) = self.cache.get_fresh::<Vec<String>>(&rel) {
            return Ok(v);
        }
        let items = self
            .get_all_pages(
                &format!("projects/{project_id}/repository/tags"),
                &[("per_page", PER_PAGE)],
            )?
            .unwrap_or_default();
        let names: Vec<String> = items
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .map(str::to_string)
            .collect();
        self.cache.put_dated(&rel, &names);
        Ok(names)
    }

    /// Release tag names, newest first (cached ten minutes). `None` when the
    /// project has no visible releases (404 or 403).
    pub fn release_tags(&self, project_id: u64) -> Result<Option<Vec<String>>, SourceError> {
        let rel = cache::releases_path(&self.host, project_id);
        if let Some(v) = self.cache.get_fresh::<Vec<String>>(&rel) {
            return Ok(Some(v));
        }
        let url = self.url(
            &format!("projects/{project_id}/releases"),
            &[("per_page", PER_PAGE)],
        );
        let first = self.get_raw(&url, true)?;
        if first.status == 403 {
            return Ok(None);
        }
        let Some(first) = self.classify(&url, first)? else {
            return Ok(None);
        };
        let mut items: Vec<serde_json::Value> =
            serde_json::from_slice(&first.body).map_err(|e| {
                SourceError::Api(format!("GET {}: invalid JSON: {e}", describe_url(&url)))
            })?;
        let mut next = next_page_url(&url, &first);
        let mut pages = 1;
        while let Some(u) = next.take() {
            pages += 1;
            if pages > MAX_PAGES {
                break;
            }
            let Some(resp) = self.get(&u)? else {
                break;
            };
            let page: Vec<serde_json::Value> = serde_json::from_slice(&resp.body).map_err(|e| {
                SourceError::Api(format!("GET {}: invalid JSON: {e}", describe_url(&u)))
            })?;
            items.extend(page);
            next = next_page_url(&u, &resp);
        }
        let names: Vec<String> = items
            .iter()
            .filter(|r| r.get("upcoming_release").and_then(|u| u.as_bool()) != Some(true))
            .filter_map(|r| r.get("tag_name").and_then(|n| n.as_str()))
            .map(str::to_string)
            .collect();
        self.cache.put_dated(&rel, &names);
        Ok(Some(names))
    }

    /// Paths touched between the merge base of `base_sha` and `head_sha`
    /// (`straight=false`, i.e. `base...head`); a rename contributes both
    /// its old and its new path, like a push diff without rename detection.
    /// Cached forever.
    pub fn compare(
        &self,
        project_id: u64,
        base_sha: &str,
        head_sha: &str,
    ) -> Result<Option<Vec<String>>, SourceError> {
        let rel = cache::compare_path(&self.host, project_id, base_sha, head_sha);
        if let Some(v) = self.cache.get_json::<Vec<String>>(&rel) {
            return Ok(Some(v));
        }
        let Some(v) = self.get_json(
            &format!("projects/{project_id}/repository/compare"),
            &[("from", base_sha), ("to", head_sha), ("straight", "false")],
        )?
        else {
            return Ok(None);
        };
        if v.get("compare_timeout").and_then(|t| t.as_bool()) == Some(true) {
            return Err(SourceError::Api(format!(
                "compare {}...{} timed out on {}; the changed-file list would be incomplete",
                &base_sha[..base_sha.len().min(8)],
                &head_sha[..head_sha.len().min(8)],
                self.host
            )));
        }
        let mut files: Vec<String> = Vec::new();
        for d in v
            .get("diffs")
            .and_then(|d| d.as_array())
            .into_iter()
            .flatten()
        {
            let old = d.get("old_path").and_then(|p| p.as_str());
            let new = d.get("new_path").and_then(|p| p.as_str());
            for p in [old, new].into_iter().flatten() {
                if !files.iter().any(|f| f == p) {
                    files.push(p.to_string());
                }
            }
        }
        self.cache.put_json(&rel, &files);
        Ok(Some(files))
    }

    /// `GET /templates/gitlab_ci_ymls/:key` (cached ten minutes). The key
    /// is the template name without its `.gitlab-ci.yml` suffix.
    pub fn template(&self, name: &str) -> Result<Option<String>, SourceError> {
        let key = name.strip_suffix(".gitlab-ci.yml").unwrap_or(name);
        let rel = cache::template_path(&self.host, key);
        if let Some(t) = self.cache.get_fresh::<String>(&rel) {
            return Ok(Some(t));
        }
        let Some(v) = self.get_json(&format!("templates/gitlab_ci_ymls/{}", encode(key)), &[])?
        else {
            return Ok(None);
        };
        let Some(content) = v.get("content").and_then(|c| c.as_str()) else {
            return Err(SourceError::Api(format!(
                "GET /templates/gitlab_ci_ymls/{key}: no content"
            )));
        };
        self.cache.put_dated(&rel, &content.to_string());
        Ok(Some(content.to_string()))
    }

    /// An `include:remote` body: credentials only when the URL is on the
    /// configured instance; cached ten minutes.
    pub fn fetch_remote(&self, url: &str, integrity: Option<&str>) -> Result<String, SourceError> {
        let ours = url
            .strip_prefix(&self.origin)
            .is_some_and(|rest| rest.starts_with('/'));
        fetch_remote_with(self.transport.as_ref(), &self.cache, ours, url, integrity)
    }
}

/// The `include:remote` fetch shared by [`ApiClient`] and [`RemoteOnly`].
fn fetch_remote_with(
    transport: &dyn Transport,
    cache: &ApiCache,
    authenticated: bool,
    url: &str,
    integrity: Option<&str>,
) -> Result<String, SourceError> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(SourceError::Api(format!("`{url}` is not an http(s) URL")));
    }
    let rel = cache::remote_path(url);
    let text = match cache.get_fresh::<String>(&rel) {
        Some(t) => t,
        None => {
            let resp = transport.get(url, authenticated)?;
            if !(200..=299).contains(&resp.status) {
                return Err(SourceError::Api(format!(
                    "GET {url}: HTTP {}{}",
                    resp.status,
                    body_excerpt(&resp.body)
                )));
            }
            let text = String::from_utf8_lossy(&resp.body).into_owned();
            cache.put_dated(&rel, &text);
            text
        }
    };
    if let Some(expected) = integrity {
        let got = format!("sha256-{}", auth::base64(&sha256(text.as_bytes())));
        if got != expected.trim() {
            return Err(SourceError::Api(format!(
                "`{url}` does not match its `integrity` ({expected}); the fetched body is {got}"
            )));
        }
    }
    Ok(text)
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).to_vec()
}

/// `include:remote` without a configured instance: anonymous HTTPS plus the
/// cache.
pub struct RemoteOnly {
    transport: Box<dyn Transport>,
    cache: ApiCache,
}

impl RemoteOnly {
    pub fn new(transport: Box<dyn Transport>, cache: ApiCache) -> Self {
        RemoteOnly { transport, cache }
    }
}

impl RemoteFetcher for RemoteOnly {
    fn fetch(&self, url: &str, integrity: Option<&str>) -> Result<String, SourceError> {
        fetch_remote_with(self.transport.as_ref(), &self.cache, false, url, integrity)
    }
}

impl RemoteFetcher for ApiClient {
    fn fetch(&self, url: &str, integrity: Option<&str>) -> Result<String, SourceError> {
        self.fetch_remote(url, integrity)
    }
}

impl InstanceApi for ApiClient {
    fn host(&self) -> &str {
        &self.host
    }

    fn template(&self, name: &str) -> Result<Option<String>, SourceError> {
        ApiClient::template(self, name)
    }

    fn release_tags(&self, key: &ProjectKey) -> Result<Option<Vec<String>>, SourceError> {
        if key.host != self.host {
            return Ok(None);
        }
        let Some(info) = self.project_info(&key.path_lc)? else {
            return Ok(None);
        };
        ApiClient::release_tags(self, info.id)
    }
}

// ----- ApiProject -------------------------------------------------------------

/// `(sha, directory prefix)` → listing.
type TreeCacheEntry = ((String, String), Arc<[String]>);

pub struct ApiProject {
    meta: ProjectMeta,
    id: u64,
    default_branch: Option<String>,
    client: Arc<ApiClient>,
    tree_cache: Mutex<Vec<TreeCacheEntry>>,
    /// `(sha, path)` of every file read, for `--clone-missing` to warm the
    /// blobless clone with exactly what the scan needed.
    reads: Mutex<Vec<(String, String)>>,
}

impl ApiProject {
    pub fn new(client: Arc<ApiClient>, info: ProjectInfo) -> ApiProject {
        ApiProject {
            meta: ProjectMeta {
                key: ProjectKey::new(client.host(), &info.path_with_namespace),
                display_path: info.path_with_namespace.clone(),
                origin: ProjectOrigin::Api {
                    project_id: info.id,
                },
                ci_config_path: info.ci_config_path.clone(),
            },
            id: info.id,
            default_branch: info.default_branch,
            client,
            tree_cache: Mutex::new(Vec::new()),
            reads: Mutex::new(Vec::new()),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn client(&self) -> &Arc<ApiClient> {
        &self.client
    }

    pub fn reads(&self) -> Vec<(String, String)> {
        self.reads.lock().unwrap().clone()
    }
}

fn looks_like_sha(r: &str) -> bool {
    (7..=40).contains(&r.len()) && r.bytes().all(|b| b.is_ascii_hexdigit())
}

impl ProjectSource for ApiProject {
    fn meta(&self) -> &ProjectMeta {
        &self.meta
    }

    fn default_branch(&self) -> Result<String, SourceError> {
        self.default_branch.clone().ok_or_else(|| {
            SourceError::Api(format!(
                "{} has no default branch (empty repository?)",
                self.meta.display_path
            ))
        })
    }

    fn resolve_ref(&self, r: &str) -> Result<Option<Sha>, SourceError> {
        // As given first (branch, tag or sha all resolve), then the explicit
        // tag and branch spellings for names the short form does not reach.
        let mut candidates = vec![r.to_string()];
        if !looks_like_sha(r) && !r.starts_with("refs/") {
            candidates.push(format!("refs/tags/{r}"));
            candidates.push(format!("refs/heads/{r}"));
        }
        for c in candidates {
            if let Some(sha) = self.client.commit_sha(self.id, &c)? {
                return Ok(Some(Sha(sha)));
            }
        }
        Ok(None)
    }

    fn read(&self, at: &TreeRef, path: &str) -> Result<Option<String>, SourceError> {
        let sha = match at {
            TreeRef::Commit(s) => &s.0,
            TreeRef::Worktree => {
                return Err(SourceError::Api(format!(
                    "{} is read through the API and has no working tree",
                    self.meta.display_path
                )));
            }
        };
        let Some(bytes) = self.client.raw_file(self.id, sha, path)? else {
            return Ok(None);
        };
        self.reads
            .lock()
            .unwrap()
            .push((sha.clone(), path.to_string()));
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn list_tree(&self, at: &TreeRef) -> Result<Arc<[String]>, SourceError> {
        self.list_tree_under(at, "")
    }

    /// One `path=<prefix>` listing instead of the whole repository — the
    /// difference between a handful of pages and hundreds on a large one.
    fn list_tree_under(&self, at: &TreeRef, prefix: &str) -> Result<Arc<[String]>, SourceError> {
        let sha = match at {
            TreeRef::Commit(s) => s.0.clone(),
            TreeRef::Worktree => return Ok(Vec::new().into()),
        };
        let prefix = prefix.trim_matches('/').to_string();
        let key = (sha, prefix);
        if let Some((_, v)) = self
            .tree_cache
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| *k == key)
        {
            return Ok(v.clone());
        }
        let v = self.client.tree(
            self.id,
            &key.0,
            (!key.1.is_empty()).then_some(key.1.as_str()),
        )?;
        self.tree_cache.lock().unwrap().push((key, v.clone()));
        Ok(v)
    }

    fn tags(&self) -> Result<Vec<String>, SourceError> {
        self.client.tags(self.id)
    }

    fn changed_files(
        &self,
        base: &str,
        head: &TreeRef,
    ) -> Result<Option<Arc<[String]>>, SourceError> {
        let TreeRef::Commit(head_sha) = head else {
            return Ok(None);
        };
        let Some(base_sha) = self.resolve_ref(base)? else {
            return Ok(None);
        };
        Ok(self
            .client
            .compare(self.id, &base_sha.0, &head_sha.0)?
            .map(Into::into))
    }
}

// ----- ApiLocator -------------------------------------------------------------

/// Locates projects of the configured host through the API, remembering
/// what it opened (for `--clone-missing`) and what it could not (so a
/// missing project is asked for once per scan).
pub struct ApiLocator {
    client: Arc<ApiClient>,
    resolved: Mutex<Vec<(ProjectKey, Arc<ApiProject>)>>,
    misses: Mutex<HashMap<ProjectKey, String>>,
}

impl ApiLocator {
    pub fn new(client: Arc<ApiClient>) -> ApiLocator {
        ApiLocator {
            client,
            resolved: Mutex::new(Vec::new()),
            misses: Mutex::new(HashMap::new()),
        }
    }

    pub fn client(&self) -> &Arc<ApiClient> {
        &self.client
    }

    /// Every project opened through the API so far.
    pub fn resolved(&self) -> Vec<Arc<ApiProject>> {
        self.resolved
            .lock()
            .unwrap()
            .iter()
            .map(|(_, p)| p.clone())
            .collect()
    }
}

impl ProjectLocator for ApiLocator {
    fn locate(&self, key: &ProjectKey) -> Result<Option<Arc<dyn ProjectSource>>, SourceError> {
        if key.host != self.client.host() {
            return Ok(None);
        }
        if let Some((_, p)) = self.resolved.lock().unwrap().iter().find(|(k, _)| k == key) {
            return Ok(Some(p.clone() as Arc<dyn ProjectSource>));
        }
        if let Some(reason) = self.misses.lock().unwrap().get(key) {
            return Err(SourceError::Api(reason.clone()));
        }
        let outcome = self.client.project(key);
        let reason = match &outcome {
            Ok(Some(p)) => {
                self.resolved.lock().unwrap().push((key.clone(), p.clone()));
                return Ok(Some(p.clone() as Arc<dyn ProjectSource>));
            }
            Ok(None) => format!(
                "{}/{} is not visible through the API at {} ({}): it does not exist, or it is \
                 private and needs a token",
                key.host,
                key.path_lc,
                self.client.origin(),
                self.client.credentials()
            ),
            Err(SourceError::Api(m)) => m.clone(),
            Err(e) => e.to_string(),
        };
        self.misses
            .lock()
            .unwrap()
            .insert(key.clone(), reason.clone());
        Err(SourceError::Api(reason))
    }

    fn all(&self) -> Vec<ProjectMeta> {
        self.resolved
            .lock()
            .unwrap()
            .iter()
            .map(|(_, p)| p.meta().clone())
            .collect()
    }
}

// ----- helpers ----------------------------------------------------------------

/// `(origin, host)`: `gitlab.example.com` → (`https://gitlab.example.com`,
/// `gitlab.example.com`); a URL keeps its scheme and port for requests
/// while the host identity drops the port, like the local index does.
pub fn split_origin(host_or_url: &str) -> (String, String) {
    let s = host_or_url.trim().trim_end_matches('/');
    let (scheme, rest) = match s.split_once("://") {
        Some((sch, rest)) => (sch.to_lowercase(), rest),
        None => ("https".to_string(), s),
    };
    let hostport = rest.split('/').next().unwrap_or(rest).to_lowercase();
    let host = hostport.split(':').next().unwrap_or(&hostport).to_string();
    (format!("{scheme}://{hostport}"), host)
}

/// Percent-encode a path segment or query value (RFC 3986 unreserved
/// characters kept), so `group/project` becomes `group%2Fproject`.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The URL without its query string, for messages.
fn describe_url(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// `: <message>` from a JSON error body, or a short excerpt of a text one.
fn body_excerpt(body: &[u8]) -> String {
    if body.is_empty() {
        return String::new();
    }
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        for k in ["message", "error", "error_description"] {
            if let Some(m) = v.get(k) {
                let m = match m {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                return format!(": {m}");
            }
        }
    }
    let text = String::from_utf8_lossy(body);
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let excerpt: String = line.chars().take(120).collect();
    if excerpt.is_empty() {
        String::new()
    } else {
        format!(": {excerpt}")
    }
}

/// The next page of a list response: the `Link` header's `rel="next"`
/// target, else `x-next-page` applied to the current URL.
fn next_page_url(current: &str, resp: &HttpResponse) -> Option<String> {
    if let Some(link) = resp.header("link") {
        for part in link.split(',') {
            let part = part.trim();
            if part.contains("rel=\"next\"") || part.ends_with("rel=next") {
                let url = part.split(';').next()?.trim();
                let url = url.strip_prefix('<')?.strip_suffix('>')?;
                return Some(url.to_string());
            }
        }
        return None;
    }
    let next = resp.header("x-next-page")?.trim();
    if next.is_empty() {
        return None;
    }
    let (base, query) = current.split_once('?').unwrap_or((current, ""));
    let mut params: Vec<String> = query
        .split('&')
        .filter(|p| !p.is_empty() && !p.starts_with("page="))
        .map(str::to_string)
        .collect();
    params.push(format!("page={next}"));
    Some(format!("{base}?{}", params.join("&")))
}

#[cfg(test)]
mod tests {
    use super::transport::FakeTransport;
    use super::*;

    const HOST: &str = "gitlab.example.com";
    const API: &str = "https://gitlab.example.com/api/v4";

    fn make_client(tmp: &std::path::Path, fake: Arc<FakeTransport>) -> Arc<ApiClient> {
        Arc::new(ApiClient::new(
            HOST,
            Box::new(SharedFake(fake)),
            ApiCache::new(tmp.to_path_buf(), false),
        ))
    }

    /// The fake behind an `Arc` so a test can keep inspecting it.
    struct SharedFake(Arc<FakeTransport>);
    impl Transport for SharedFake {
        fn get(&self, url: &str, authenticated: bool) -> Result<HttpResponse, SourceError> {
            self.0.get(url, authenticated)
        }
        fn describe(&self) -> String {
            self.0.describe()
        }
        fn auth(&self) -> Option<&auth::Auth> {
            self.0.auth()
        }
    }

    fn project_json() -> &'static str {
        r#"{"id":42,"path_with_namespace":"Acme/API","default_branch":"main","ci_config_path":""}"#
    }

    #[test]
    fn origins_and_encoding() {
        assert_eq!(
            split_origin("gitlab.example.com"),
            (
                "https://gitlab.example.com".into(),
                "gitlab.example.com".into()
            )
        );
        assert_eq!(
            split_origin("https://GitLab.Example.com/"),
            (
                "https://gitlab.example.com".into(),
                "gitlab.example.com".into()
            )
        );
        assert_eq!(
            split_origin("http://localhost:8080"),
            ("http://localhost:8080".into(), "localhost".into())
        );
        assert_eq!(encode("acme/api"), "acme%2Fapi");
        assert_eq!(encode(".gitlab/ci/a b.yml"), ".gitlab%2Fci%2Fa%20b.yml");
        assert_eq!(encode("refs/tags/v1.0"), "refs%2Ftags%2Fv1.0");
    }

    #[test]
    fn next_page_from_link_or_header() {
        let resp = HttpResponse {
            status: 200,
            headers: vec![(
                "link".into(),
                "<https://h/api/v4/x?page_token=abc&per_page=100>; rel=\"next\", <https://h/api/v4/x?page=1>; rel=\"first\"".into(),
            )],
            body: Vec::new(),
        };
        assert_eq!(
            next_page_url("https://h/api/v4/x?per_page=100", &resp).as_deref(),
            Some("https://h/api/v4/x?page_token=abc&per_page=100")
        );
        let resp = HttpResponse {
            status: 200,
            headers: vec![("X-Next-Page".into(), "3".into())],
            body: Vec::new(),
        };
        assert_eq!(
            next_page_url("https://h/api/v4/x?per_page=100&page=2", &resp).as_deref(),
            Some("https://h/api/v4/x?per_page=100&page=3")
        );
        let resp = HttpResponse {
            status: 200,
            headers: vec![("x-next-page".into(), "".into())],
            body: Vec::new(),
        };
        assert!(next_page_url("https://h/api/v4/x", &resp).is_none());
    }

    #[test]
    fn project_source_over_a_fake_transport() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTransport::new());
        fake.route(
            &format!("{API}/projects/acme%2Fapi"),
            200,
            &[],
            project_json(),
        );
        fake.route(
            &format!("{API}/projects/42/repository/commits/main"),
            200,
            &[],
            r#"{"id":"1111111111111111111111111111111111111111"}"#,
        );
        // `v1` only resolves through its tag spelling.
        fake.route(
            &format!("{API}/projects/42/repository/commits/refs%2Ftags%2Fv1"),
            200,
            &[],
            r#"{"id":"2222222222222222222222222222222222222222"}"#,
        );
        let sha = "1111111111111111111111111111111111111111";
        fake.route(
            &format!("{API}/projects/42/repository/files/.gitlab-ci.yml/raw?ref={sha}"),
            200,
            &[],
            "stages: [a]\n",
        );
        fake.route(
            &format!(
                "{API}/projects/42/repository/tree?recursive=true&ref={sha}&per_page=100&pagination=keyset"
            ),
            200,
            &[(
                "Link",
                &format!(
                    "<{API}/projects/42/repository/tree?page_token=t2&per_page=100&recursive=true&ref={sha}>; rel=\"next\""
                ),
            )],
            r#"[{"type":"tree","path":"ci"},{"type":"blob","path":".gitlab-ci.yml"},{"type":"blob","path":"ci/a.yml"}]"#,
        );
        fake.route(
            &format!(
                "{API}/projects/42/repository/tree?page_token=t2&per_page=100&recursive=true&ref={sha}"
            ),
            200,
            &[],
            r#"[{"type":"blob","path":"ci/b.yml"}]"#,
        );
        fake.route(
            &format!(
                "{API}/projects/42/repository/tree?recursive=true&ref={sha}&per_page=100&pagination=keyset&path=ci"
            ),
            200,
            &[],
            r#"[{"type":"blob","path":"ci/a.yml"},{"type":"blob","path":"ci/b.yml"}]"#,
        );
        fake.route(
            &format!("{API}/projects/42/repository/tags?per_page=100"),
            200,
            &[("x-next-page", "2")],
            r#"[{"name":"v2"}]"#,
        );
        fake.route(
            &format!("{API}/projects/42/repository/tags?per_page=100&page=2"),
            200,
            &[("x-next-page", "")],
            r#"[{"name":"v1"}]"#,
        );
        let base = "2222222222222222222222222222222222222222";
        fake.route(
            &format!(
                "{API}/projects/42/repository/compare?from={base}&to={sha}&straight=false"
            ),
            200,
            &[],
            r#"{"diffs":[{"old_path":"a.yml","new_path":"a.yml"},{"old_path":"old.yml","new_path":"new.yml","renamed_file":true}],"compare_timeout":false}"#,
        );

        let client = make_client(tmp.path(), fake.clone());
        let locator = ApiLocator::new(client.clone());

        // Another host is not ours.
        assert!(
            locator
                .locate(&ProjectKey::new("other.example.com", "acme/api"))
                .unwrap()
                .is_none()
        );
        let key = ProjectKey::new(HOST, "acme/api");
        let p = locator.locate(&key).unwrap().expect("located");
        assert_eq!(p.meta().display_path, "Acme/API");
        assert!(matches!(
            p.meta().origin,
            ProjectOrigin::Api { project_id: 42 }
        ));
        assert_eq!(p.meta().ci_config_path, None);
        assert_eq!(p.default_branch().unwrap(), "main");
        assert_eq!(locator.all().len(), 1);

        let head = p.resolve_ref("main").unwrap().unwrap();
        assert_eq!(head.0, sha);
        assert_eq!(p.resolve_ref("v1").unwrap().unwrap().0, base);
        assert!(p.resolve_ref("nope").unwrap().is_none());
        // Missing refs are asked with all three spellings and never cached.
        assert_eq!(
            fake.count(&format!("{API}/projects/42/repository/commits/nope")),
            1
        );
        assert!(p.resolve_ref("nope").unwrap().is_none());
        assert_eq!(
            fake.count(&format!("{API}/projects/42/repository/commits/nope")),
            2
        );

        let tree = TreeRef::Commit(head.clone());
        assert_eq!(
            p.read(&tree, ".gitlab-ci.yml").unwrap().as_deref(),
            Some("stages: [a]\n")
        );
        assert!(p.read(&tree, "missing.yml").unwrap().is_none());
        assert!(p.read(&TreeRef::Worktree, "x").is_err());
        assert_eq!(
            locator.resolved()[0].reads(),
            vec![(sha.to_string(), ".gitlab-ci.yml".to_string())]
        );

        let listing = p.list_tree(&tree).unwrap();
        assert_eq!(&*listing, &[".gitlab-ci.yml", "ci/a.yml", "ci/b.yml"]);
        // A subtree is its own (cached) request.
        assert_eq!(
            &*p.list_tree_under(&tree, "ci/").unwrap(),
            &["ci/a.yml", "ci/b.yml"]
        );
        assert_eq!(&*p.list_tree_under(&tree, "").unwrap(), &*listing);
        assert_eq!(p.tags().unwrap(), vec!["v2", "v1"]);

        let changed = p.changed_files("v1", &tree).unwrap().unwrap();
        assert_eq!(&*changed, &["a.yml", "old.yml", "new.yml"]);
        assert!(p.changed_files("nope", &tree).unwrap().is_none());
        assert!(p.changed_files("v1", &TreeRef::Worktree).unwrap().is_none());

        // Everything immutable came from the cache on the second round.
        let calls_before = fake.calls().len();
        let client2 = make_client(tmp.path(), fake.clone());
        let p2 = client2.project(&key).unwrap().unwrap();
        let head2 = p2.resolve_ref("main").unwrap().unwrap();
        assert_eq!(head2, head);
        assert_eq!(
            p2.read(&tree, ".gitlab-ci.yml").unwrap().as_deref(),
            Some("stages: [a]\n")
        );
        assert_eq!(&*p2.list_tree(&tree).unwrap(), &*listing);
        assert_eq!(p2.list_tree_under(&tree, "ci").unwrap().len(), 2);
        assert_eq!(p2.tags().unwrap(), vec!["v2", "v1"]);
        assert_eq!(&*p2.changed_files("v1", &tree).unwrap().unwrap(), &*changed);
        // One request: `v1` as given misses again (misses are never cached)
        // before its cached tag spelling answers.
        assert_eq!(fake.calls().len(), calls_before + 1);
        assert!(
            tmp.path()
                .join("gitlab.example.com/42/blobs")
                .join(sha)
                .join(".gitlab-ci.yml")
                .is_file()
        );
        assert!(
            tmp.path()
                .join(format!("gitlab.example.com/42/tree/{sha}.json"))
                .is_file()
        );
        assert!(
            tmp.path()
                .join(format!("gitlab.example.com/42/tree/{sha}/ci.json"))
                .is_file()
        );
        assert!(
            tmp.path()
                .join("gitlab.example.com/42/refs/main.json")
                .is_file()
        );
        assert!(
            tmp.path()
                .join("gitlab.example.com/paths/acme%2Fapi.json")
                .is_file()
        );

        // `--refresh` asks for the ref again but still reads blobs from disk.
        let fresh = Arc::new(ApiClient::new(
            HOST,
            Box::new(SharedFake(fake.clone())),
            ApiCache::new(tmp.path().to_path_buf(), true),
        ));
        let p3 = fresh.project(&key).unwrap().unwrap();
        let before = fake.calls().len();
        p3.resolve_ref("main").unwrap();
        p3.read(&tree, ".gitlab-ci.yml").unwrap();
        assert_eq!(fake.calls().len(), before + 1);
    }

    #[test]
    fn errors_are_explained_without_echoing_the_token() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTransport::new());
        fake.route(
            &format!("{API}/projects/acme%2Fprivate"),
            401,
            &[],
            r#"{"message":"401 Unauthorized"}"#,
        );
        fake.route(
            &format!("{API}/projects/acme%2Fforbidden"),
            403,
            &[],
            r#"{"message":"403 Forbidden"}"#,
        );
        fake.route(
            &format!("{API}/projects/acme%2Fbroken"),
            500,
            &[],
            "<html>Internal Server Error</html>",
        );
        let client = make_client(tmp.path(), fake.clone());
        let e = client.project_info("acme/private").unwrap_err().to_string();
        assert!(e.contains("HTTP 401"), "{e}");
        assert!(e.contains("--token"), "{e}");
        assert!(!e.contains("secret"), "{e}");
        let e = client
            .project_info("acme/forbidden")
            .unwrap_err()
            .to_string();
        assert!(e.contains("HTTP 403"), "{e}");
        let e = client.project_info("acme/broken").unwrap_err().to_string();
        assert!(
            e.contains("HTTP 500: <html>Internal Server Error</html>"),
            "{e}"
        );
        assert!(client.project_info("acme/missing").unwrap().is_none());
        // Errors are not cached.
        assert!(!tmp.path().join("gitlab.example.com/paths").exists());

        // The locator remembers a miss for the scan and explains it.
        let locator = ApiLocator::new(client.clone());
        let key = ProjectKey::new(HOST, "acme/missing");
        let e = match locator.locate(&key) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a missing project is an explained error"),
        };
        assert!(e.contains("not visible through the API"), "{e}");
        let n = fake.count(&format!("{API}/projects/acme%2Fmissing"));
        let _ = locator.locate(&key);
        assert_eq!(fake.count(&format!("{API}/projects/acme%2Fmissing")), n);
    }

    #[test]
    fn rate_limits_are_retried_once() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTransport::new());
        let url = format!("{API}/projects/acme%2Fapi");
        fake.route(&url, 429, &[("Retry-After", "0")], "");
        fake.route(&url, 200, &[], project_json());
        let client = make_client(tmp.path(), fake.clone());
        assert_eq!(client.project_info("acme/api").unwrap().unwrap().id, 42);
        assert_eq!(fake.count(&url), 2);

        // A second 429 gives up (the last route repeats)...
        let fake = Arc::new(FakeTransport::new());
        fake.route(&url, 429, &[("Retry-After", "0")], "");
        let client = make_client(&tmp.path().join("a"), fake.clone());
        let e = client.project_info("acme/api").unwrap_err().to_string();
        assert!(e.contains("still rate limited"), "{e}");
        assert_eq!(fake.count(&url), 2);
        // ...and a missing Retry-After gives up at once.
        let fake = Arc::new(FakeTransport::new());
        fake.route(&url, 429, &[], "");
        let client = ApiClient::new(
            HOST,
            Box::new(SharedFake(fake.clone())),
            ApiCache::new(tmp.path().join("b"), false),
        );
        let e = client.project_info("acme/api").unwrap_err().to_string();
        assert!(e.contains("429"), "{e}");
        assert_eq!(fake.count(&url), 1);
    }

    #[test]
    fn templates_releases_and_remote_bodies() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeTransport::new());
        fake.route(
            &format!("{API}/templates/gitlab_ci_ymls/Gradle"),
            200,
            &[],
            r#"{"name":"Gradle","content":"build:\n  script: gradle\n"}"#,
        );
        fake.route(
            &format!("{API}/projects/acme%2Fcomp"),
            200,
            &[],
            r#"{"id":7,"path_with_namespace":"acme/comp","default_branch":"main"}"#,
        );
        fake.route(
            &format!("{API}/projects/7/releases?per_page=100"),
            200,
            &[],
            r#"[{"tag_name":"2.0.0"},{"tag_name":"1.2.3","upcoming_release":true},{"tag_name":"1.2.2"}]"#,
        );
        fake.route(
            "https://gitlab.example.com/acme/comp/-/raw/main/x.yml",
            200,
            &[],
            "x: 1\n",
        );
        fake.route("https://elsewhere.example.org/y.yml", 200, &[], "y: 2\n");
        let client = make_client(tmp.path(), fake.clone());

        assert_eq!(
            InstanceApi::template(&*client, "Gradle.gitlab-ci.yml")
                .unwrap()
                .as_deref(),
            Some("build:\n  script: gradle\n")
        );
        assert!(
            InstanceApi::template(&*client, "Jobs/Nope.gitlab-ci.yml")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            InstanceApi::release_tags(&*client, &ProjectKey::new(HOST, "acme/comp")).unwrap(),
            Some(vec!["2.0.0".to_string(), "1.2.2".to_string()])
        );
        assert!(
            InstanceApi::release_tags(&*client, &ProjectKey::new("other.example.com", "acme/comp"))
                .unwrap()
                .is_none()
        );

        // Credentials only travel to the configured origin.
        assert_eq!(
            client
                .fetch(
                    "https://gitlab.example.com/acme/comp/-/raw/main/x.yml",
                    None
                )
                .unwrap(),
            "x: 1\n"
        );
        assert_eq!(
            client
                .fetch("https://elsewhere.example.org/y.yml", None)
                .unwrap(),
            "y: 2\n"
        );
        let calls = fake.calls.lock().unwrap().clone();
        assert!(calls.iter().any(|(u, a)| u.ends_with("/x.yml") && *a));
        assert!(calls.iter().any(|(u, a)| u.ends_with("/y.yml") && !*a));
        assert!(client.fetch("ftp://x/y.yml", None).is_err());

        // `integrity` is the base64 sha256 of the body.
        let ok = format!("sha256-{}", auth::base64(&sha256(b"y: 2\n")));
        assert!(
            client
                .fetch("https://elsewhere.example.org/y.yml", Some(&ok))
                .is_ok()
        );
        let e = client
            .fetch("https://elsewhere.example.org/y.yml", Some("sha256-AAAA"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("integrity"), "{e}");
        // Cached by URL: one fetch served every call above.
        assert_eq!(fake.count("https://elsewhere.example.org/y.yml"), 1);
        assert!(tmp.path().join("remote").is_dir());
    }
}
