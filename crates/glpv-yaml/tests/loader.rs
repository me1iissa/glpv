use glpv_yaml::{FileId, Kind, Node, Value, emit_document, parse, semantic_eq};

fn f() -> FileId {
    FileId(0)
}

fn root(text: &str) -> Node {
    let (docs, _) = parse(f(), text).unwrap();
    docs.into_iter().next().unwrap().root.unwrap()
}

#[test]
fn spans_are_one_based() {
    let r = root("a: b\n");
    let m = r.as_map().unwrap();
    let e = &m.entries["a"];
    assert_eq!((e.key_span.start.line, e.key_span.start.col), (1, 1));
    assert_eq!((e.key_span.end.line, e.key_span.end.col), (1, 2));
    assert_eq!((e.value.span.start.line, e.value.span.start.col), (1, 4));
}

#[test]
fn anchors_and_aliases_clone_with_provenance() {
    let text = "base: &b\n  k: v\nuse:\n  copy: *b\n";
    let r = root(text);
    let copy = r.get("use").unwrap().get("copy").unwrap();
    assert_eq!(copy.get("k").unwrap().as_str(), Some("v"));
    // Span stays at the anchor definition; alias_at records the use site.
    assert_eq!(copy.span.start.line, 2);
    let alias_at = copy.alias_at.expect("alias_at set");
    assert_eq!(alias_at.start.line, 4);
}

#[test]
fn merge_key_psych_semantics() {
    // Keys BEFORE `<<` are clobbered by the merge; keys AFTER win over it.
    let text = "\
base: &b\n  x: from-base\n  y: from-base\n  z: from-base\n\
job:\n  x: before\n  <<: *b\n  y: after\n";
    let (docs, diags) = parse(f(), text).unwrap();
    let r = docs[0].root.as_ref().unwrap();
    let job = r.get("job").unwrap().as_map().unwrap();
    assert_eq!(job.get("x").unwrap().as_str(), Some("from-base"));
    assert_eq!(job.get("y").unwrap().as_str(), Some("after"));
    assert_eq!(job.get("z").unwrap().as_str(), Some("from-base"));
    assert!(diags.iter().any(|d| d.code == "yaml.merge-key-clobber"));
}

#[test]
fn merge_key_sequence_earlier_elements_win() {
    let text = "\
a: &a\n  k: from-a\n  only-a: 1\nb: &b\n  k: from-b\n  only-b: 2\n\
job:\n  <<: [*a, *b]\n";
    let r = root(text);
    let job = r.get("job").unwrap().as_map().unwrap();
    assert_eq!(job.get("k").unwrap().as_str(), Some("from-a"));
    assert_eq!(job.get("only-a").unwrap().as_int(), Some(1));
    assert_eq!(job.get("only-b").unwrap().as_int(), Some(2));
}

#[test]
fn merge_key_non_map_stays_literal() {
    let r = root("job:\n  <<: plain\n");
    let job = r.get("job").unwrap().as_map().unwrap();
    assert_eq!(job.get("<<").unwrap().as_str(), Some("plain"));
}

#[test]
fn duplicate_keys_last_wins_first_position() {
    let (docs, diags) = parse(f(), "a: 1\nb: 2\na: 3\n").unwrap();
    let r = docs[0].root.as_ref().unwrap();
    let m = r.as_map().unwrap();
    assert_eq!(m.get("a").unwrap().as_int(), Some(3));
    let keys: Vec<&str> = m.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, vec!["a", "b"]);
    assert!(diags.iter().any(|d| d.code == "yaml.duplicate-key"));
}

#[test]
fn reference_tag_is_preserved() {
    let r = root("job:\n  script:\n    - !reference [.tmpl, script]\n");
    let script = r
        .get("job")
        .unwrap()
        .get("script")
        .unwrap()
        .as_seq()
        .unwrap();
    let Kind::Tagged { tag, inner } = &script[0].kind else {
        panic!("expected tagged node, got {:?}", script[0].kind);
    };
    assert_eq!(tag, "!reference");
    let parts = inner.as_seq().unwrap();
    assert_eq!(parts[0].as_str(), Some(".tmpl"));
    assert_eq!(parts[1].as_str(), Some("script"));
}

#[test]
fn core_str_tag_blocks_merge() {
    let r = root("job:\n  !!str <<: value\n");
    let job = r.get("job").unwrap().as_map().unwrap();
    assert_eq!(job.get("<<").unwrap().as_str(), Some("value"));
}

#[test]
fn psych_scalars_in_context() {
    let r = root("a: yes\nb: 017\nc: \"no\"\nd:\ne: 1:30\n");
    assert_eq!(r.get("a").unwrap().as_bool(), Some(true));
    assert_eq!(r.get("b").unwrap().as_int(), Some(15));
    assert_eq!(r.get("c").unwrap().as_str(), Some("no"));
    assert!(r.get("d").unwrap().is_null());
    assert_eq!(r.get("e").unwrap().as_int(), Some(90));
}

#[test]
fn two_documents_spec_header() {
    let text = "spec:\n  inputs:\n    env:\n---\njob:\n  script: [a]\n";
    let (docs, _) = parse(f(), text).unwrap();
    assert_eq!(docs.len(), 2);
    assert!(docs[0].root.as_ref().unwrap().get("spec").is_some());
    assert!(docs[1].root.as_ref().unwrap().get("job").is_some());
}

#[test]
fn scan_error_is_fatal() {
    assert!(parse(f(), "a: [unclosed\n").is_err());
}

#[test]
fn round_trip_through_emitter() {
    let text = r#"
stages: [a, b]
variables:
  X: yes
  Y: 017
  Z: "quoted: colon"
  MULTI: |
    line1
    line2
job:
  script:
    - echo "hi"
    - !reference [.t, script]
  rules:
    - if: $CI_COMMIT_BRANCH == "main"
      when: manual
  empty_map: {}
  empty_seq: []
  n: null
  f: 1.5
"#;
    let a = root(text);
    let emitted = emit_document(&a);
    let b = root(&emitted);
    assert!(
        semantic_eq(&a, &b),
        "round trip changed the tree:\n{emitted}"
    );
}

fn job_cache(r: &Node) -> &Node {
    r.get("cargo-check").unwrap().get("cache").unwrap()
}

#[test]
fn real_world_vidchat_file_parses() {
    let path = "/srv/projects/example/.gitlab-ci.yml";
    if let Ok(text) = std::fs::read_to_string(path) {
        let (docs, diags) = parse(f(), &text).unwrap();
        let r = docs[0].root.as_ref().unwrap();
        // `<<: *rust-cache` must give cargo-check a cache map whose span points
        // back at the `.rust-cache:` anchor definition (lines 13-20).
        let cache = job_cache(r);
        assert!(
            (13..=21).contains(&cache.span.start.line),
            "span {:?}",
            cache.span
        );
        assert!(
            !diags
                .iter()
                .any(|d| matches!(d.severity, glpv_yaml::Severity::Error))
        );
        // Round-trip the whole real file.
        let b = root(&emit_document(r));
        assert!(semantic_eq(r, &b));
    }
}

#[test]
fn value_types() {
    // Emitted strings that look like other scalar types must be quoted.
    let cases = ["yes", "017", "1:30", "null", "1,000", ".inf", "2024-01-01"];
    for c in cases {
        let node = Node::str(c, glpv_yaml::Span::point(f(), 1, 1));
        let text = format!("k: {}\n", "PLACEHOLDER");
        let _ = text;
        let emitted = emit_document(&node);
        let (docs, _) = parse(f(), &emitted).unwrap();
        let back = docs[0].root.as_ref().unwrap();
        assert_eq!(
            back.as_str(),
            Some(c),
            "string {c:?} did not survive emission: {emitted:?}"
        );
        let _ = Value::Str(String::new());
    }
}

#[test]
fn lenient_under_indented_quoted_continuation() {
    // gitlab-org/gitlab's rules.gitlab-ci.yml contains this shape; libyaml
    // accepts it, so we must too.
    let text = "\
.if-x: &a\n  if: '$A == \"b\" && $C =~\n  /^[\\d-]+-stable$/ && $D'\n\
.if-y:\n  if: 'ok'\n";
    let (docs, diags) = parse(f(), text).unwrap();
    let r = docs[0].root.as_ref().unwrap();
    let v = r
        .get(".if-x")
        .unwrap()
        .get("if")
        .unwrap()
        .as_str()
        .unwrap()
        .to_string();
    assert!(v.contains("$A == \"b\""));
    assert!(v.contains("/^[\\d-]+-stable$/"), "{v}");
    assert!(!v.contains('\n'), "folded into one line: {v}");
    assert!(diags.iter().any(|d| d.code == "yaml.lenient-quoted-indent"));
    assert_eq!(
        r.get(".if-y").unwrap().get("if").unwrap().as_str(),
        Some("ok")
    );
}
