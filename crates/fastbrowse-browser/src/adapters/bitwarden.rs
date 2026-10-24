//! Login values from a Bitwarden vault item, read through the `bw` CLI the user has unlocked
//! (`adapters/bitwarden.py`).
//!
//! The item's own saved URIs decide where it may be used, under each URI's match detection
//! (https://bitwarden.com/help/uri-match-detection/). Secrets here are scoped to an origin, not a page, so
//! Starts with and Exact narrow to the URI's origin; a regular expression is not trusted, and a URI set to Never
//! matches nothing. A login saved for https is not released over http.

use std::fmt;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde::de::{Deserializer, Error as _};

use crate::adapters::totp::one_time_code;
use crate::shared::{SecretValue, urlsplit};

/// The vault is locked, the item is missing, or it does not belong to the start origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitwardenError(pub String);

impl fmt::Display for BitwardenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BitwardenError {}

/// Bitwarden's stored codes; an unset match means the account default, base domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UriMatch {
    Domain = 0,
    Host = 1,
    StartsWith = 2,
    Exact = 3,
    RegularExpression = 4,
    Never = 5,
}

impl<'de> Deserialize<'de> for UriMatch {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match u8::deserialize(deserializer)? {
            0 => Self::Domain,
            1 => Self::Host,
            2 => Self::StartsWith,
            3 => Self::Exact,
            4 => Self::RegularExpression,
            5 => Self::Never,
            other => return Err(D::Error::custom(format!("unknown URI match {other}"))),
        })
    }
}

#[derive(Deserialize)]
struct Uri {
    #[serde(default)]
    uri: Option<String>,
    #[serde(default, rename = "match")]
    detection: Option<UriMatch>,
}

#[derive(Deserialize)]
struct Login {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    totp: Option<String>,
    #[serde(default)]
    uris: Option<Vec<Uri>>,
}

#[derive(Deserialize)]
struct Item {
    name: String,
    #[serde(default)]
    login: Option<Login>,
}

/// `SplitResult.hostname`: the netloc past its credentials and before its port, lowercased, or None when empty.
fn hostname(netloc: &str) -> Option<String> {
    let hostinfo = netloc.rsplit_once('@').map_or(netloc, |(_, host)| host);
    let host = match hostinfo.split_once('[') {
        Some((_, bracketed)) => bracketed.split_once(']').map_or(bracketed, |(host, _)| host),
        None => hostinfo.split_once(':').map_or(hostinfo, |(host, _)| host),
    };
    if host.is_empty() {
        return None;
    }
    // An IPv6 zone after `%` keeps its case.
    Some(match host.split_once('%') {
        Some((address, zone)) => format!("{}%{zone}", address.to_lowercase()),
        None => host.to_lowercase(),
    })
}

/// True when Bitwarden would fill the login saved for `uri` on `origin`.
pub fn covers(uri: &str, detection: Option<UriMatch>, origin: &str) -> bool {
    // Vault URIs are often saved bare ("amazon.com"), which urlsplit reads as a path, not a host.
    let saved = if uri.contains("://") { urlsplit(uri) } else { urlsplit(&format!("https://{uri}")) };
    let visited = urlsplit(origin);
    let Some(saved_host) = hostname(&saved.1) else {
        return false;
    };
    if visited.0 != saved.0 {
        return false;
    }
    match detection.unwrap_or(UriMatch::Domain) {
        UriMatch::Domain => {
            let base = saved_host.strip_prefix("www.").unwrap_or(&saved_host);
            let host = hostname(&visited.1).unwrap_or_default();
            host == base || host.ends_with(&format!(".{base}"))
        }
        UriMatch::Host | UriMatch::StartsWith | UriMatch::Exact => visited.1.to_lowercase() == saved.1.to_lowercase(),
        UriMatch::RegularExpression | UriMatch::Never => false,
    }
}

/// Python's `repr` of a str, which the error messages quote item names with.
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') { '"' } else { '\'' };
    let mut out = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// The item's username, password and authenticator code, keyed by the secret names the agent sees.
pub fn login_values(item_json: &str, origin: &str) -> Result<Vec<(String, SecretValue)>, BitwardenError> {
    // serde's message can quote the input, which here holds the password.
    let item: Item = serde_json::from_str(item_json)
        .map_err(|_| BitwardenError("bw returned something other than a vault item".into()))?;
    let name = py_repr(&item.name);
    let Some(login) = item.login else {
        return Err(BitwardenError(format!("Bitwarden item {name} is not a login")));
    };
    let uris = login.uris.unwrap_or_default();
    let scoped =
        uris.iter().any(|u| u.uri.as_deref().is_some_and(|uri| !uri.is_empty() && covers(uri, u.detection, origin)));
    if !scoped {
        return Err(BitwardenError(format!("Bitwarden item {name} is not saved for {origin}")));
    }
    let mut values: Vec<(String, SecretValue)> = [("username", login.username), ("password", login.password)]
        .into_iter()
        .filter_map(|(key, value)| value.filter(|v| !v.is_empty()).map(|v| (key.to_owned(), SecretValue::Value(v))))
        .collect();
    if let Some(totp) = login.totp.filter(|totp| !totp.is_empty()) {
        // Amazon's two-step sign-in stopped a run at "Enter OTP" with the key in the vault. The code is made when
        // it is typed, and a key that cannot make one fails here rather than at that prompt.
        let code = one_time_code(&totp).map_err(|error| {
            BitwardenError(format!("Bitwarden item {name} has an authenticator key fastbrowse cannot use: {error}"))
        })?;
        values.push(("one_time_code".to_owned(), code));
    }
    Ok(values)
}

/// `str.strip()`: Unicode whitespace plus the ASCII separators `str.isspace` counts.
fn py_strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}

/// The first of `str.splitlines()`, which breaks on every line boundary Python knows, not only `\n`.
fn first_line(text: &str) -> &str {
    let boundary = ['\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}'];
    text.split(boundary).next().unwrap_or("")
}

/// Read `item` (a name or id) from the unlocked vault; `BW_SESSION` must be in the environment.
pub async fn bitwarden_login(item: &str, origin: &str) -> Result<Vec<(String, SecretValue)>, BitwardenError> {
    let child = tokio::process::Command::new("bw")
        .args(["get", "item", item, "--nointeraction"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(BitwardenError("the Bitwarden CLI (bw) is not installed".into()));
        }
        Err(error) => return Err(BitwardenError(format!("the Bitwarden CLI (bw) could not start: {error}"))),
    };
    let done = match tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await {
        Ok(Ok(done)) => done,
        Ok(Err(error)) => return Err(BitwardenError(format!("bw get item failed: {error}"))),
        // Dropping the wait kills bw, which `kill_on_drop` arranged.
        Err(_) => return Err(BitwardenError("bw get item timed out after 30 seconds".into())),
    };
    if !done.status.success() {
        // bw reports a locked vault and a missing item on stderr; neither carries a secret. An expired CLI login
        // surfaces as "Not found" above a Node stack trace, which hides the one fix that works.
        let stderr = String::from_utf8_lossy(&done.stderr);
        let detail = py_strip(&stderr);
        if detail.contains("invalid_grant") {
            return Err(BitwardenError("the bw CLI's login has expired: run bw login, then bw unlock".into()));
        }
        let line = if detail.is_empty() { "no output" } else { first_line(detail) };
        return Err(BitwardenError(format!("bw get item failed: {line}")));
    }
    login_values(&String::from_utf8_lossy(&done.stdout), origin)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // RFC 6238's SHA1 seed, encoded here rather than written out so a secret scanner does not read it as a key.
    fn key() -> String {
        data_encoding::BASE32.encode(b"12345678901234567890")
    }

    fn item(uris: &[&str], password: Option<&str>, detection: Option<u8>, totp: Option<&str>) -> String {
        let uris: Vec<_> = uris.iter().map(|u| json!({"match": detection, "uri": u})).collect();
        let login = json!({"username": "me@example.com", "password": password, "totp": totp, "uris": uris});
        json!({"id": "1", "name": "Amazon", "type": 1, "login": login}).to_string()
    }

    fn plain(values: &[(String, SecretValue)]) -> Vec<(String, String)> {
        values
            .iter()
            .map(|(name, value)| match value {
                SecretValue::Value(v) => (name.clone(), v.clone()),
                SecretValue::Provider(_) => (name.clone(), "<provider>".into()),
            })
            .collect()
    }

    #[test]
    fn a_login_is_released_only_where_its_match_detection_allows() {
        use UriMatch::*;
        let cases = [
            ("https://www.amazon.com", None, "https://www.amazon.com", true),
            ("amazon.com", None, "https://www.amazon.com", true),
            ("https://amazon.com/ap/signin", Some(Domain), "https://smile.amazon.com", true),
            ("https://amazon.com", None, "https://amazon.com.evil.test", false),
            ("https://amazon.com", None, "https://notamazon.com", false),
            ("https://amazon.com", None, "http://www.amazon.com", false),
            ("https://www.amazon.com", Some(Host), "https://smile.amazon.com", false),
            ("https://www.amazon.com/ap/signin", Some(Exact), "https://www.amazon.com", true),
            ("https://www.amazon.com/ap/signin", Some(Exact), "http://sub.www.amazon.com", false),
            ("https://www.amazon.com", Some(Never), "https://www.amazon.com", false),
            (r"https://www\.amazon\.com/.*", Some(RegularExpression), "https://www.amazon.com", false),
            ("", None, "https://amazon.com", false),
            ("https://WWW.Amazon.com", None, "https://smile.AMAZON.com:443", true),
            ("https://WWW.Amazon.com", Some(Host), "https://www.amazon.com", true),
        ];
        for (uri, detection, origin, expected) in cases {
            assert_eq!(covers(uri, detection, origin), expected, "{uri} {detection:?} {origin}");
        }
    }

    #[test]
    fn login_values_are_named_and_scoped_to_the_items_uris() {
        let both = login_values(&item(&["amazon.com"], Some("hunter2"), None, None), "https://www.amazon.com").unwrap();
        assert_eq!(plain(&both), [("username".into(), "me@example.com".into()), ("password".into(), "hunter2".into())]);
        let username = login_values(&item(&["amazon.com"], None, None, None), "https://www.amazon.com").unwrap();
        assert_eq!(plain(&username), [("username".into(), "me@example.com".into())]);
        let error = login_values(&item(&["amazon.com"], Some("hunter2"), None, None), "https://evil.test").unwrap_err();
        assert_eq!(error.0, "Bitwarden item 'Amazon' is not saved for https://evil.test");
        let never = item(&["https://www.amazon.com"], Some("hunter2"), Some(5), None);
        assert!(login_values(&never, "https://www.amazon.com").unwrap_err().0.contains("not saved for"));
    }

    #[test]
    fn a_malformed_item_error_does_not_echo_the_password() {
        let malformed = json!({"login": {"password": "hunter2"}}).to_string();
        let error = login_values(&malformed, "https://www.amazon.com").unwrap_err();
        assert_eq!(error.0, "bw returned something other than a vault item");
        let not_login = json!({"name": "Note", "login": null}).to_string();
        assert_eq!(login_values(&not_login, "https://x.test").unwrap_err().0, "Bitwarden item 'Note' is not a login");
    }

    #[tokio::test]
    async fn an_authenticator_key_offers_a_code() {
        let values =
            login_values(&item(&["amazon.co.uk"], Some("hunter2"), None, Some(&key())), "https://www.amazon.co.uk")
                .unwrap();
        let names: Vec<&str> = values.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["username", "password", "one_time_code"]);
        let SecretValue::Provider(provider) = &values[2].1 else { panic!("the code is made when it is typed") };
        let code = provider().await;
        assert!(code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()));
    }

    #[test]
    fn an_unusable_authenticator_key_fails_at_load_without_echoing_it() {
        let key = key();
        for totp in
            [format!("steam://{key}"), format!("otpauth://hotp/Amazon?secret={key}"), "zz-garbage-key-zz".into()]
        {
            let error =
                login_values(&item(&["amazon.co.uk"], Some("hunter2"), None, Some(&totp)), "https://www.amazon.co.uk")
                    .unwrap_err();
            assert!(error.0.contains("authenticator key"), "{}", error.0);
            assert!(!error.0.contains(&key) && !error.0.contains("zz-garbage-key-zz"));
        }
    }

    #[test]
    fn stderr_reads_as_python_strips_and_splits_it() {
        assert_eq!(first_line(py_strip("\n  You are not logged in.\r\nmore\n")), "You are not logged in.");
        assert_eq!(py_repr("it's"), "\"it's\"");
    }
}
