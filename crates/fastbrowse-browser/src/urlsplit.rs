//! CPython's `urlsplit`, which core does not port: it parses addresses as Chrome does, and these call sites mirror how
//! Python reads text a person typed (an `otpauth://` key, a vault URI) or labels an address.

/// CPython's `urlsplit`: (scheme, netloc, path, query, fragment).
pub fn urlsplit(url: &str) -> (String, String, String, String, String) {
    // The WHATWG C0 controls and space are stripped from the front, and tabs and newlines anywhere.
    let url: String =
        url.trim_start_matches(|c: char| c <= ' ').chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();
    let mut rest = url.as_str();
    let mut scheme = String::new();
    if let Some(colon) = rest.find(':') {
        let candidate = &rest[..colon];
        if candidate.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && candidate.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            scheme = candidate.to_ascii_lowercase();
            rest = &rest[colon + 1..];
        }
    }
    let mut netloc = String::new();
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = after[..end].to_owned();
        rest = &after[end..];
    }
    let (rest, fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    (scheme, netloc, path.to_owned(), query.to_owned(), fragment.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_keeps_what_python_keeps() {
        assert_eq!(
            urlsplit("HTTP://user@Example.com:8/a?q=1#f"),
            ("http".into(), "user@Example.com:8".into(), "/a".into(), "q=1".into(), "f".into())
        );
        assert_eq!(urlsplit("amazon.com").2, "amazon.com");
        assert_eq!(urlsplit(" \thttps://x.test").1, "x.test");
    }
}
