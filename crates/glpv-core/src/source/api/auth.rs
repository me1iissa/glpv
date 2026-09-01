//! Credential discovery for the API source.
//!
//! Order: an explicit token (`--token`) → `GLPV_TOKEN` → `GITLAB_TOKEN` →
//! glab's config (`hosts.<host>.token` and whether it is an OAuth token) →
//! the `glab api` CLI for a host glab knows but holds no plain token for →
//! anonymous (public projects only).

use std::collections::BTreeMap;
use std::path::PathBuf;

/// How a token travels: personal/project/group access tokens as
/// `PRIVATE-TOKEN`, OAuth access tokens as `Authorization: Bearer`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Auth {
    PrivateToken(String),
    Bearer(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenKind {
    Personal,
    OAuth,
    Unknown,
}

impl Auth {
    /// Classify a token: the `glpat-` family is a personal-style token;
    /// anything else of unknown provenance is sent as a bearer token, which
    /// GitLab accepts for personal access tokens as well as OAuth ones.
    pub fn from_token(token: &str, kind: TokenKind) -> Auth {
        let token = token.trim().to_string();
        match kind {
            TokenKind::Personal => Auth::PrivateToken(token),
            TokenKind::OAuth => Auth::Bearer(token),
            TokenKind::Unknown => {
                if token.starts_with("glpat-") || token.starts_with("glsoat-") {
                    Auth::PrivateToken(token)
                } else {
                    Auth::Bearer(token)
                }
            }
        }
    }

    pub fn header(&self) -> (&'static str, String) {
        match self {
            Auth::PrivateToken(t) => ("PRIVATE-TOKEN", t.clone()),
            Auth::Bearer(t) => ("Authorization", format!("Bearer {t}")),
        }
    }

    pub fn token(&self) -> &str {
        match self {
            Auth::PrivateToken(t) | Auth::Bearer(t) => t,
        }
    }

    /// The `Authorization` header value git-over-HTTP wants for this token
    /// (basic auth with the `oauth2` pseudo-user, accepted for personal
    /// tokens too).
    pub fn git_basic_header(&self) -> String {
        format!(
            "Basic {}",
            base64(format!("oauth2:{}", self.token()).as_bytes())
        )
    }
}

/// What [`discover`] settled on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Credentials {
    Token(Auth),
    /// Shell out to `glab api`, which refreshes its own OAuth session.
    Glab,
    Anonymous,
}

/// Discover credentials for `host`; the second value says where they came
/// from (for diagnostics — never the token itself).
pub fn discover(host: &str, explicit: Option<&str>) -> (Credentials, String) {
    if let Some(t) = explicit.map(str::trim).filter(|t| !t.is_empty()) {
        return (
            Credentials::Token(Auth::from_token(t, TokenKind::Unknown)),
            "token from --token".to_string(),
        );
    }
    for var in ["GLPV_TOKEN", "GITLAB_TOKEN"] {
        if let Some(t) = std::env::var(var)
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
        {
            return (
                Credentials::Token(Auth::from_token(&t, TokenKind::Unknown)),
                format!("token from ${var}"),
            );
        }
    }
    let glab_present = glab_on_path();
    if let Some(path) = glab_config_path()
        && let Ok(text) = std::fs::read_to_string(&path)
        && let Some(entry) = glab_host_entry(&text, host)
    {
        // glab lists hosts it merely knows about (gitlab.com by default);
        // only an entry with a token is a login.
        let token = entry.get("token").map(|t| t.trim()).unwrap_or("");
        let oauth = entry.get("is_oauth2").is_some_and(|v| v == "true");
        if !token.is_empty() && !oauth {
            return (
                Credentials::Token(Auth::from_token(token, TokenKind::Personal)),
                format!("token from glab config ({})", path.display()),
            );
        }
        if !token.is_empty() && glab_present {
            // OAuth sessions expire and glab knows how to refresh them.
            return (Credentials::Glab, "glab api".to_string());
        }
        if !token.is_empty() {
            return (
                Credentials::Token(Auth::from_token(token, TokenKind::OAuth)),
                format!("OAuth token from glab config ({})", path.display()),
            );
        }
    }
    (Credentials::Anonymous, "anonymous".to_string())
}

/// `$GLAB_CONFIG_DIR/config.yml`, else `$XDG_CONFIG_HOME/glab-cli/config.yml`,
/// else `~/.config/glab-cli/config.yml`.
pub fn glab_config_path() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("GLAB_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(d).join("config.yml"));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("glab-cli/config.yml"))
}

pub fn glab_on_path() -> bool {
    std::process::Command::new("glab")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The `hosts.<host>` mapping of a glab config, read with a deliberately
/// small YAML reader: two-level block mappings of scalars, which is all glab
/// writes there.
pub fn glab_host_entry(text: &str, host: &str) -> Option<BTreeMap<String, String>> {
    let host = host.to_lowercase();
    let mut in_hosts = false;
    let mut host_indent: Option<usize> = None;
    let mut current: Option<(String, BTreeMap<String, String>)> = None;
    let mut found: Option<BTreeMap<String, String>> = None;

    let finish = |current: &mut Option<(String, BTreeMap<String, String>)>,
                  found: &mut Option<BTreeMap<String, String>>| {
        if let Some((name, map)) = current.take()
            && name == host
            && found.is_none()
        {
            *found = Some(map);
        }
    };

    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - trimmed.len();
        if indent == 0 {
            finish(&mut current, &mut found);
            in_hosts = trimmed == "hosts:";
            host_indent = None;
            continue;
        }
        if !in_hosts {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = unquote(key.trim());
        let value = unquote(value.trim());
        match host_indent {
            None => {
                host_indent = Some(indent);
                current = Some((key.to_lowercase(), BTreeMap::new()));
            }
            Some(h) if indent == h => {
                finish(&mut current, &mut found);
                current = Some((key.to_lowercase(), BTreeMap::new()));
            }
            Some(h) if indent > h => {
                if let Some((_, map)) = &mut current {
                    map.insert(key, value);
                }
            }
            Some(_) => {}
        }
    }
    finish(&mut current, &mut found);
    found
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Standard base64 (RFC 4648 §4) — enough for one HTTP header.
pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_kinds() {
        assert_eq!(
            Auth::from_token("glpat-abc", TokenKind::Unknown),
            Auth::PrivateToken("glpat-abc".into())
        );
        assert_eq!(
            Auth::from_token("0123456789abcdef", TokenKind::Unknown),
            Auth::Bearer("0123456789abcdef".into())
        );
        assert_eq!(
            Auth::from_token(" legacy ", TokenKind::Personal),
            Auth::PrivateToken("legacy".into())
        );
        assert_eq!(
            Auth::Bearer("x".into()).header(),
            ("Authorization", "Bearer x".to_string())
        );
        assert_eq!(
            Auth::PrivateToken("x".into()).header(),
            ("PRIVATE-TOKEN", "x".to_string())
        );
        assert_eq!(
            Auth::PrivateToken("tok".into()).git_basic_header(),
            "Basic b2F1dGgyOnRvaw=="
        );
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn reads_the_host_block_of_a_glab_config() {
        // The shape glab writes (values are placeholders).
        let text = "\
# glab config
git_protocol: ssh
hosts:
    gitlab.com:
        token: not-a-real-token
        api_host: gitlab.com
    Gitlab.Example.Com:
        token: \"quoted-placeholder\"
        is_oauth2: \"true\"
        api_protocol: https
    other.example.org:
        api_host: other.example.org
editor:
";
        let e = glab_host_entry(text, "gitlab.com").unwrap();
        assert_eq!(e.get("token").map(String::as_str), Some("not-a-real-token"));
        assert!(!e.contains_key("is_oauth2"));

        let e = glab_host_entry(text, "gitlab.example.com").unwrap();
        assert_eq!(
            e.get("token").map(String::as_str),
            Some("quoted-placeholder")
        );
        assert_eq!(e.get("is_oauth2").map(String::as_str), Some("true"));

        let e = glab_host_entry(text, "other.example.org").unwrap();
        assert!(!e.contains_key("token"));

        assert!(glab_host_entry(text, "missing.example.org").is_none());
        assert!(glab_host_entry("editor: vi\n", "gitlab.com").is_none());
    }
}
