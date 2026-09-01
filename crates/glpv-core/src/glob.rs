//! Two glob dialects GitLab uses on repository paths.
//!
//! `glob_to_regex` is the wildcard-path translation of
//! `Repository#search_files_by_wildcard_path`: regex-escape the pattern, then
//! `\*\*/` → `(.*?/)?`, `\*\*` → `.*?`, `\*` → `([^/])*?`, anchored. Used by
//! `include:local` globs and `rules:exists`.
//!
//! `changes_glob_to_regex` (with `brace_expand`) is Ruby's `File.fnmatch?`
//! under `FNM_PATHNAME | FNM_DOTMATCH | FNM_EXTGLOB`, which is what
//! `rules:changes` patterns are matched with
//! (`lib/gitlab/ci/build/rules/rule/clause/changes.rb`). The two differ:
//! under fnmatch a `*` never crosses `/`, `**` only descends as a whole
//! `**/` segment, and braces expand.

use regex::Regex;

pub fn glob_to_regex(pattern: &str) -> Regex {
    let escaped = regex::escape(pattern);
    let translated = escaped
        .replace(r"\*\*/", "(.*?/)?")
        .replace(r"\*\*", ".*?")
        .replace(r"\*", "([^/])*?");
    Regex::new(&format!("^{translated}$")).expect("escaped glob is always a valid regex")
}

pub fn is_glob(pattern: &str) -> bool {
    pattern.contains('*')
}

/// Brace expansion as `File.fnmatch?` performs it under `FNM_EXTGLOB`: the
/// first top-level `{…}` group is split on its top-level commas and every
/// alternative is expanded recursively (so groups nest and empty
/// alternatives are allowed — `{,jh/}.gitlab-ci.yml` yields two patterns).
/// A `{` without a closing `}` expands to nothing: the pattern can never
/// match. `\x` escapes are kept verbatim for the glob translation.
pub fn brace_expand(pattern: &str) -> Vec<String> {
    let b = pattern.as_bytes();
    let mut lbrace: Option<usize> = None;
    let mut rbrace: Option<usize> = None;
    let mut nest = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' => {
                if nest == 0 {
                    lbrace = Some(i);
                }
                nest += 1;
            }
            b'}' if lbrace.is_some() => {
                nest -= 1;
                if nest == 0 {
                    rbrace = Some(i);
                    break;
                }
            }
            b'\\' => i += 1,
            _ => {}
        }
        i += 1;
    }

    let (Some(l), Some(r)) = (lbrace, rbrace) else {
        return if lbrace.is_none() {
            vec![pattern.to_string()]
        } else {
            Vec::new()
        };
    };

    let prefix = &pattern[..l];
    let suffix = &pattern[r + 1..];
    let mut out = Vec::new();
    let mut start = l + 1;
    let mut p = l + 1;
    let mut nest = 0usize;
    loop {
        if p >= r || (b[p] == b',' && nest == 0) {
            let alt = &pattern[start..p.min(r)];
            out.extend(brace_expand(&format!("{prefix}{alt}{suffix}")));
            if p >= r {
                break;
            }
            start = p + 1;
        } else {
            match b[p] {
                b'{' => nest += 1,
                b'}' => nest = nest.saturating_sub(1),
                b'\\' => p += 1,
                _ => {}
            }
        }
        p += 1;
    }
    out
}

/// One brace-free `rules:changes` pattern → an anchored regex with Ruby
/// `fnmatch` (`FNM_PATHNAME | FNM_DOTMATCH`) semantics:
///
/// - `**/` at the start of a path segment: zero or more directories
///   (hidden ones included) — `(?:[^/]*/)*`; consecutive `**/` collapse;
/// - any other run of `*`: within one segment — `[^/]*` (so `src/**` is
///   `src/*`, and `a**/b` is `a*/b`);
/// - `?`: one character that is not `/`;
/// - `[…]`: a character class that never matches `/`; `[!…]` and `[^…]`
///   negate; a reversed range matches nothing; an unterminated `[` makes the
///   whole pattern unmatchable;
/// - `\x`: the literal `x`; everything else is literal.
///
/// Paths are compared whole and repository-relative: a leading `/` in the
/// pattern is literal and never matches.
pub fn changes_glob_to_regex(pattern: &str) -> Regex {
    let mut re = String::with_capacity(pattern.len() * 2 + 2);
    re.push('^');
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    let mut segment_start = true;
    while i < chars.len() {
        let c = chars[i];
        if segment_start && chars[i..].starts_with(&['*', '*', '/']) {
            while chars[i..].starts_with(&['*', '*', '/']) {
                i += 3;
            }
            re.push_str("(?:[^/]*/)*");
            continue;
        }
        segment_start = false;
        match c {
            '*' => {
                while i < chars.len() && chars[i] == '*' {
                    i += 1;
                }
                re.push_str("[^/]*");
                continue;
            }
            '?' => re.push_str("[^/]"),
            '[' => {
                let Some((class, next)) = bracket_class(&chars, i + 1) else {
                    return never_matches();
                };
                re.push_str(&class);
                i = next;
                continue;
            }
            '\\' if i + 1 < chars.len() => {
                i += 1;
                re.push_str(&regex::escape(&chars[i].to_string()));
            }
            '/' => {
                re.push('/');
                segment_start = true;
            }
            _ => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    Regex::new(&re).expect("translated fnmatch pattern is always a valid regex")
}

/// Parse a bracket expression starting just after `[`; returns the regex
/// class text and the index after the closing `]`, or `None` when the
/// bracket is unterminated.
fn bracket_class(chars: &[char], mut i: usize) -> Option<(String, usize)> {
    let negated = matches!(chars.get(i), Some('!') | Some('^'));
    if negated {
        i += 1;
    }
    // (lo, hi) items; single characters are lo == hi.
    let mut items: Vec<(char, char)> = Vec::new();
    let take = |i: &mut usize| -> Option<char> {
        let mut c = *chars.get(*i)?;
        if c == '\\' {
            *i += 1;
            c = *chars.get(*i)?;
        }
        *i += 1;
        Some(c)
    };
    loop {
        match chars.get(i) {
            None => return None,
            Some(']') => break,
            Some(_) => {}
        }
        let lo = take(&mut i)?;
        if chars.get(i) == Some(&'-') && chars.get(i + 1).is_some_and(|c| *c != ']') {
            i += 1;
            let hi = take(&mut i)?;
            if lo <= hi {
                items.push((lo, hi));
            }
        } else {
            items.push((lo, lo));
        }
    }
    let end = i + 1;

    let mut class = String::from("[");
    if negated {
        class.push_str("^/");
    }
    for (lo, hi) in items {
        // A class never matches `/`: carve it out of positive ranges.
        let parts: Vec<(char, char)> = if !negated && lo <= '/' && '/' <= hi {
            let mut v = Vec::new();
            if lo < '/' {
                v.push((lo, '.'));
            }
            if '/' < hi {
                v.push(('0', hi));
            }
            v
        } else {
            vec![(lo, hi)]
        };
        for (lo, hi) in parts {
            class.push_str(&regex::escape(&lo.to_string()));
            if lo != hi {
                class.push('-');
                class.push_str(&regex::escape(&hi.to_string()));
            }
        }
    }
    if class == "[" {
        // Empty positive class: nothing can match it.
        return Some(("[^\\s\\S]".to_string(), end));
    }
    class.push(']');
    Some((class, end))
}

fn never_matches() -> Regex {
    Regex::new("^[^\\s\\S]$").expect("static regex")
}

/// Brace-expand and compile a `rules:changes` pattern list.
pub fn changes_matcher(patterns: &[String]) -> Vec<Regex> {
    patterns
        .iter()
        .flat_map(|p| brace_expand(p))
        .map(|p| changes_glob_to_regex(&p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pat: &str, path: &str) -> bool {
        glob_to_regex(pat).is_match(path)
    }

    #[test]
    fn single_star_stays_in_directory() {
        assert!(matches("configs/*.yml", "configs/a.yml"));
        assert!(!matches("configs/*.yml", "configs/sub/a.yml"));
        assert!(!matches("configs/*.yml", "a.yml"));
    }

    #[test]
    fn double_star_descends() {
        assert!(matches("configs/**.yml", "configs/a.yml"));
        assert!(matches("configs/**.yml", "configs/sub/a.yml"));
        assert!(matches("configs/**/*.yml", "configs/sub/deep/a.yml"));
        // `**/` is optional, so the file may sit at the base as well.
        assert!(matches("configs/**/*.yml", "configs/a.yml"));
        assert!(matches("**/*.rs", "src/main.rs"));
        assert!(matches("**/*.rs", "main.rs"));
    }

    #[test]
    fn literal_dots_are_escaped() {
        assert!(!matches("a.yml", "aXyml"));
        assert!(matches("a.yml", "a.yml"));
    }

    // ---- rules:changes (fnmatch) dialect ----

    fn fnm(pat: &str, path: &str) -> bool {
        changes_matcher(&[pat.to_string()])
            .iter()
            .any(|re| re.is_match(path))
    }

    #[test]
    fn brace_expansion() {
        assert_eq!(brace_expand("plain"), vec!["plain"]);
        assert_eq!(brace_expand("*.{rb,py,sh}"), vec!["*.rb", "*.py", "*.sh"]);
        assert_eq!(
            brace_expand("{,jh/}.gitlab-ci.yml"),
            vec![".gitlab-ci.yml", "jh/.gitlab-ci.yml"]
        );
        assert_eq!(
            brace_expand("**/*.{yml,yaml}{,.*}"),
            vec!["**/*.yml", "**/*.yml.*", "**/*.yaml", "**/*.yaml.*"]
        );
        assert_eq!(brace_expand("a{b,{c,d}}e"), vec!["abe", "ace", "ade"]);
        assert_eq!(brace_expand("a{}b"), vec!["ab"]);
        assert_eq!(brace_expand("a\\{b,c}d"), vec!["a\\{b,c}d"]);
        assert_eq!(brace_expand("a}b"), vec!["a}b"]);
        assert!(brace_expand("a{b,c").is_empty());
    }

    #[test]
    fn fnmatch_stars_stay_in_segments() {
        assert!(fnm("path/to/directory/*", "path/to/directory/file.txt"));
        assert!(!fnm(
            "path/to/directory/*",
            "path/to/directory/sub/file.txt"
        ));
        assert!(fnm("path/to/directory/**/*", "path/to/directory/file.txt"));
        assert!(fnm(
            "path/to/directory/**/*",
            "path/to/directory/a/b/file.txt"
        ));
        assert!(fnm("*.md", "README.md"));
        assert!(!fnm("*.md", "docs/README.md"));
        assert!(fnm("**/*.md", "docs/README.md"));
        assert!(fnm("**/*.md", "README.md"));
        assert!(fnm("src/**", "src/main.rs"));
        assert!(!fnm("src/**", "src/a/b"));
        assert!(fnm("a**/b", "ax/b"));
        assert!(!fnm("a**/b", "a/x/b"));
        assert!(fnm("a*/b", "ax/b"));
        assert!(fnm("**", "file"));
        assert!(!fnm("**", "dir/file"));
        assert!(fnm("src/**/**/*.rs", "src/x.rs"));
        assert!(fnm("src/**/**/*.rs", "src/a/b/x.rs"));
    }

    #[test]
    fn fnmatch_hidden_and_anchoring() {
        assert!(fnm("**/config", ".hidden/config"));
        assert!(fnm("**/config", "config"));
        assert!(fnm("*", ".dotfile"));
        assert!(!fnm("/src/x", "src/x"));
        assert!(!fnm("src", "src/x"));
        assert!(!fnm("src/x", "a/src/x"));
    }

    #[test]
    fn fnmatch_question_mark_and_classes() {
        assert!(fnm("src/main.??", "src/main.rs"));
        assert!(!fnm("src/main.??", "src/main.rst"));
        assert!(!fnm("src?main.rs", "src/main.rs"));
        assert!(fnm("scripts/[a-c]*.sh", "scripts/build.sh"));
        assert!(!fnm("scripts/[a-c]*.sh", "scripts/deploy.sh"));
        assert!(fnm("[!a]x", "bx"));
        assert!(!fnm("[!a]x", "ax"));
        assert!(fnm("[^a]x", "bx"));
        assert!(!fnm("[^a]x", "/x"));
        assert!(!fnm("a[/]b", "a/b"));
        assert!(!fnm("a[+-9]b", "a/b"));
        assert!(fnm("a[+-9]b", "a5b"));
        assert!(fnm("[a-]", "-"));
        assert!(fnm("[a-]", "a"));
        assert!(!fnm("[z-a]", "m"));
        assert!(!fnm("[abc", "a"));
        assert!(!fnm("[abc", "[abc"));
        assert!(fnm("[\\]]", "]"));
    }

    #[test]
    fn fnmatch_escapes_and_literals() {
        assert!(fnm("a\\*b", "a*b"));
        assert!(!fnm("a\\*b", "axb"));
        assert!(fnm("a.b", "a.b"));
        assert!(!fnm("a.b", "axb"));
        assert!(fnm("dir/(x)+$", "dir/(x)+$"));
        assert!(fnm("*.{rb,py,sh}", "x.py"));
        assert!(!fnm("*.{rb,py,sh}", "x.js"));
        assert!(!fnm("*.{rb,py", "x.rb"));
        assert!(fnm("{,jh/}.gitlab-ci.yml", "jh/.gitlab-ci.yml"));
        assert!(fnm("**/*.{yml,yaml}{,.*}", "a/b/c.yaml.erb"));
    }
}
