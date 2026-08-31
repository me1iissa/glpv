//! GitLab's wildcard-path semantics (`Repository#search_files_by_wildcard_path`):
//! regex-escape the pattern, then `\*\*/` → `(.*?/)?`, `\*\*` → `.*?`,
//! `\*` → `([^/])*?`, anchored. Used by `include:local` globs and `rules:exists`.

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
}
