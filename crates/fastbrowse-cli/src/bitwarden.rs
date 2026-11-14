//! Login values from a Bitwarden vault item, read through rbw when it is installed, so a locked vault prompts
//! through rbw's pinentry, and otherwise through the `bw` CLI the user has unlocked.
//!
//! The item's own saved URIs decide where it may be used, under each URI's match detection. Secrets here are scoped
//! to an origin, not a page, so Starts with and Exact narrow to the URI's origin; a regular expression is not trusted,
//! and a URI set to Never matches nothing. A login saved for https is not released over http.

use std::io;
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

/// The program a vault item is read through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reader {
    Rbw,
    Bw,
}

impl Reader {
    pub fn program(self) -> &'static str {
        match self {
            Reader::Rbw => "rbw",
            Reader::Bw => "bw",
        }
    }

    fn args(self, item: &str) -> Vec<&str> {
        match self {
            Reader::Rbw => vec!["get", "--raw", item],
            Reader::Bw => vec!["get", "item", item, "--nointeraction"],
        }
    }

    /// The item key holding the login, and the URI key holding its match detection, in this program's JSON.
    fn keys(self) -> (&'static str, &'static str) {
        match self {
            Reader::Rbw => ("data", "match_type"),
            Reader::Bw => ("login", "match"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Reader::Rbw => "rbw",
            Reader::Bw => "the Bitwarden CLI (bw)",
        }
    }
}

/// The item's username, password and authenticator code, keyed by the secret names the agent sees.
pub fn login_values(
    reader: Reader,
    item_json: &str,
    origin: &str,
) -> Result<IndexMap<String, SecretValue>, BitwardenError> {
    // serde's message would quote the input, which here holds the password.
    let unreadable = || BitwardenError(format!("{} returned something other than a vault item", reader.program()));
    let item: Value = serde_json::from_str(item_json).map_err(|_| unreadable())?;
    let name = text(item.get("name")).ok_or_else(unreadable)?;
    let (login_key, match_key) = reader.keys();
    let not_a_login = || BitwardenError(format!("Bitwarden item {} is not a login", repr(name)));
    let login = match item.get(login_key) {
        None | Some(Value::Null) => return Err(not_a_login()),
        // rbw's `data` holds every item type; only its login variant has a `uris` key, serialized even when null.
        Some(Value::Object(fields)) if reader == Reader::Rbw && !fields.contains_key("uris") => {
            return Err(not_a_login());
        }
        Some(login @ Value::Object(_)) => login,
        Some(_) => return Err(unreadable()),
    };
    let uris = login.get("uris").and_then(Value::as_array).cloned().unwrap_or_default();
    let saved_here = uris.iter().any(|saved| {
        let detection = saved.get(match_key).and_then(Value::as_i64);
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

fn spawn_failure(reader: Reader, error: &io::Error) -> BitwardenError {
    if error.kind() == io::ErrorKind::NotFound {
        return BitwardenError(format!("{} is not installed", reader.name()));
    }
    BitwardenError(format!("{} could not be started: {error}", reader.name()))
}

/// What a reader printed and whether it exited successfully.
pub struct Ran {
    pub ok: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

fn run_failure(reader: Reader, stderr: &[u8]) -> BitwardenError {
    // Both programs report a locked vault and a missing item on stderr; neither carries a secret.
    let stderr = String::from_utf8_lossy(stderr);
    let detail = stderr.trim();
    // An expired bw login surfaces as "Not found" above a Node stack trace, which hides the one fix that works.
    if reader == Reader::Bw && detail.contains("invalid_grant") {
        return BitwardenError("the bw CLI's login has expired: run bw login, then bw unlock".into());
    }
    let first = detail.lines().next().filter(|line| !line.is_empty()).unwrap_or("no output");
    let command = match reader {
        Reader::Rbw => "rbw get",
        Reader::Bw => "bw get item",
    };
    BitwardenError(format!("{command} failed: {first}"))
}

/// Read `item` through rbw, or through bw only when rbw is not installed; `run` starts a reader with its arguments.
pub fn read_with(
    item: &str,
    origin: &str,
    run: impl Fn(Reader, &[&str]) -> io::Result<Ran>,
) -> Result<IndexMap<String, SecretValue>, BitwardenError> {
    let (reader, done) = match run(Reader::Rbw, &Reader::Rbw.args(item)) {
        Ok(done) => (Reader::Rbw, done),
        Err(error) if error.kind() == io::ErrorKind::NotFound => match run(Reader::Bw, &Reader::Bw.args(item)) {
            Ok(done) => (Reader::Bw, done),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(BitwardenError("neither rbw nor the Bitwarden CLI (bw) is installed".into()));
            }
            Err(error) => return Err(spawn_failure(Reader::Bw, &error)),
        },
        Err(error) => return Err(spawn_failure(Reader::Rbw, &error)),
    };
    if !done.ok {
        return Err(run_failure(reader, &done.stderr));
    }
    login_values(reader, &String::from_utf8_lossy(&done.stdout), origin)
}

/// Read `item` (a name or id) through rbw, which unlocks through its pinentry, or else through bw, which needs
/// `BW_SESSION` in the environment.
pub fn login(item: &str, origin: &str) -> Result<IndexMap<String, SecretValue>, BitwardenError> {
    read_with(item, origin, |reader, args| {
        let done = Command::new(reader.program()).args(args).stdin(Stdio::null()).output()?;
        Ok(Ran { ok: done.status.success(), stdout: done.stdout, stderr: done.stderr })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_missing_program_reads_as_not_installed() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(spawn_failure(Reader::Bw, &missing).0.contains("not installed"));
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let denied = spawn_failure(Reader::Rbw, &denied).0;
        assert!(denied.contains("could not be started") && !denied.contains("not installed"), "{denied}");
    }

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
            login_values(Reader::Bw, item, "https://bank.test").unwrap_err().0,
            "Bitwarden item 'Shop' is not saved for https://bank.test"
        );
        let values = login_values(Reader::Bw, item, "https://shop.test").unwrap();
        assert_eq!(values.keys().collect::<Vec<_>>(), ["username", "password"]);
        assert_eq!(
            login_values(Reader::Bw, "{}", "https://shop.test").unwrap_err().0,
            "bw returned something other than a vault item"
        );
    }

    const RBW_ITEM: &str = r#"{"id": "0d3c", "folder": null, "name": "Shop", "data": {"username": "u", "password": "p", "totp": null, "uris": [{"uri": "shop.test", "match_type": null}, {"uri": "https://bank.test", "match_type": 5}]}, "fields": [], "notes": null, "history": []}"#;

    fn ran(stdout: &str) -> std::io::Result<Ran> {
        Ok(Ran { ok: true, stdout: stdout.into(), stderr: Vec::new() })
    }

    #[test]
    fn an_rbw_item_is_read_and_scoped_by_its_saved_uris() {
        let values = login_values(Reader::Rbw, RBW_ITEM, "https://shop.test").unwrap();
        assert_eq!(values.keys().collect::<Vec<_>>(), ["username", "password"]);
        assert_eq!(
            login_values(Reader::Rbw, RBW_ITEM, "https://bank.test").unwrap_err().0,
            "Bitwarden item 'Shop' is not saved for https://bank.test"
        );
        let note = r#"{"id": "1", "name": "Note", "data": null, "fields": [], "notes": "n", "history": []}"#;
        assert_eq!(
            login_values(Reader::Rbw, note, "https://shop.test").unwrap_err().0,
            "Bitwarden item 'Note' is not a login"
        );
        let card = r#"{"id": "2", "name": "Card", "data": {"cardholder_name": "A", "number": "4", "brand": null, "exp_month": null, "exp_year": null, "code": null}, "fields": [], "notes": null, "history": []}"#;
        assert_eq!(
            login_values(Reader::Rbw, card, "https://shop.test").unwrap_err().0,
            "Bitwarden item 'Card' is not a login"
        );
    }

    #[test]
    fn rbw_is_read_first_and_bw_only_when_rbw_is_missing() {
        let bw_item =
            r#"{"name": "Shop", "login": {"username": "b", "password": "q", "uris": [{"uri": "shop.test"}]}}"#;
        let tried = std::cell::RefCell::new(Vec::new());
        let values = read_with("Shop", "https://shop.test", |reader, args| {
            tried.borrow_mut().push((reader.program(), args.join(" ")));
            match reader {
                Reader::Rbw => ran(RBW_ITEM),
                Reader::Bw => ran(bw_item),
            }
        })
        .unwrap();
        assert!(matches!(&values["username"], SecretValue::Plain(user) if user == "u"));
        assert_eq!(*tried.borrow(), [("rbw", "get --raw Shop".to_string())]);

        tried.borrow_mut().clear();
        let values = read_with("Shop", "https://shop.test", |reader, args| {
            tried.borrow_mut().push((reader.program(), args.join(" ")));
            match reader {
                Reader::Rbw => Err(std::io::ErrorKind::NotFound.into()),
                Reader::Bw => ran(bw_item),
            }
        })
        .unwrap();
        assert!(matches!(&values["username"], SecretValue::Plain(user) if user == "b"));
        assert_eq!(
            *tried.borrow(),
            [("rbw", "get --raw Shop".to_string()), ("bw", "get item Shop --nointeraction".to_string())]
        );

        let neither = read_with("Shop", "https://shop.test", |_, _| Err(std::io::ErrorKind::NotFound.into()));
        assert_eq!(neither.unwrap_err().0, "neither rbw nor the Bitwarden CLI (bw) is installed");
    }

    #[test]
    fn an_rbw_failure_is_reported_not_retried_with_bw() {
        let tried = std::cell::RefCell::new(Vec::new());
        let failed = read_with("Shop", "https://shop.test", |reader, _| {
            tried.borrow_mut().push(reader.program());
            Ok(Ran { ok: false, stdout: Vec::new(), stderr: b"rbw get: couldn't find entry for 'Shop'\n".to_vec() })
        });
        assert_eq!(failed.unwrap_err().0, "rbw get failed: rbw get: couldn't find entry for 'Shop'");
        assert_eq!(*tried.borrow(), ["rbw"]);
    }
}
