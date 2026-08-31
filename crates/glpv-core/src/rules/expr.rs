//! `rules:if` expression evaluation, faithful to GitLab's implementation
//! (`lib/gitlab/ci/pipeline/expression/`): shunting-yard over the lexeme set
//! `( ) $VAR "str" 'str' /re/ismU null true false == != =~ !~ && || !`,
//! Ruby value semantics (`&&`/`||` return operands; only nil/false are falsy
//! mid-expression), and a final Rails `present?` (blank strings are falsy).
//!
//! Three-valued: variables a static crawl cannot see evaluate to Unknown,
//! which propagates instead of being guessed.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use regex::Regex;

use crate::vars::{VarState, VarTable};

const MAX_TOKENS: usize = 100;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tri {
    True,
    False,
    Unknown,
}

#[derive(Clone, Debug)]
enum Val {
    Nil,
    Bool(bool),
    Str(String),
    Re(String), // /pattern/flags, kept textual until matched
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    LParen,
    RParen,
    Var(String),
    Str(String),
    Pattern(String),
    Null,
    Bool(bool),
    Eq,
    Ne,
    Match,
    NotMatch,
    And,
    Or,
    Not,
}

pub struct EvalResult {
    pub result: Tri,
    /// Variables read during evaluation with their states.
    pub vars_used: Vec<(String, VarState)>,
    /// Human-readable notes (syntax errors, invalid regexes, …).
    pub notes: Vec<String>,
}

pub fn eval_if(expr: &str, vars: &VarTable) -> EvalResult {
    let mut notes = Vec::new();
    let mut vars_used = Vec::new();

    let tokens = match lex(expr) {
        Ok(t) => t,
        Err(e) => {
            notes.push(format!(
                "expression does not parse ({e}); GitLab evaluates it as false"
            ));
            return EvalResult {
                result: Tri::False,
                vars_used,
                notes,
            };
        }
    };
    let rpn = match to_rpn(&tokens) {
        Ok(r) => r,
        Err(e) => {
            notes.push(format!(
                "expression does not parse ({e}); GitLab evaluates it as false"
            ));
            return EvalResult {
                result: Tri::False,
                vars_used,
                notes,
            };
        }
    };

    let mut stack: Vec<Val> = Vec::new();
    for tok in rpn {
        match tok {
            Tok::Var(name) => {
                let state = vars.get(&name);
                vars_used.push((name, state.clone()));
                stack.push(match state {
                    VarState::Known(v) => Val::Str(v),
                    VarState::Unset => Val::Nil,
                    VarState::Unknown => Val::Unknown,
                });
            }
            Tok::Str(s) => stack.push(Val::Str(s)),
            Tok::Pattern(p) => stack.push(Val::Re(p)),
            Tok::Null => stack.push(Val::Nil),
            Tok::Bool(b) => stack.push(Val::Bool(b)),
            Tok::Not => {
                let v = stack.pop().unwrap_or(Val::Nil);
                stack.push(match ruby_truthy(&v) {
                    Tri::True => Val::Bool(false),
                    Tri::False => Val::Bool(true),
                    Tri::Unknown => Val::Unknown,
                });
            }
            Tok::Eq | Tok::Ne => {
                let rhs = stack.pop().unwrap_or(Val::Nil);
                let lhs = stack.pop().unwrap_or(Val::Nil);
                let eq = equals(&lhs, &rhs);
                let v = if tok == Tok::Ne { not_tri(eq) } else { eq };
                stack.push(tri_to_val(v));
            }
            Tok::Match | Tok::NotMatch => {
                let rhs = stack.pop().unwrap_or(Val::Nil);
                let lhs = stack.pop().unwrap_or(Val::Nil);
                let m = matches_re(&lhs, &rhs, &mut notes);
                let v = if tok == Tok::NotMatch { not_tri(m) } else { m };
                stack.push(tri_to_val(v));
            }
            Tok::And => {
                let rhs = stack.pop().unwrap_or(Val::Nil);
                let lhs = stack.pop().unwrap_or(Val::Nil);
                stack.push(match ruby_truthy(&lhs) {
                    Tri::False => lhs,
                    Tri::True => rhs,
                    Tri::Unknown => Val::Unknown,
                });
            }
            Tok::Or => {
                let rhs = stack.pop().unwrap_or(Val::Nil);
                let lhs = stack.pop().unwrap_or(Val::Nil);
                stack.push(match ruby_truthy(&lhs) {
                    Tri::True => lhs,
                    Tri::False => rhs,
                    Tri::Unknown => Val::Unknown,
                });
            }
            Tok::LParen | Tok::RParen => unreachable!("removed by shunting-yard"),
        }
    }

    let result = match stack.pop() {
        Some(v) => present(&v),
        None => {
            notes.push("empty expression".to_string());
            Tri::False
        }
    };
    EvalResult {
        result,
        vars_used,
        notes,
    }
}

fn tri_to_val(t: Tri) -> Val {
    match t {
        Tri::True => Val::Bool(true),
        Tri::False => Val::Bool(false),
        Tri::Unknown => Val::Unknown,
    }
}

fn not_tri(t: Tri) -> Tri {
    match t {
        Tri::True => Tri::False,
        Tri::False => Tri::True,
        Tri::Unknown => Tri::Unknown,
    }
}

/// Ruby truthiness: only nil and false are falsy ("" is truthy here!).
fn ruby_truthy(v: &Val) -> Tri {
    match v {
        Val::Nil | Val::Bool(false) => Tri::False,
        Val::Unknown => Tri::Unknown,
        _ => Tri::True,
    }
}

/// Rails `present?`, applied to the final statement value: blank strings are falsy.
fn present(v: &Val) -> Tri {
    match v {
        Val::Nil | Val::Bool(false) => Tri::False,
        Val::Bool(true) | Val::Re(_) => Tri::True,
        Val::Str(s) => {
            if s.chars().any(|c| !c.is_whitespace()) {
                Tri::True
            } else {
                Tri::False
            }
        }
        Val::Unknown => Tri::Unknown,
    }
}

fn equals(lhs: &Val, rhs: &Val) -> Tri {
    match (lhs, rhs) {
        (Val::Unknown, _) | (_, Val::Unknown) => Tri::Unknown,
        (Val::Str(a), Val::Str(b)) => (a == b).into(),
        (Val::Nil, Val::Nil) => Tri::True,
        (Val::Bool(a), Val::Bool(b)) => (a == b).into(),
        // Cross-type Ruby `==` is false (a string never equals nil or true).
        _ => Tri::False,
    }
}

impl From<bool> for Tri {
    fn from(b: bool) -> Tri {
        if b { Tri::True } else { Tri::False }
    }
}

static RE_CACHE: LazyLock<Mutex<HashMap<String, Option<Regex>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn compile_pattern(text: &str) -> Option<Regex> {
    let mut cache = RE_CACHE.lock().unwrap();
    if let Some(r) = cache.get(text) {
        return r.clone();
    }
    let compiled = (|| {
        let body_flags = text.strip_prefix('/')?;
        let slash = body_flags.rfind('/')?;
        let (body, flags) = body_flags.split_at(slash);
        let flags = &flags[1..];
        let source = if flags.is_empty() {
            body.replace("\\/", "/")
        } else {
            format!("(?{flags}){}", body.replace("\\/", "/"))
        };
        Regex::new(&source).ok()
    })();
    cache.insert(text.to_string(), compiled.clone());
    compiled
}

fn matches_re(lhs: &Val, rhs: &Val, notes: &mut Vec<String>) -> Tri {
    // The right side must be a regex literal, or a variable whose value has
    // /…/flags form (GitLab compiles those too).
    let pattern = match rhs {
        Val::Re(p) => p.clone(),
        Val::Str(s) if s.starts_with('/') && s.len() > 1 => s.clone(),
        Val::Unknown => return Tri::Unknown,
        _ => {
            notes.push(
                "the right side of =~ is not a /regex/; GitLab treats the match as false"
                    .to_string(),
            );
            return Tri::False;
        }
    };
    let Some(re) = compile_pattern(&pattern) else {
        notes.push(format!(
            "invalid regex {pattern}; GitLab evaluates the rule as false"
        ));
        return Tri::False;
    };
    let text = match lhs {
        Val::Nil => String::new(), // Ruby nil.to_s
        Val::Str(s) => s.clone(),
        Val::Unknown => return Tri::Unknown,
        Val::Bool(_) | Val::Re(_) => {
            notes.push("the left side of =~ is not a string".to_string());
            return Tri::False;
        }
    };
    re.is_match(&text).into()
}

fn lex(expr: &str) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let bytes = expr.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if out.len() > MAX_TOKENS {
            return Err(format!("more than {MAX_TOKENS} tokens"));
        }
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b'$' => {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                if end == start {
                    return Err("`$` with no variable name".to_string());
                }
                out.push(Tok::Var(expr[start..end].to_string()));
                i = end;
            }
            b'"' | b'\'' => {
                // Non-greedy, no escape processing — exactly GitLab's lexer.
                let quote = c;
                let start = i + 1;
                let end = bytes[start..]
                    .iter()
                    .position(|b| *b == quote)
                    .ok_or("unterminated string")?;
                out.push(Tok::Str(expr[start..start + end].to_string()));
                i = start + end + 1;
            }
            b'/' => {
                // /([^/]|\/)+[^\]/ + flags [ismU]*
                let start = i;
                let mut j = i + 1;
                let mut prev_backslash = false;
                let mut closed = None;
                while j < bytes.len() {
                    if bytes[j] == b'/' && !prev_backslash {
                        closed = Some(j);
                        break;
                    }
                    prev_backslash = bytes[j] == b'\\' && !prev_backslash;
                    j += 1;
                }
                let close = closed.ok_or("unterminated regex")?;
                let mut end = close + 1;
                while end < bytes.len() && matches!(bytes[end], b'i' | b's' | b'm' | b'U') {
                    end += 1;
                }
                out.push(Tok::Pattern(expr[start..end].to_string()));
                i = end;
            }
            b'=' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    out.push(Tok::Eq);
                    i += 2;
                } else if bytes.get(i + 1) == Some(&b'~') {
                    out.push(Tok::Match);
                    i += 2;
                } else {
                    return Err("stray `=`".to_string());
                }
            }
            b'!' => match bytes.get(i + 1) {
                Some(b'=') => {
                    out.push(Tok::Ne);
                    i += 2;
                }
                Some(b'~') => {
                    out.push(Tok::NotMatch);
                    i += 2;
                }
                _ => {
                    out.push(Tok::Not);
                    i += 1;
                }
            },
            b'&' => {
                if bytes.get(i + 1) == Some(&b'&') {
                    out.push(Tok::And);
                    i += 2;
                } else {
                    return Err("stray `&`".to_string());
                }
            }
            b'|' => {
                if bytes.get(i + 1) == Some(&b'|') {
                    out.push(Tok::Or);
                    i += 2;
                } else {
                    return Err("stray `|`".to_string());
                }
            }
            _ => {
                // null / true / false keywords
                let rest = &expr[i..];
                if rest.starts_with("null") {
                    out.push(Tok::Null);
                    i += 4;
                } else if rest.starts_with("true") {
                    out.push(Tok::Bool(true));
                    i += 4;
                } else if rest.starts_with("false") {
                    out.push(Tok::Bool(false));
                    i += 5;
                } else {
                    return Err(format!("unexpected character `{}`", &expr[i..i + 1]));
                }
            }
        }
    }
    Ok(out)
}

fn precedence(t: &Tok) -> Option<u8> {
    // Lower binds tighter (GitLab's own numbering).
    match t {
        Tok::Not => Some(1),
        Tok::Eq | Tok::Ne | Tok::Match | Tok::NotMatch => Some(10),
        Tok::And => Some(11),
        Tok::Or => Some(12),
        _ => None,
    }
}

fn to_rpn(tokens: &[Tok]) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let mut ops: Vec<Tok> = Vec::new();
    for t in tokens {
        match t {
            Tok::Var(_) | Tok::Str(_) | Tok::Pattern(_) | Tok::Null | Tok::Bool(_) => {
                out.push(t.clone())
            }
            Tok::LParen => ops.push(t.clone()),
            Tok::RParen => loop {
                match ops.pop() {
                    Some(Tok::LParen) => break,
                    Some(op) => out.push(op),
                    None => return Err("unbalanced parentheses".to_string()),
                }
            },
            op => {
                let p = precedence(op).unwrap();
                while let Some(top) = ops.last() {
                    match precedence(top) {
                        Some(tp) if tp <= p && *top != Tok::Not => out.push(ops.pop().unwrap()),
                        // Unary ! is right-associative; pop only tighter ops.
                        Some(tp) if tp < p => out.push(ops.pop().unwrap()),
                        _ => break,
                    }
                }
                ops.push(op.clone());
            }
        }
    }
    while let Some(op) = ops.pop() {
        if op == Tok::LParen {
            return Err("unbalanced parentheses".to_string());
        }
        out.push(op);
    }
    if out.is_empty() {
        return Err("empty expression".to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> VarTable {
        let mut t = VarTable::default();
        t.set_known("CI_COMMIT_BRANCH", "main");
        t.set_known("CI_DEFAULT_BRANCH", "main");
        t.set_known("CI_PIPELINE_SOURCE", "push");
        t.set_known("EMPTY", "");
        t.set("CI_COMMIT_TAG", VarState::Unset);
        t.set("MYSTERY", VarState::Unknown);
        t
    }

    fn ev(expr: &str) -> Tri {
        eval_if(expr, &table()).result
    }

    #[test]
    fn comparisons() {
        assert_eq!(ev(r#"$CI_COMMIT_BRANCH == "main""#), Tri::True);
        assert_eq!(ev(r#"$CI_COMMIT_BRANCH != "main""#), Tri::False);
        assert_eq!(ev(r#"$CI_COMMIT_BRANCH == $CI_DEFAULT_BRANCH"#), Tri::True);
        assert_eq!(ev(r#"$CI_COMMIT_TAG == null"#), Tri::True);
        assert_eq!(ev(r#"$CI_COMMIT_BRANCH == null"#), Tri::False);
        // Comparing a string variable to a boolean literal is always false.
        assert_eq!(ev(r#"$CI_COMMIT_BRANCH == true"#), Tri::False);
    }

    #[test]
    fn presence() {
        assert_eq!(ev("$CI_COMMIT_BRANCH"), Tri::True);
        assert_eq!(ev("$CI_COMMIT_TAG"), Tri::False, "unset variable");
        assert_eq!(ev("$EMPTY"), Tri::False, "blank string fails present?");
        assert_eq!(ev("$MYSTERY"), Tri::Unknown);
    }

    #[test]
    fn boolean_operators_ruby_semantics() {
        assert_eq!(
            ev(r#"$CI_COMMIT_BRANCH && $CI_PIPELINE_SOURCE == "push""#),
            Tri::True
        );
        assert_eq!(ev(r#"$CI_COMMIT_TAG && $CI_COMMIT_BRANCH"#), Tri::False);
        assert_eq!(ev(r#"$CI_COMMIT_TAG || $CI_COMMIT_BRANCH"#), Tri::True);
        // "" is truthy for ||, but the returned "" then fails present?.
        assert_eq!(ev(r#"$EMPTY || $CI_COMMIT_BRANCH"#), Tri::False);
        assert_eq!(ev(r#"$MYSTERY || $CI_COMMIT_BRANCH"#), Tri::Unknown);
        assert_eq!(ev(r#"!$CI_COMMIT_TAG"#), Tri::True);
        assert_eq!(
            ev(r#"!$EMPTY"#),
            Tri::False,
            "! uses Ruby truthiness, not present?"
        );
    }

    #[test]
    fn parentheses_and_precedence() {
        assert_eq!(
            ev(
                r#"($CI_COMMIT_TAG || $CI_COMMIT_BRANCH == "main") && $CI_PIPELINE_SOURCE == "push""#
            ),
            Tri::True
        );
        // && binds tighter than ||.
        assert_eq!(
            ev(r#"$CI_COMMIT_TAG && $MYSTERY || $CI_COMMIT_BRANCH == "main""#),
            Tri::True
        );
    }

    #[test]
    fn regex_matching() {
        assert_eq!(ev(r#"$CI_COMMIT_BRANCH =~ /^ma/"#), Tri::True);
        assert_eq!(ev(r#"$CI_COMMIT_BRANCH =~ /^MA/i"#), Tri::True);
        assert_eq!(ev(r#"$CI_COMMIT_BRANCH !~ /release/"#), Tri::True);
        // nil coerces to "" on the left.
        assert_eq!(ev(r#"$CI_COMMIT_TAG =~ /x/"#), Tri::False);
        assert_eq!(ev(r#"$CI_COMMIT_TAG =~ /^$/"#), Tri::True);
        // Non-regex right side → false with a note.
        let r = eval_if(r#"$CI_COMMIT_BRANCH =~ "main""#, &table());
        assert_eq!(r.result, Tri::False);
        assert!(!r.notes.is_empty());
        assert_eq!(ev(r#"$MYSTERY =~ /x/"#), Tri::Unknown);
    }

    #[test]
    fn syntax_errors_are_false() {
        assert_eq!(ev("$CI_COMMIT_BRANCH =="), Tri::False);
        assert_eq!(ev("((("), Tri::False);
        assert_eq!(ev("bogus tokens"), Tri::False);
        assert_eq!(ev(""), Tri::False);
    }

    #[test]
    fn vars_used_reported() {
        let r = eval_if(r#"$CI_COMMIT_TAG || $MYSTERY"#, &table());
        assert_eq!(r.vars_used.len(), 2);
        assert_eq!(r.vars_used[0].0, "CI_COMMIT_TAG");
    }
}
