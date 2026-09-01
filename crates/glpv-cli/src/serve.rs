//! The tiny HTTP server behind `glpv serve`: static files from the scan's
//! output directory, `index.html` with a reload script injected, and an
//! `/events` server-sent-events stream that fires whenever a rescan produced a
//! new generation. Standard library only (a thread per connection), so the
//! default binary stays small.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Script appended to the served `index.html`: reload when the server says
/// so. `location.reload()` keeps the URL hash, so the simulation, selection
/// and camera survive.
pub const RELOAD_SCRIPT: &str = r#"
<script>
(function () {
  if (typeof EventSource === "undefined") return;
  var es = new EventSource("/events");
  es.onmessage = function (e) { if (e.data === "reload") location.reload(); };
})();
</script>
"#;

/// Monotonic generation counter shared by the rescanner and the SSE streams.
#[derive(Default)]
pub struct Generation {
    inner: Mutex<u64>,
    changed: Condvar,
}

impl Generation {
    pub fn current(&self) -> u64 {
        *self.inner.lock().unwrap()
    }
    pub fn bump(&self) -> u64 {
        let mut g = self.inner.lock().unwrap();
        *g += 1;
        self.changed.notify_all();
        *g
    }
    /// Wait until the generation moves past `seen`, or `timeout` elapses.
    pub fn wait_past(&self, seen: u64, timeout: Duration) -> Option<u64> {
        let g = self.inner.lock().unwrap();
        let (g, _) = self
            .changed
            .wait_timeout_while(g, timeout, |g| *g <= seen)
            .unwrap();
        if *g > seen { Some(*g) } else { None }
    }
}

pub struct Server {
    pub root: PathBuf,
    pub generation: Arc<Generation>,
}

/// A parsed request: method and decoded path (query dropped).
#[derive(Debug, PartialEq)]
pub struct Request {
    pub method: String,
    pub path: String,
}

pub fn parse_request<R: BufRead>(r: &mut R) -> Option<Request> {
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    let path = target.split('?').next().unwrap_or("/").to_string();
    // headers are read and ignored
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).ok()? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
    Some(Request { method, path })
}

/// Map a URL path onto a file under the root, refusing anything that would
/// escape it. `/` maps to `index.html`.
pub fn resolve_path(root: &Path, url_path: &str) -> Option<PathBuf> {
    let rel = url_path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let rel = Path::new(rel);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    let full = root.join(rel);
    if full.is_dir() {
        return Some(full.join("index.html"));
    }
    Some(full)
}

pub fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("json") => "application/json",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("dot") => "text/vnd.graphviz",
        Some("mmd") => "text/plain; charset=utf-8",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

fn write_response(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

impl Server {
    pub fn new(root: PathBuf, generation: Arc<Generation>) -> Self {
        Server { root, generation }
    }

    /// Serve forever on `listener`, one thread per connection.
    pub fn run(self: Arc<Self>, listener: TcpListener) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let me = self.clone();
            std::thread::spawn(move || me.handle(stream));
        }
    }

    pub fn handle(&self, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let req = {
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => return,
            });
            match parse_request(&mut reader) {
                Some(r) => r,
                None => return,
            }
        };
        if req.method != "GET" && req.method != "HEAD" {
            write_response(
                &mut stream,
                "405 Method Not Allowed",
                "text/plain",
                b"GET only\n",
            );
            return;
        }
        if req.path == "/events" {
            self.events(stream);
            return;
        }
        let Some(path) = resolve_path(&self.root, &req.path) else {
            write_response(&mut stream, "404 Not Found", "text/plain", b"not found\n");
            return;
        };
        let mut body = Vec::new();
        match std::fs::File::open(&path).and_then(|mut f| f.read_to_end(&mut body)) {
            Ok(_) => {}
            Err(_) => {
                write_response(&mut stream, "404 Not Found", "text/plain", b"not found\n");
                return;
            }
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("index.html") {
            body.extend_from_slice(RELOAD_SCRIPT.as_bytes());
        }
        write_response(&mut stream, "200 OK", content_type(&path), &body);
    }

    /// Server-sent events: `reload` on every new generation, a comment every
    /// 15 s to keep proxies from closing the stream.
    fn events(&self, mut stream: TcpStream) {
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\nretry: 1000\n\n";
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }
        let mut seen = self.generation.current();
        loop {
            match self.generation.wait_past(seen, Duration::from_secs(15)) {
                Some(g) => {
                    seen = g;
                    if stream.write_all(b"data: reload\n\n").is_err() {
                        return;
                    }
                }
                None => {
                    if stream.write_all(b": ping\n\n").is_err() {
                        return;
                    }
                }
            }
            let _ = stream.flush();
        }
    }
}

/// Paths a watcher must ignore: git internals, `node_modules`, and the
/// scan's own output directory. Anything else under a repository may be
/// CI-relevant (`exists:` rules look at every file), so a change there
/// rescans — the debounce absorbs bursts.
pub fn is_ignored(path: &Path, out_dir: &Path) -> bool {
    if path.starts_with(out_dir) {
        return true;
    }
    path.components()
        .any(|c| matches!(c, Component::Normal(n) if n == ".git" || n == "node_modules"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn requests_parse_and_paths_stay_inside_the_root() {
        let mut r = Cursor::new("GET /graph.json?x=1 HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(
            parse_request(&mut r),
            Some(Request {
                method: "GET".into(),
                path: "/graph.json".into()
            })
        );
        let root = Path::new("/srv/out");
        assert_eq!(resolve_path(root, "/"), Some(root.join("index.html")));
        assert_eq!(
            resolve_path(root, "/mermaid/a.mmd"),
            Some(root.join("mermaid/a.mmd"))
        );
        assert_eq!(resolve_path(root, "/../etc/passwd"), None);
        assert_eq!(resolve_path(root, "/a/../../x"), None);
        assert_eq!(
            content_type(Path::new("x.html")),
            "text/html; charset=utf-8"
        );
    }

    #[test]
    fn generation_waits_and_bumps() {
        let g = Arc::new(Generation::default());
        assert_eq!(g.current(), 0);
        assert_eq!(g.wait_past(0, Duration::from_millis(20)), None);
        let g2 = g.clone();
        let t = std::thread::spawn(move || g2.wait_past(0, Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(g.bump(), 1);
        assert_eq!(t.join().unwrap(), Some(1));
    }

    #[test]
    fn ignored_paths() {
        let out = Path::new("/w/glpv-out");
        assert!(is_ignored(Path::new("/w/glpv-out/index.html"), out));
        assert!(is_ignored(Path::new("/w/repo/.git/index"), out));
        assert!(is_ignored(
            Path::new("/w/repo/node_modules/x/index.js"),
            out
        ));
        assert!(!is_ignored(Path::new("/w/repo/target/ci.yml"), out));
        assert!(!is_ignored(Path::new("/w/repo/.gitlab-ci.yml"), out));
    }
}
