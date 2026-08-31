//! Plain-scalar typing that mirrors Ruby Psych's `ScalarScanner#tokenize`
//! (Psych 5, non-strict integers) — the semantics GitLab actually runs.
//!
//! Notable divergences from YAML 1.2 (and from PyYAML in places): booleans are
//! case-insensitive (`yES` is true), integers accept `_` and `,` separators,
//! leading-zero integers are octal, `12:30` is sexagesimal, and unquoted
//! dates/times are *rejected* by GitLab's `YAML.safe_load` call.

use std::sync::LazyLock;

use regex::Regex;

use crate::Value;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PsychNote {
    /// `:sym` — a Ruby Symbol under Psych; we keep the raw text as a string.
    SymbolScalar,
    /// Unquoted date/time — Psych would build a Date/Time, which
    /// `YAML.safe_load` (as used by GitLab) rejects with `DisallowedClass`.
    DisallowedDate,
    /// Integer that does not fit in i64 (Ruby would use a bignum).
    IntOverflow,
}

// Psych's "obviously a string" fast path. Unanchored at the end on purpose:
// only a matching *prefix* is required. `[[:alpha:]]` in Ruby is any letter.
static OBVIOUS_STR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^[^\d.:\-]?[\p{Alphabetic}_\s!@#$%\^&*(){}<>|/\\~;=]+"#).unwrap()
});

static BOOL_TRUE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?i:yes|true|on)$").unwrap());
static BOOL_FALSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?i:no|false|off)$").unwrap());
static NULL_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?i:null)$").unwrap());

// http://yaml.org/type/timestamp.html, as transcribed in Psych.
static TIME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^-?\d{4}-\d{1,2}-\d{1,2}(?:[Tt]|\s+)\d{1,2}:\d\d:\d\d(?:\.\d*)?(?:\s*(?:Z|[-+]\d{1,2}:?(?:\d\d)?))?$",
    )
    .unwrap()
});
static DATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{4}-\d{1,2}-\d{1,2}$").unwrap());

static INF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[-+]?\.(?i:inf)$").unwrap());
static NAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\.(?i:nan)$").unwrap());

static SEXAGESIMAL_INT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-+]?[0-9][0-9_]*(:[0-5]?[0-9]){1,2}$").unwrap());
static SEXAGESIMAL_FLOAT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-+]?[0-9][0-9_]*(:[0-5]?[0-9]){1,2}\.[0-9_]*$").unwrap());

static FLOAT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-+]?([0-9][0-9_,]*)?\.[0-9]*([eE][-+][0-9]+)?$").unwrap());
static BARE_DOT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[-+]?\.$").unwrap());

static INTEGER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:[-+]?0b[0-1_,]+|[-+]?0[0-7_,]+|[-+]?(?:0|[1-9](?:[0-9]|,[0-9]|_[0-9])*)|[-+]?0x[0-9a-fA-F_,]+)$",
    )
    .unwrap()
});

/// Type a plain (unquoted, untagged) scalar exactly as Psych would.
pub fn resolve_plain(raw: &str) -> (Value, Option<PsychNote>) {
    if raw.is_empty() {
        return (Value::Null, None);
    }

    if OBVIOUS_STR.is_match(raw) || raw.contains('\n') {
        if raw.len() > 5 {
            return (Value::Str(raw.to_string()), None);
        }
        let first = raw.chars().next().unwrap();
        if !matches!(
            first,
            'y' | 'Y' | 't' | 'T' | 'o' | 'O' | 'n' | 'N' | 'f' | 'F' | '~'
        ) {
            return (Value::Str(raw.to_string()), None);
        }
        if raw == "~" || NULL_WORD.is_match(raw) {
            return (Value::Null, None);
        }
        if BOOL_TRUE.is_match(raw) {
            return (Value::Bool(true), None);
        }
        if BOOL_FALSE.is_match(raw) {
            return (Value::Bool(false), None);
        }
        return (Value::Str(raw.to_string()), None);
    }

    if TIME.is_match(raw) || DATE.is_match(raw) {
        return (Value::Str(raw.to_string()), Some(PsychNote::DisallowedDate));
    }
    if INF.is_match(raw) {
        let v = if raw.starts_with('-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
        return (Value::Float(v), None);
    }
    if NAN.is_match(raw) {
        return (Value::Float(f64::NAN), None);
    }
    if raw.len() >= 2 && raw.starts_with(':') {
        return (Value::Str(raw.to_string()), Some(PsychNote::SymbolScalar));
    }
    if SEXAGESIMAL_INT.is_match(raw) {
        return match parse_sexagesimal_int(raw) {
            Some(i) => (Value::Int(i), None),
            None => (Value::Str(raw.to_string()), Some(PsychNote::IntOverflow)),
        };
    }
    if SEXAGESIMAL_FLOAT.is_match(raw) {
        return (Value::Float(parse_sexagesimal_float(raw)), None);
    }
    if FLOAT.is_match(raw) {
        if BARE_DOT.is_match(raw) {
            return (Value::Str(raw.to_string()), None);
        }
        return match parse_float(raw) {
            Some(f) => (Value::Float(f), None),
            None => (Value::Str(raw.to_string()), None),
        };
    }
    if INTEGER.is_match(raw) {
        return match parse_int(raw) {
            Some(i) => (Value::Int(i), None),
            None => (Value::Str(raw.to_string()), Some(PsychNote::IntOverflow)),
        };
    }

    (Value::Str(raw.to_string()), None)
}

/// Boolean per Psych's case-insensitive YAML 1.1 set (used for `!!bool`).
pub(crate) fn parse_bool(raw: &str) -> Option<bool> {
    if BOOL_TRUE.is_match(raw) {
        Some(true)
    } else if BOOL_FALSE.is_match(raw) {
        Some(false)
    } else {
        None
    }
}

/// Ruby `Integer(string.gsub(/[,_]/, ''))`: 0b binary, leading-0 octal, 0x hex.
pub(crate) fn parse_int(raw: &str) -> Option<i64> {
    let cleaned: String = raw.chars().filter(|c| *c != ',' && *c != '_').collect();
    let (neg, body) = match cleaned.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, cleaned.strip_prefix('+').unwrap_or(&cleaned)),
    };
    let magnitude = if let Some(b) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
        u64::from_str_radix(b, 2).ok()?
    } else if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).ok()?
    } else if body.len() > 1 && body.starts_with('0') {
        u64::from_str_radix(&body[1..], 8).ok()?
    } else {
        body.parse::<u64>().ok()?
    };
    if neg {
        if magnitude > i64::MAX as u64 + 1 {
            None
        } else if magnitude == i64::MAX as u64 + 1 {
            Some(i64::MIN)
        } else {
            Some(-(magnitude as i64))
        }
    } else {
        i64::try_from(magnitude).ok()
    }
}

/// Psych: `Float(string.gsub(/[,_]|\.([Ee]|$)/, '\1'))`.
pub(crate) fn parse_float(raw: &str) -> Option<f64> {
    let mut cleaned: String = raw.chars().filter(|c| *c != ',' && *c != '_').collect();
    if let Some(stripped) = cleaned.strip_suffix('.') {
        cleaned = stripped.to_string();
    }
    cleaned = cleaned.replace(".e", "e").replace(".E", "E");
    cleaned.parse::<f64>().ok()
}

fn parse_sexagesimal_int(raw: &str) -> Option<i64> {
    let (neg, body) = sign_split(raw);
    let mut total: i64 = 0;
    for part in body.split(':') {
        let digits: String = part.chars().filter(|c| *c != '_').collect();
        let n: i64 = digits.parse().ok()?;
        total = total.checked_mul(60)?.checked_add(n)?;
    }
    Some(if neg { -total } else { total })
}

fn parse_sexagesimal_float(raw: &str) -> f64 {
    let (neg, body) = sign_split(raw);
    let mut total = 0.0f64;
    for part in body.split(':') {
        let digits: String = part.chars().filter(|c| *c != '_').collect();
        total = total * 60.0 + digits.parse::<f64>().unwrap_or(0.0);
    }
    if neg { -total } else { total }
}

fn sign_split(raw: &str) -> (bool, &str) {
    match raw.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, raw.strip_prefix('+').unwrap_or(raw)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Value {
        resolve_plain(s).0
    }

    #[test]
    fn nulls() {
        assert_eq!(v(""), Value::Null);
        assert_eq!(v("~"), Value::Null);
        assert_eq!(v("null"), Value::Null);
        assert_eq!(v("Null"), Value::Null);
        assert_eq!(v("NULL"), Value::Null);
        assert_eq!(v("NuLL"), Value::Null); // Psych's /^null$/i, unlike YAML 1.1's tri-form
    }

    #[test]
    fn booleans_case_insensitive() {
        assert_eq!(v("yes"), Value::Bool(true));
        assert_eq!(v("yES"), Value::Bool(true)); // Psych divergence from the YAML 1.1 spec
        assert_eq!(v("on"), Value::Bool(true));
        assert_eq!(v("True"), Value::Bool(true));
        assert_eq!(v("no"), Value::Bool(false));
        assert_eq!(v("Off"), Value::Bool(false));
        assert_eq!(v("FALSE"), Value::Bool(false));
        // Not booleans:
        assert_eq!(v("yes2"), Value::Str("yes2".into()));
        assert_eq!(v("on-call"), Value::Str("on-call".into()));
        assert_eq!(v("true-ish"), Value::Str("true-ish".into()));
        assert_eq!(v("y"), Value::Str("y".into())); // y/n are NOT booleans in Psych
        assert_eq!(v("n"), Value::Str("n".into()));
    }

    #[test]
    fn integers() {
        assert_eq!(v("0"), Value::Int(0));
        assert_eq!(v("42"), Value::Int(42));
        assert_eq!(v("-7"), Value::Int(-7));
        assert_eq!(v("+7"), Value::Int(7));
        assert_eq!(v("1_000"), Value::Int(1000));
        assert_eq!(v("1,000"), Value::Int(1000)); // Psych accepts commas; PyYAML does not
        assert_eq!(v("017"), Value::Int(15)); // leading zero = octal
        assert_eq!(v("0x1A"), Value::Int(26));
        assert_eq!(v("0b101"), Value::Int(5));
        assert_eq!(v("08"), Value::Str("08".into())); // not octal, not decimal → string
        assert_eq!(v("0o17"), Value::Str("0o17".into())); // YAML 1.1 has no 0o form
        assert_eq!(v("03334444456"), Value::Int(0o3334444456));
    }

    #[test]
    fn floats() {
        assert_eq!(v(".5"), Value::Float(0.5));
        assert_eq!(v("1."), Value::Float(1.0));
        assert_eq!(v("1.5"), Value::Float(1.5));
        assert_eq!(v("-1.5"), Value::Float(-1.5));
        assert_eq!(v("1.e+3"), Value::Float(1000.0));
        assert_eq!(v("1.5e+3"), Value::Float(1500.0));
        assert_eq!(v("."), Value::Str(".".into()));
        assert_eq!(v("-."), Value::Str("-.".into()));
        // No exponent sign → not FLOAT per Psych's regex (requires [-+]):
        assert_eq!(v("1.5e3"), Value::Str("1.5e3".into()));
        assert_eq!(v(".inf"), Value::Float(f64::INFINITY));
        assert_eq!(v("-.Inf"), Value::Float(f64::NEG_INFINITY));
        assert!(matches!(v(".nan"), Value::Float(f) if f.is_nan()));
    }

    #[test]
    fn sexagesimal() {
        assert_eq!(v("1:30"), Value::Int(90));
        assert_eq!(v("1:02:03"), Value::Int(3723));
        assert_eq!(v("-1:30"), Value::Int(-90));
        assert_eq!(v("1:30.5"), Value::Float(90.5));
    }

    #[test]
    fn dates_are_flagged() {
        assert_eq!(
            resolve_plain("2024-01-01"),
            (
                Value::Str("2024-01-01".into()),
                Some(PsychNote::DisallowedDate)
            )
        );
        assert_eq!(
            resolve_plain("2024-01-01 10:00:00").1,
            Some(PsychNote::DisallowedDate)
        );
    }

    #[test]
    fn symbols_are_flagged() {
        assert_eq!(
            resolve_plain(":prod"),
            (Value::Str(":prod".into()), Some(PsychNote::SymbolScalar))
        );
        assert_eq!(v(":"), Value::Str(":".into()));
    }

    #[test]
    fn obvious_strings() {
        assert_eq!(v("hello world"), Value::Str("hello world".into()));
        assert_eq!(v("v1.2.3"), Value::Str("v1.2.3".into()));
        assert_eq!(v("x=1"), Value::Str("x=1".into()));
        assert_eq!(v("infinity"), Value::Str("infinity".into()));
        assert_eq!(v("e5"), Value::Str("e5".into()));
        assert_eq!(v("-foo"), Value::Str("-foo".into()));
        assert_eq!(v("1.2.3"), Value::Str("1.2.3".into()));
    }

    #[test]
    fn overflow() {
        assert_eq!(
            resolve_plain("99999999999999999999").1,
            Some(PsychNote::IntOverflow)
        );
        assert_eq!(v("9223372036854775807"), Value::Int(i64::MAX));
    }
}
