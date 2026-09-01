//! How bytes reach the API: direct HTTPS (reqwest over rustls) or the
//! already-authenticated `glab api` CLI. Both sit behind one trait so the
//! client and its tests do not care.

use std::process::Command;
use std::time::Duration;

use super::auth::Auth;
use crate::source::SourceError;

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub trait Transport: Send + Sync {
    /// GET `url`. `authenticated` attaches the credentials — the client only
    /// asks for that on the configured host.
    fn get(&self, url: &str, authenticated: bool) -> Result<HttpResponse, SourceError>;
    /// Where the credentials come from, for diagnostics (never the token).
    fn describe(&self) -> String;
    /// The token in hand, if any (for `git clone` of private projects).
    fn auth(&self) -> Option<&Auth> {
        None
    }
}

/// Direct HTTPS.
pub struct HttpsTransport {
    client: reqwest::blocking::Client,
    auth: Option<Auth>,
    label: String,
}

impl HttpsTransport {
    pub fn new(auth: Option<Auth>, label: impl Into<String>) -> Result<Self, SourceError> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("glpv/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| SourceError::Api(format!("cannot build the HTTP client: {e}")))?;
        Ok(HttpsTransport {
            client,
            auth,
            label: label.into(),
        })
    }
}

impl Transport for HttpsTransport {
    fn get(&self, url: &str, authenticated: bool) -> Result<HttpResponse, SourceError> {
        let mut req = self.client.get(url);
        if authenticated && let Some(auth) = &self.auth {
            let (name, value) = auth.header();
            req = req.header(name, value);
        }
        let resp = req
            .send()
            .map_err(|e| SourceError::Api(format!("GET {url}: {}", without_url(e))))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    v.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let body = resp
            .bytes()
            .map_err(|e| SourceError::Api(format!("GET {url}: {}", without_url(e))))?
            .to_vec();
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    fn describe(&self) -> String {
        self.label.clone()
    }

    fn auth(&self) -> Option<&Auth> {
        self.auth.as_ref()
    }
}

/// reqwest repeats the URL in its messages; ours already names it.
fn without_url(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut s = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(inner) = src {
        s.push_str(": ");
        s.push_str(&inner.to_string());
        src = inner.source();
    }
    s
}

/// `glab api --hostname <host> --include <endpoint>` for the configured
/// host; anything else (remote includes elsewhere) goes over plain HTTPS.
pub struct GlabTransport {
    host: String,
    api_base: String,
    fallback: HttpsTransport,
}

impl GlabTransport {
    /// `api_base` is the configured instance's `…/api/v4/` prefix.
    pub fn new(host: &str, api_base: &str) -> Result<Self, SourceError> {
        Ok(GlabTransport {
            host: host.to_string(),
            api_base: api_base.trim_end_matches('/').to_string() + "/",
            fallback: HttpsTransport::new(None, "anonymous")?,
        })
    }
}

impl Transport for GlabTransport {
    fn get(&self, url: &str, authenticated: bool) -> Result<HttpResponse, SourceError> {
        let Some(endpoint) = url.strip_prefix(&self.api_base).filter(|_| authenticated) else {
            return self.fallback.get(url, false);
        };
        let out = Command::new("glab")
            .args(["api", "--hostname", &self.host, "--include", endpoint])
            .env("NO_COLOR", "1")
            .env("GLAB_NO_UPDATE_NOTIFIER", "1")
            .output()
            .map_err(|e| SourceError::Api(format!("cannot run `glab api`: {e}")))?;
        match parse_glab_output(&out.stdout) {
            Some(resp) => Ok(resp),
            None => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let reason = stderr
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty() && !l.eq_ignore_ascii_case("error"))
                    .unwrap_or("no response")
                    .to_string();
                Err(SourceError::Api(format!(
                    "`glab api --hostname {} {endpoint}` failed: {reason}",
                    self.host
                )))
            }
        }
    }

    fn describe(&self) -> String {
        format!("glab api (host {} from glab's config)", self.host)
    }
}

/// `--include` output: `HTTP/2.0 200 OK`, one `Name: value` per line, a
/// blank line, then the body.
pub fn parse_glab_output(stdout: &[u8]) -> Option<HttpResponse> {
    let text_head = String::from_utf8_lossy(&stdout[..stdout.len().min(64)]);
    if !text_head.starts_with("HTTP/") {
        return None;
    }
    let (head_end, body_start) = find_blank_line(stdout)?;
    let head = String::from_utf8_lossy(&stdout[..head_end]);
    let mut lines = head.lines();
    let status_line = lines.next()?;
    let status: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    let headers = lines
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    Some(HttpResponse {
        status,
        headers,
        body: stdout[body_start..].to_vec(),
    })
}

fn find_blank_line(bytes: &[u8]) -> Option<(usize, usize)> {
    let crlf = bytes.windows(4).position(|w| w == b"\r\n\r\n");
    let lf = bytes.windows(2).position(|w| w == b"\n\n");
    match (crlf, lf) {
        (Some(a), Some(b)) if b < a => Some((b, b + 2)),
        (Some(a), _) => Some((a, a + 4)),
        (None, Some(b)) => Some((b, b + 2)),
        (None, None) => None,
    }
}

/// `(url, status, headers, body)` a [`FakeTransport`] answers with.
#[cfg(test)]
type Route = (String, u16, Vec<(String, String)>, Vec<u8>);

/// A canned transport for tests: URL → response, with a call log.
#[cfg(test)]
#[derive(Default)]
pub struct FakeTransport {
    pub routes: std::sync::Mutex<Vec<Route>>,
    pub calls: std::sync::Mutex<Vec<(String, bool)>>,
    pub auth: Option<Auth>,
}

#[cfg(test)]
impl FakeTransport {
    pub fn new() -> Self {
        FakeTransport::default()
    }

    pub fn route(&self, url: &str, status: u16, headers: &[(&str, &str)], body: &str) {
        self.routes.lock().unwrap().push((
            url.to_string(),
            status,
            headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body.as_bytes().to_vec(),
        ));
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(u, _)| u.clone())
            .collect()
    }

    pub fn count(&self, url: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(u, _)| u == url)
            .count()
    }
}

#[cfg(test)]
impl Transport for FakeTransport {
    fn get(&self, url: &str, authenticated: bool) -> Result<HttpResponse, SourceError> {
        self.calls
            .lock()
            .unwrap()
            .push((url.to_string(), authenticated));
        let n = self.count(url);
        let routes = self.routes.lock().unwrap();
        // Repeated routes for one URL answer successive calls in order
        // (the last one repeats), so a 429-then-200 sequence is expressible.
        let matching: Vec<_> = routes.iter().filter(|(u, ..)| u == url).collect();
        let Some((_, status, headers, body)) =
            matching.get(n - 1).or_else(|| matching.last()).copied()
        else {
            return Ok(HttpResponse {
                status: 404,
                headers: Vec::new(),
                body: br#"{"message":"404 Not Found"}"#.to_vec(),
            });
        };
        Ok(HttpResponse {
            status: *status,
            headers: headers.clone(),
            body: body.clone(),
        })
    }

    fn describe(&self) -> String {
        "fake".to_string()
    }

    fn auth(&self) -> Option<&Auth> {
        self.auth.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    /// A one-shot HTTP responder: answers `replies` in order, recording
    /// each request head.
    fn serve(replies: Vec<&'static str>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut heads = Vec::new();
            for reply in replies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = vec![0u8; 8192];
                let mut head = Vec::new();
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    head.extend_from_slice(&buf[..n]);
                    if n == 0 || head.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                heads.push(String::from_utf8_lossy(&head).into_owned());
                stream.write_all(reply.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
            heads
        });
        (base, handle)
    }

    #[test]
    fn https_transport_sends_the_right_header_only_when_asked() {
        let ok = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Next-Page: 2\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
        let limited = "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (base, server) = serve(vec![ok, ok, ok, limited]);

        let private =
            HttpsTransport::new(Some(Auth::PrivateToken("not-a-real-token".into())), "test")
                .unwrap();
        let r = private
            .get(&format!("{base}/api/v4/projects/1"), true)
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.header("x-next-page"), Some("2"));
        assert_eq!(r.body, b"{}");
        private.get(&format!("{base}/other"), false).unwrap();

        let bearer =
            HttpsTransport::new(Some(Auth::Bearer("oauth-placeholder".into())), "test").unwrap();
        bearer.get(&format!("{base}/api/v4/version"), true).unwrap();
        let r = bearer.get(&format!("{base}/api/v4/limited"), true).unwrap();
        assert_eq!(r.status, 429);
        assert_eq!(r.header("Retry-After"), Some("1"));

        // Header names are compared case-insensitively (an HTTP/1.1 client
        // may lower-case them on the wire).
        let heads: Vec<String> = server
            .join()
            .unwrap()
            .into_iter()
            .map(|h| h.to_lowercase())
            .collect();
        assert!(
            heads[0].contains("private-token: not-a-real-token"),
            "{}",
            heads[0]
        );
        assert!(heads[0].contains("user-agent: glpv/"), "{}", heads[0]);
        assert!(!heads[1].contains("private-token"), "{}", heads[1]);
        assert!(!heads[1].contains("authorization"), "{}", heads[1]);
        assert!(
            heads[2].contains("authorization: bearer oauth-placeholder"),
            "{}",
            heads[2]
        );
    }

    #[test]
    fn parses_glab_include_output() {
        let out =
            b"HTTP/2.0 200 OK\nContent-Type: application/json\r\nX-Next-Page: 2\r\n\r\n{\"a\":1}";
        let r = parse_glab_output(out).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.header("x-next-page"), Some("2"));
        assert_eq!(r.body, b"{\"a\":1}");

        let out = b"HTTP/1.1 404 Not Found\n\n{\"message\":\"404 Not Found\"}";
        let r = parse_glab_output(out).unwrap();
        assert_eq!(r.status, 404);
        assert_eq!(r.body, b"{\"message\":\"404 Not Found\"}");

        assert!(parse_glab_output(b"ERROR Unauthenticated.").is_none());
        assert!(parse_glab_output(b"").is_none());
    }
}
