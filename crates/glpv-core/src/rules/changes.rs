//! The pure half of `rules:changes` — matching a pattern list against a
//! changed-file list and the "is there a push event at all" rule. Shared by
//! the native evaluator and the WebAssembly build; the git side lives in
//! `crate::diff`.
//!
//! Semantics per `lib/gitlab/ci/build/rules/rule/clause/changes.rb` and
//! <https://docs.gitlab.com/ci/yaml/#ruleschanges>.

use crate::glob::changes_matcher;

/// Above this many `files × patterns` comparisons GitLab stops matching and
/// assumes the clause matches (`CHANGES_MAX_PATTERN_COMPARISONS`).
pub const MAX_PATTERN_COMPARISONS: usize = 50_000;

/// One `changes:` clause, patterns already variable-expanded.
#[derive(Clone, Copy, Debug)]
pub struct ChangesQuery<'a> {
    pub patterns: &'a [String],
    /// Expanded `compare_to` ref, when the clause has one.
    pub compare_to: Option<&'a str>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ChangesMatch {
    /// The first changed file (in diff order) matching any pattern.
    Matched(String),
    /// No changed file matched; carries the number of files examined.
    NoMatch(usize),
    /// Not computed: a blanket assumption (viewer) or GitLab's comparison
    /// cap (`true`).
    Assumed(bool),
}

impl ChangesMatch {
    pub fn as_bool(&self) -> bool {
        match self {
            ChangesMatch::Matched(_) => true,
            ChangesMatch::NoMatch(_) => false,
            ChangesMatch::Assumed(b) => *b,
        }
    }
}

/// Match `patterns` (brace expansion + Ruby `fnmatch` semantics) against
/// `files`, GitLab-style: no patterns or no files never match; more than
/// [`MAX_PATTERN_COMPARISONS`] comparisons are assumed to match.
pub fn match_changes(patterns: &[String], files: &[String]) -> ChangesMatch {
    if patterns.is_empty() || files.is_empty() {
        return ChangesMatch::NoMatch(files.len());
    }
    if patterns.len().saturating_mul(files.len()) > MAX_PATTERN_COMPARISONS {
        return ChangesMatch::Assumed(true);
    }
    let regexes = changes_matcher(patterns);
    for f in files {
        if regexes.iter().any(|re| re.is_match(f)) {
            return ChangesMatch::Matched(f.clone());
        }
    }
    ChangesMatch::NoMatch(files.len())
}

/// Whether a pipeline of this source has a changed-paths set at all. Only
/// branch pushes, merge request pipelines and external pull requests do;
/// tag pushes, schedules, web/api/trigger runs and downstream (multi-project)
/// pipelines do not — their `changes:` clauses (without `compare_to`)
/// always match.
pub fn has_push_event(source: &str, is_tag: bool) -> bool {
    !is_tag
        && matches!(
            source,
            "push" | "merge_request_event" | "external_pull_request_event"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RuleClause, RulesMode, RulesSummary, Span, When};
    use crate::rules::{EvalContext, evaluate_rules};
    use crate::vars::{VarState, VarTable};

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn matching() {
        let files = strings(&["docs/index.md", "src/main.rs"]);
        assert_eq!(
            match_changes(&strings(&["src/**/*"]), &files),
            ChangesMatch::Matched("src/main.rs".into())
        );
        assert_eq!(
            match_changes(&strings(&["*.md", "docs/*"]), &files),
            ChangesMatch::Matched("docs/index.md".into())
        );
        assert_eq!(
            match_changes(&strings(&["*.txt"]), &files),
            ChangesMatch::NoMatch(2)
        );
        assert_eq!(match_changes(&[], &files), ChangesMatch::NoMatch(2));
        assert_eq!(
            match_changes(&strings(&["*"]), &[]),
            ChangesMatch::NoMatch(0)
        );
        let many: Vec<String> = (0..50_001).map(|i| format!("f{i}")).collect();
        assert_eq!(
            match_changes(&strings(&["nomatch"]), &many),
            ChangesMatch::Assumed(true)
        );
    }

    #[test]
    fn push_events() {
        assert!(has_push_event("push", false));
        assert!(has_push_event("merge_request_event", false));
        assert!(has_push_event("external_pull_request_event", false));
        assert!(!has_push_event("push", true));
        for s in [
            "schedule",
            "web",
            "api",
            "trigger",
            "pipeline",
            "parent_pipeline",
            "chat",
        ] {
            assert!(!has_push_event(s, false), "{s}");
        }
    }

    // ---- the evaluator's changes branch ----

    fn clause(changes: &[&str], compare_to: Option<&str>, when: Option<When>) -> RuleClause {
        RuleClause {
            r#if: None,
            changes: Some(strings(changes)),
            compare_to: compare_to.map(|s| s.to_string()),
            changes_regexp: None,
            exists: None,
            when,
            allow_failure: None,
            variables: Default::default(),
            span: Span {
                file: 0,
                start: [1, 1],
                end: [1, 1],
            },
            legacy: None,
        }
    }

    fn summary(rules: Vec<RuleClause>) -> RulesSummary {
        RulesSummary {
            mode: RulesMode::Conditional,
            rules,
        }
    }

    fn run(
        summary: &RulesSummary,
        vars: &VarTable,
        source: &str,
        is_tag: bool,
        checker: Option<crate::rules::ChangesChecker<'_>>,
    ) -> crate::model::JobEvaluation {
        let ctx = EvalContext {
            vars,
            exists: None,
            changes: checker,
            source,
            ref_name: "main",
            is_tag,
            push_event: has_push_event(source, is_tag),
        };
        evaluate_rules(summary, &ctx, "t", When::OnSuccess)
    }

    fn note(e: &crate::model::JobEvaluation) -> &str {
        e.trace[0].note.as_deref().unwrap_or("")
    }

    #[test]
    fn decided_against_a_diff() {
        use crate::model::Outcome;
        let files = strings(&["docs/index.md", "src/main.rs"]);
        let checker = |q: &ChangesQuery<'_>| Some(match_changes(q.patterns, &files));
        let vars = VarTable::default();

        let e = run(
            &summary(vec![clause(&["src/**/*"], None, None)]),
            &vars,
            "push",
            false,
            Some(&checker),
        );
        assert_eq!(e.outcome, Outcome::Runs);
        assert_eq!(note(&e), "changes: matched by src/main.rs");

        let e = run(
            &summary(vec![clause(&["*.txt"], None, None)]),
            &vars,
            "push",
            false,
            Some(&checker),
        );
        assert_eq!(e.outcome, Outcome::Skipped);
        assert_eq!(note(&e), "changes: no match in 2 changed file(s)");

        // MR pipelines have a diff too.
        let e = run(
            &summary(vec![clause(&["*.txt"], None, None)]),
            &vars,
            "merge_request_event",
            false,
            Some(&checker),
        );
        assert_eq!(e.outcome, Outcome::Skipped);
    }

    #[test]
    fn no_push_event_always_matches() {
        use crate::model::Outcome;
        let never = |_: &ChangesQuery<'_>| Some(ChangesMatch::NoMatch(0));
        let vars = VarTable::default();
        let s = summary(vec![clause(&["*.txt"], None, None)]);
        for (source, tag) in [("schedule", false), ("push", true), ("pipeline", false)] {
            let e = run(&s, &vars, source, tag, Some(&never));
            assert_eq!(e.outcome, Outcome::Runs, "{source} tag={tag}");
            assert_eq!(
                note(&e),
                format!("changes: no push event for source {source}; always matches")
            );
        }
        // …even when the pattern mentions a variable nobody can expand.
        let e = run(
            &summary(vec![clause(&["$X/**/*"], None, None)]),
            &vars,
            "schedule",
            false,
            Some(&never),
        );
        assert_eq!(e.outcome, Outcome::Runs);
    }

    #[test]
    fn compare_to_bypasses_the_push_event_rule() {
        use crate::model::Outcome;
        let seen = std::cell::RefCell::new(Vec::new());
        let checker = |q: &ChangesQuery<'_>| {
            seen.borrow_mut().push(q.compare_to.map(|s| s.to_string()));
            Some(ChangesMatch::NoMatch(3))
        };
        let mut vars = VarTable::default();
        vars.set_known("CI_DEFAULT_BRANCH", "main");
        let s = summary(vec![clause(
            &["docs/**/*"],
            Some("refs/heads/$CI_DEFAULT_BRANCH"),
            None,
        )]);
        let e = run(&s, &vars, "schedule", false, Some(&checker));
        assert_eq!(e.outcome, Outcome::Skipped);
        assert_eq!(note(&e), "changes: no match in 3 changed file(s)");
        assert_eq!(
            seen.borrow().as_slice(),
            [Some("refs/heads/main".to_string())]
        );
        assert!(
            e.trace[0]
                .clause
                .contains("compare_to: refs/heads/$CI_DEFAULT_BRANCH")
        );
    }

    #[test]
    fn variables_in_patterns() {
        use crate::model::Outcome;
        let files = strings(&["src/main.rs"]);
        let checker = |q: &ChangesQuery<'_>| Some(match_changes(q.patterns, &files));
        let mut vars = VarTable::default();
        vars.set_known("SRC_DIR", "src");
        vars.set("CI_COMMIT_TAG", VarState::Unset);

        let e = run(
            &summary(vec![clause(&["$SRC_DIR/**/*"], None, None)]),
            &vars,
            "push",
            false,
            Some(&checker),
        );
        assert_eq!(e.outcome, Outcome::Runs);

        // An unset variable stays literal (and so matches nothing here).
        let e = run(
            &summary(vec![clause(&["$CI_COMMIT_TAG/**/*"], None, None)]),
            &vars,
            "push",
            false,
            Some(&checker),
        );
        assert_eq!(e.outcome, Outcome::Skipped);

        let e = run(
            &summary(vec![clause(&["$UNKNOWN_DIR/**/*", "$OTHER/x"], None, None)]),
            &vars,
            "push",
            false,
            Some(&checker),
        );
        assert_eq!(e.outcome, Outcome::Unknown);
        assert_eq!(note(&e), "changes: $UNKNOWN_DIR, $OTHER unknown");

        let e = run(
            &summary(vec![clause(&["docs/*"], Some("$UNKNOWN_REF"), None)]),
            &vars,
            "push",
            false,
            Some(&checker),
        );
        assert_eq!(e.outcome, Outcome::Unknown);
        assert_eq!(note(&e), "changes: compare_to $UNKNOWN_REF unknown");
    }

    #[test]
    fn assumed_and_undecidable() {
        use crate::model::Outcome;
        let vars = VarTable::default();
        let s = summary(vec![clause(&["src/**/*"], None, None)]);

        let yes = |_: &ChangesQuery<'_>| Some(ChangesMatch::Assumed(true));
        let e = run(&s, &vars, "push", false, Some(&yes));
        assert_eq!(e.outcome, Outcome::Runs);
        assert_eq!(note(&e), "changes: assumed match");

        let no = |_: &ChangesQuery<'_>| Some(ChangesMatch::Assumed(false));
        let e = run(&s, &vars, "push", false, Some(&no));
        assert_eq!(e.outcome, Outcome::Skipped);
        assert_eq!(note(&e), "changes: assumed no match");

        let none = |_: &ChangesQuery<'_>| None;
        let e = run(&s, &vars, "push", false, Some(&none));
        assert_eq!(e.outcome, Outcome::Unknown);
        assert_eq!(
            note(&e),
            "changes: depends on the diff; undecidable statically"
        );

        let e = run(&s, &vars, "push", false, None);
        assert_eq!(e.outcome, Outcome::Unknown);
    }

    #[test]
    fn regexp_form() {
        use crate::model::Outcome;
        let vars = VarTable::default();
        let mut c = clause(&[], None, None);
        c.changes_regexp = Some("^docs/".to_string());
        let s = summary(vec![c]);

        let some = |_: &ChangesQuery<'_>| Some(ChangesMatch::NoMatch(4));
        let e = run(&s, &vars, "push", false, Some(&some));
        assert_eq!(e.outcome, Outcome::Unknown);
        assert_eq!(note(&e), "changes:regexp is not evaluated");
        assert!(e.trace[0].clause.contains("changes: regexp(^docs/)"));

        // An empty diff decides even a regexp clause.
        let empty = |_: &ChangesQuery<'_>| Some(ChangesMatch::NoMatch(0));
        let e = run(&s, &vars, "push", false, Some(&empty));
        assert_eq!(e.outcome, Outcome::Skipped);

        let e = run(&s, &vars, "schedule", false, Some(&some));
        assert_eq!(e.outcome, Outcome::Runs);
    }

    #[test]
    fn never_clause_then_fallthrough() {
        use crate::model::Outcome;
        let files = strings(&["scripts/build.sh"]);
        let checker = |q: &ChangesQuery<'_>| Some(match_changes(q.patterns, &files));
        let vars = VarTable::default();
        let mut always = clause(&[], None, Some(When::OnSuccess));
        always.changes = None;
        let s = summary(vec![
            clause(&["scripts/*"], None, Some(When::Never)),
            always,
        ]);
        let e = run(&s, &vars, "push", false, Some(&checker));
        assert_eq!(e.outcome, Outcome::Skipped);
        assert_eq!(e.trace[0].result, "matched");
        assert_eq!(e.trace[1].result, "not_reached");
    }
}
