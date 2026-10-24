//! Port of `src/fastbrowse/adapters/bitwarden.py`: login values from a Bitwarden vault item, read through the `bw` CLI
//! the user has unlocked.
//!
//! The item's own saved URIs decide where it may be used, under each URI's match detection. Secrets here are scoped
//! to an origin, not a page, so Starts with and Exact narrow to the URI's origin; a regular expression is not trusted,
//! and a URI set to Never matches nothing. A login saved for https is not released over http.

use std::process::{Command, Stdio};

use fastbrowse_core::models::SecretValue;
use fastbrowse_core::py::text::repr;
use indexmap::IndexMap;
use serde_json::Value;

use crate::options::SplitUrl;
use crate::totp;

/// The vault is locked, the item is missing, or it does not belong to the start origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitwardenError(pub String);

/// True when Bitwarden would fill the login saved for `uri` on `origin`. `detection` is Bitwarden's stored code; an
/// unset one means the account default, base domain.
pub fn covers(uri: &str, detection: Option<i64>, origin: &str) -> bool {
    // Vault URIs are often saved bare ("amazon.com"), which urlsplit reads as a path, not a host.
    let full = if uri.contains("://") { uri.to_string() } else { format!("https://{uri}") };
    let saved = SplitUrl::of(&full);
    let visited = SplitUrl::of(origin);
    if saved.hostname().is_empty() || visited.scheme != saved.scheme {
        return false;
    }
    match detection.unwrap_or(0) {
        0 => {
            let saved_host = saved.hostname();
            let base = saved_host.strip_prefix("www.").unwrap_or(&saved_host);
            let host = visited.hostname();
            host == base || host.ends_with(&format!(".{base}"))
        }
        1..=3 => visited.netloc.to_lowercase() == saved.netloc.to_lowercase(),
        _ => false,
    }
}

fn text(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

/// The item's username, password and authenticator code, keyed by the secret names the agent sees.
pub fn login_values(item_json: &str, origin: &str) -> Result<IndexMap<String, SecretValue>, BitwardenError> {
    // pydantic's message would quote the input, which here holds the password.
    let unreadable = || BitwardenError("bw returned something other than a vault item".into());
    let item: Value = serde_json::from_str(item_json).map_err(|_| unreadable())?;
    let name = text(item.get("name")).ok_or_else(unreadable)?;
    let login = match item.get("login") {
        None | Some(Value::Null) => {
            return Err(BitwardenError(format!("Bitwarden item {} is not a login", repr(name))));
        }
        Some(login @ Value::Object(_)) => login,
        Some(_) => return Err(unreadable()),
    };
    let uris = login.get("uris").and_then(Value::as_array).cloned().unwrap_or_default();
    let saved_here = uris.iter().any(|saved| {
        let detection = saved.get("match").and_then(Value::as_i64);
        text(saved.get("uri")).is_some_and(|uri| !uri.is_empty() && covers(uri, detection, origin))
    });
    if !saved_here {
        return Err(BitwardenError(format!("Bitwarden item {} is not saved for {origin}", repr(name))));
    }
    let mut values = IndexMap::new();
    for field in ["username", "password"] {
        if let Some(value) = text(login.get(field)).filter(|value| !value.is_empty()) {
            values.insert(field.to_string(), SecretValue::Plain(value.to_string()));
        }
    }
    if let Some(key) = text(login.get("totp")).filter(|key| !key.is_empty()) {
        // The code is made when it is typed, and a key that cannot make one fails here rather than at that prompt.
        let code = totp::one_time_code(key).map_err(|error| {
            BitwardenError(format!(
                "Bitwarden item {} has an authenticator key fastbrowse cannot use: {}",
                repr(name),
                error.0
            ))
        })?;
        values.insert("one_time_code".into(), code);
    }
    Ok(values)
}

/// Read `item` (a name or id) from the unlocked vault; `BW_SESSION` must be in the environment.
pub fn login(item: &str, origin: &str) -> Result<IndexMap<String, SecretValue>, BitwardenError> {
    let done = Command::new("bw")
        .args(["get", "item", item, "--nointeraction"])
        .stdin(Stdio::null())
        .output()
        .map_err(|_| BitwardenError("the Bitwarden CLI (bw) is not installed".into()))?;
    if !done.status.success() {
        // bw reports a locked vault and a missing item on stderr; neither carries a secret. An expired CLI login
        // surfaces as "Not found" above a Node stack trace, which hides the one fix that works.
        let stderr = String::from_utf8_lossy(&done.stderr);
        let detail = stderr.trim();
        if detail.contains("invalid_grant") {
            return Err(BitwardenError("the bw CLI's login has expired: run bw login, then bw unlock".into()));
        }
        let first = detail.lines().next().unwrap_or("no output");
        return Err(BitwardenError(format!(
            "bw get item failed: {}",
            if detail.is_empty() { "no output" } else { first }
        )));
    }
    login_values(&String::from_utf8_lossy(&done.stdout), origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_uri_covers_an_origin_by_its_match_detection() {
        assert!(covers("amazon.com", None, "https://www.amazon.com"));
        assert!(!covers("amazon.com", None, "http://amazon.com"));
        assert!(covers("https://login.example.com/path", Some(3), "https://login.example.com"));
        assert!(!covers("https://login.example.com", Some(1), "https://example.com"));
        assert!(!covers("https://example.com", Some(4), "https://example.com"));
    }

    #[test]
    fn an_item_not_saved_for_the_origin_is_refused() {
        let item = r#"{"name": "Shop", "login": {"username": "u", "password": "p", "uris": [{"uri": "shop.test"}]}}"#;
        assert_eq!(
            login_values(item, "https://bank.test").unwrap_err().0,
            "Bitwarden item 'Shop' is not saved for https://bank.test"
        );
        let values = login_values(item, "https://shop.test").unwrap();
        assert_eq!(values.keys().collect::<Vec<_>>(), ["username", "password"]);
        assert_eq!(
            login_values("{}", "https://shop.test").unwrap_err().0,
            "bw returned something other than a vault item"
        );
    }
}
