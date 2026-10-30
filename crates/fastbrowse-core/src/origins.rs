//! The exact-origin grant, which documents a run may inspect and control.
//!
//! Addresses are parsed by the WHATWG URL Standard, so an origin here is the origin Chrome gives the document.

use std::collections::HashSet;
use std::fmt;

use url::Url;

pub const GRANT_ERROR: &str = "allowed_origins entries are bare http(s) origins such as https://example.com";
pub const EMPTY_ERROR: &str = "allowed_origins is empty: pass None for an unscoped run";

/// Every way `parse_origin` and `parse_origins` refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginError {
    /// The entry is not a bare http(s) origin.
    NotBare,
    /// `parse_origins` got nothing.
    Empty,
}

impl fmt::Display for OriginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            OriginError::NotBare => GRANT_ERROR,
            OriginError::Empty => EMPTY_ERROR,
        })
    }
}

impl std::error::Error for OriginError {}

/// `scheme://host[:port]` as Chrome serializes the document's origin, or `None` for anything that is not an
/// http(s) address. An opaque address (about:, data:, blob:, file:, javascript:) has no origin here, even where
/// WHATWG would derive one, as it does for blob:.
pub fn origin_of(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    Some(parsed.origin().ascii_serialization())
}

/// The normalized origin a grant entry names.
///
/// An entry is configuration a person wrote, so it is held to the plain form: `http(s)://host[:port]` with an
/// optional `/`, nothing a browser would silently strip or reinterpret (whitespace, control characters, `\`),
/// and no wildcard, userinfo, path, query, fragment or empty label (a trailing dot is a distinct host and stays).
pub fn parse_origin(value: &str) -> Result<String, OriginError> {
    let lower = value.to_ascii_lowercase();
    if value.chars().any(|c| c <= ' ' || c == '\u{7f}' || matches!(c, '\\' | '*' | '?' | '#' | '@'))
        || !(lower.starts_with("http://") || lower.starts_with("https://"))
        || lower.contains(":///")
    {
        return Err(OriginError::NotBare);
    }
    let parsed = Url::parse(value).map_err(|_| OriginError::NotBare)?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.domain().is_some_and(|d| d.starts_with('.') || d.contains(".."))
    {
        return Err(OriginError::NotBare);
    }
    origin_of(value).ok_or(OriginError::NotBare)
}

/// Each entry normalized, first occurrence order, duplicates dropped.
pub fn parse_origins<I, S>(values: I) -> Result<Vec<String>, OriginError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen = HashSet::new();
    let mut origins = Vec::new();
    for value in values {
        let origin = parse_origin(value.as_ref())?;
        if seen.insert(origin.clone()) {
            origins.push(origin);
        }
    }
    if origins.is_empty() {
        return Err(OriginError::Empty);
    }
    Ok(origins)
}

/// Only documents at an approved exact origin are granted.
pub struct OriginGrant {
    origins: HashSet<String>,
}

impl OriginGrant {
    pub fn new<I: IntoIterator<Item = String>>(origins: I) -> Self {
        Self { origins: origins.into_iter().collect() }
    }

    pub fn allows(&self, url: &str) -> bool {
        origin_of(url).is_some_and(|origin| self.origins.contains(&origin))
    }
}

#[cfg(test)]
mod tests {
    //! The behavior WHATWG changes, pinned.
    use super::*;

    fn grant() -> OriginGrant {
        OriginGrant::new(["https://a.example".to_string()])
    }

    #[test]
    fn a_grant_entry_is_normalized_to_one_exact_origin() {
        for (entry, origin) in [
            ("https://Example.COM", "https://example.com"),
            ("https://example.com:443/", "https://example.com"),
            ("http://example.com:80", "http://example.com"),
            ("http://127.0.0.1:8080", "http://127.0.0.1:8080"),
            ("http://[::1]:3000", "http://[::1]:3000"),
            ("https://b\u{fc}cher.example", "https://xn--bcher-kva.example"),
            ("https://example.com.", "https://example.com."),
            ("https://fa\u{df}.de", "https://xn--fa-hia.de"),
            ("http://0x7f.1", "http://127.0.0.1"),
            ("https://ex%41mple.com", "https://example.com"),
        ] {
            assert_eq!(parse_origin(entry).as_deref(), Ok(origin), "{entry}");
        }
    }

    #[test]
    fn a_grant_entry_that_is_not_a_bare_origin_is_refused() {
        for entry in [
            "",
            "example.com",
            "*",
            "https://*.example.com",
            "ftp://example.com",
            "file:///tmp",
            "blob:https://example.com/id",
            "https://good@evil.example",
            "https://user:pw@example.com",
            "https://@example.com",
            "https://example.com/app",
            "https://example.com?x=1",
            "https://example.com#frag",
            "https://example.com:notaport",
            "https://exam ple.com",
            "https://good\\@evil.example",
            // Forms WHATWG would repair; an entry is refused instead, so it reads as what it grants.
            " https://example.com",
            "https://exa\tmple.com",
            "https:example.com",
            "https:/example.com",
            "https:///example.com",
            "https://.example.com",
            "https://a..example.com",
            "https://\u{3002}",
            "https://example.com\\",
            // WHATWG refuses these where CPython accepted them.
            "https://[fe80::1%25eth0]",
            "https://[v1.x]",
            "https://example.com:65536",
            "https://xn--a.example",
        ] {
            assert_eq!(parse_origin(entry), Err(OriginError::NotBare), "{entry:?}");
        }
    }

    #[test]
    fn a_grant_needs_an_origin() {
        assert_eq!(parse_origins(Vec::<String>::new()), Err(OriginError::Empty));
        assert_eq!(
            parse_origins(["https://A.example", "https://a.example:443"]).unwrap(),
            vec!["https://a.example".to_string()]
        );
        assert_eq!(parse_origins(["https://a.example/path"]), Err(OriginError::NotBare));
    }

    #[test]
    fn a_grant_allows_only_exact_origins() {
        for (url, allowed) in [
            ("https://a.example/page?token=1#x", true),
            ("https://a.example:443/", true),
            ("about:blank", false),
            ("https://a.example:8443/", false),
            ("http://a.example/", false),
            ("https://a.example.evil.example/", false),
            ("https://evil.example/https://a.example/", false),
            ("https://a.example@evil.example/", false),
            ("https://a.example.:443/", false),
            ("https://a.example\t@evil.example/", false),
            ("about:blank#x", false),
            ("data:text/html,hi", false),
            ("blob:https://a.example/id", false),
            ("javascript:alert(1)", false),
            ("view-source:https://a.example/", false),
            ("chrome-error://chromewebdata/", false),
            ("about:srcdoc", false),
            ("", false),
        ] {
            assert_eq!(grant().allows(url), allowed, "{url:?}");
        }
    }

    /// Where CPython's urlsplit and Chrome disagree, the origin is Chrome's: the document Chrome loaded.
    #[test]
    fn an_address_has_the_origin_chrome_gives_it() {
        for (url, origin) in [
            // `\` is `/` in a special URL, so the `@` sits in the path, not the userinfo.
            ("https://a.example\\@evil.example/", Some("https://a.example")),
            // Tabs and newlines are removed before parsing: the host is evil.example.
            ("https://a.example\t@evil.example/", Some("https://evil.example")),
            // A percent-encoded host is decoded.
            ("https://a.exam%70le/", Some("https://a.example")),
            ("https://EX%41MPLE.com/", Some("https://example.com")),
            // A special scheme needs no slashes, and extra ones are skipped.
            ("https:a.example/", Some("https://a.example")),
            ("https:///a.example/", Some("https://a.example")),
            // Leading and trailing C0 controls and spaces are stripped.
            ("  https://a.example/  ", Some("https://a.example")),
            // IPv4 in any form WHATWG reads is serialized dotted-decimal.
            ("http://0x7f.1/", Some("http://127.0.0.1")),
            ("http://2130706433/", Some("http://127.0.0.1")),
            // IPv6 is serialized compressed and lowercase.
            ("http://[::FFFF:1.2.3.4]/", Some("http://[::ffff:102:304]")),
            ("http://[2001:db8:0:0:0:0:0:1]:80/", Some("http://[2001:db8::1]")),
            // A trailing dot names a different host, and stays.
            ("https://a.example./", Some("https://a.example.")),
            // Ports: leading zeros and an empty port are a default port.
            ("https://a.example:0443/", Some("https://a.example")),
            ("https://a.example:/", Some("https://a.example")),
            ("https://a.example:65536/", None),
            ("https://a.example:+1/", None),
            // UTS #46 nontransitional: `ß` stays `ß`, as in Chrome, where IDNA 2003 made it `ss`.
            ("https://fa\u{df}.de/", Some("https://xn--fa-hia.de")),
            ("https://\u{3c2}.example/", Some("https://xn--3xa.example")),
            ("https://\u{ff21}\u{ff22}.example/", Some("https://ab.example")),
            ("https://b\u{fc}cher\u{3002}example/", Some("https://xn--bcher-kva.example")),
            // Refused outright where CPython accepted.
            ("https://[fe80::1%25eth0]/", None),
            ("https://[v1.x]/", None),
            ("https://[1.2.3.4]/", None),
            ("https://xn--a.example/", None),
            ("https://a\u{fffd}.example/", None),
            ("https://exam ple.com/", None),
        ] {
            assert_eq!(origin_of(url).as_deref(), origin, "{url:?}");
        }
    }

    #[test]
    fn a_port_the_scheme_implies_is_not_part_of_the_origin() {
        for (url, origin) in [
            ("https://shop.example.test:443/cart", "https://shop.example.test"),
            ("http://shop.example.test:80/", "http://shop.example.test"),
            ("https://SHOP.example.test/", "https://shop.example.test"),
            ("https://shop.example.test:8443/", "https://shop.example.test:8443"),
            ("http://shop.example.test:443/", "http://shop.example.test:443"),
            ("https://user:pw@shop.example.test/", "https://shop.example.test"),
            ("https://[::1]/login", "https://[::1]"),
            ("https://[::1]:8443/", "https://[::1]:8443"),
            ("http://[2001:db8::1]:80/", "http://[2001:db8::1]"),
        ] {
            assert_eq!(origin_of(url).as_deref(), Some(origin), "{url}");
        }
    }
}
