//! `glpv serve`'s HTTP layer over a temporary output directory: static files
//! with the reload script injected into index.html, path escapes refused,
//! and the /events stream delivering a reload when the generation bumps.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use glpv_cli::serve::{Generation, RELOAD_SCRIPT, Server};

fn get(addr: std::net::SocketAddr, path: &str) -> (String, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: t\r\n\r\n").unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap();
    (head.to_string(), body.to_string())
}

#[test]
fn serves_files_injects_reload_and_streams_events() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("index.html"),
        "<title>t</title><div id=app></div>",
    )
    .unwrap();
    std::fs::write(dir.path().join("graph.json"), "{\"pipelines\":[]}").unwrap();
    std::fs::create_dir(dir.path().join("mermaid")).unwrap();
    std::fs::write(dir.path().join("mermaid/overview.mmd"), "flowchart LR").unwrap();

    let generation = Arc::new(Generation::default());
    let server = Arc::new(Server::new(dir.path().to_path_buf(), generation.clone()));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || server.run(listener));

    let (head, body) = get(addr, "/");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains("text/html"), "{head}");
    assert!(body.starts_with("<title>t</title>"));
    assert!(body.ends_with(RELOAD_SCRIPT), "reload script appended");

    let (head, body) = get(addr, "/graph.json?x=1");
    assert!(head.contains("application/json"), "{head}");
    assert_eq!(body, "{\"pipelines\":[]}");

    let (head, _) = get(addr, "/mermaid/overview.mmd");
    assert!(head.starts_with("HTTP/1.1 200"));
    let (head, _) = get(addr, "/../Cargo.toml");
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");
    let (head, _) = get(addr, "/nope.txt");
    assert!(head.starts_with("HTTP/1.1 404"), "{head}");

    // events: headers, the retry hint, then a reload when the generation moves
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(s, "GET /events HTTP/1.1\r\nHost: t\r\n\r\n").unwrap();
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 200"), "{line}");
    let mut saw_stream = false;
    loop {
        line.clear();
        r.read_line(&mut line).unwrap();
        if line.contains("text/event-stream") {
            saw_stream = true;
        }
        if line == "\r\n" {
            break;
        }
    }
    assert!(saw_stream);
    line.clear();
    r.read_line(&mut line).unwrap();
    assert_eq!(line, "retry: 1000\n");
    std::thread::sleep(Duration::from_millis(50));
    generation.bump();
    let mut got = String::new();
    for _ in 0..4 {
        line.clear();
        r.read_line(&mut line).unwrap();
        got.push_str(&line);
        if got.contains("data: reload") {
            break;
        }
    }
    assert!(got.contains("data: reload\n"), "{got:?}");
}
