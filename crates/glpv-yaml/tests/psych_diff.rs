//! Differential test of our Psych scalar typing against PyYAML's SafeLoader
//! (both implement YAML 1.1, so they agree except where Psych deliberately
//! diverges — those cases are allowlisted with the expected Psych result).
//!
//! Skips silently when python3 or PyYAML is unavailable.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use glpv_yaml::{Value, resolve_plain};

const PY: &str = r#"
import sys, yaml, datetime
for line in sys.stdin.read().split("\n"):
    if line == "":
        continue
    try:
        v = yaml.load(line, Loader=yaml.SafeLoader)
    except Exception:
        print("error"); continue
    if v is None: print("null")
    elif isinstance(v, bool): print("bool")
    elif isinstance(v, int): print("int:%d" % v)
    elif isinstance(v, float): print("float")
    elif isinstance(v, str): print("str")
    elif isinstance(v, (datetime.date, datetime.datetime)): print("date")
    else: print("other")
"#;

fn our_class(raw: &str) -> String {
    match resolve_plain(raw).0 {
        Value::Null => "null".into(),
        Value::Bool(_) => "bool".into(),
        Value::Int(i) => format!("int:{i}"),
        Value::Float(_) => "float".into(),
        Value::Str(_) => "str".into(),
    }
}

/// line → (expected ours, expected PyYAML) where the two loaders legitimately differ.
fn allowlist() -> HashMap<&'static str, (&'static str, &'static str)> {
    HashMap::from([
        // Psych booleans/null are fully case-insensitive; PyYAML only accepts
        // lower/Title/UPPER forms.
        ("NuLL", ("null", "str")),
        ("yES", ("bool", "str")),
        ("nO", ("bool", "str")),
        ("TrUe", ("bool", "str")),
        ("oN", ("bool", "str")),
        ("oFf", ("bool", "str")),
        // Psych accepts commas as digit separators.
        ("1,000", ("int:1000", "str")),
        // Psych's sexagesimal regex allows a leading 0; PyYAML requires [1-9].
        ("0:30", ("int:30", "str")),
        // Psych's `.inf` check is case-insensitive beyond the three spec forms.
        (".iNf", ("float", "str")),
        // Unquoted dates: Psych builds Date/Time (GitLab then *rejects* them);
        // we type them as strings and raise an error diagnostic instead.
        ("2024-01-01", ("str", "date")),
        ("2024-01-01 10:00:00", ("str", "date")),
        // Ruby bignum overflow: we keep the text as a string.
        ("99999999999999999999", ("str", "int:99999999999999999999")),
        // Not scalar-typing differences: PyYAML cannot parse these as a bare
        // document (`*` starts an alias; `=` maps to the special `value` tag
        // that SafeConstructor refuses). Our function is unit-level.
        ("**/*.rs", ("str", "error")),
        ("=", ("str", "error")),
    ])
}

#[test]
fn scalar_typing_matches_pyyaml() {
    let corpus = include_str!("scalars.txt");
    let lines: Vec<&str> = corpus.lines().filter(|l| !l.is_empty()).collect();

    let child = Command::new("python3")
        .args(["-c", PY])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        eprintln!("python3 not available; skipping differential test");
        return;
    };
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(lines.join("\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    if !out.status.success() {
        eprintln!("PyYAML not available; skipping differential test");
        return;
    }
    let py_results: Vec<&str> = std::str::from_utf8(&out.stdout).unwrap().lines().collect();
    assert_eq!(py_results.len(), lines.len(), "python output out of sync");

    let allow = allowlist();
    let mut failures = Vec::new();
    for (line, py) in lines.iter().zip(&py_results) {
        let ours = our_class(line);
        if let Some((want_ours, want_py)) = allow.get(line) {
            if ours != *want_ours || py != want_py {
                failures.push(format!(
                    "{line:?}: allowlisted as ours={want_ours} py={want_py}, got ours={ours} py={py}"
                ));
            }
            continue;
        }
        if ours != *py {
            failures.push(format!("{line:?}: ours={ours} pyyaml={py}"));
        }
    }
    assert!(failures.is_empty(), "divergences:\n{}", failures.join("\n"));
}
