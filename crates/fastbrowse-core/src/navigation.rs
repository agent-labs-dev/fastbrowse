//! Port of `src/fastbrowse/navigation.py`: addresses supplied by the caller, never inferred from page instructions.
//!
//! An address is judged as Chrome would load it (WHATWG, ADR 0002), where Python used `urlsplit`, `urljoin` and IDNA
//! 2003. The cases where the two disagree are pinned in the tests below.

use indexmap::IndexSet;
use url::{Host, Url};

use crate::origins::origin_of;
use crate::py::text;

const SENTENCE_END: &[char] = &['.', ',', ';', ':', '\'', '\u{2019}', '\u{201d}', '\u{2026}'];
const QUESTION_END: &[char] = &['.', ',', ';', ':', '\'', '\u{2019}', '\u{201d}', '\u{2026}', '!', '?'];

/// What ends an address written in text: space, `<`, `>`, `"` and a backtick.
fn ends_address(c: char) -> bool {
    text::is_space(c) || matches!(c, '<' | '>' | '"' | '`')
}

/// The address a match at `start` names, without the sentence punctuation or unmatched wrapper that closes around it.
fn trimmed<'a>(task: &str, start: usize, found: &'a str, sentence_end: &[char]) -> &'a str {
    let mut url = found.trim_end_matches(sentence_end);
    for marker in ["**", "__", "*", "_"] {
        if task[..start].ends_with(marker) && url.ends_with(marker) {
            url = &url[..url.len() - marker.len()];
            break;
        }
    }
    for (left, right) in [('(', ')'), ('[', ']'), ('{', '}')] {
        while url.ends_with(right) && url.matches(right).count() > url.matches(left).count() {
            url = &url[..url.len() - 1];
        }
    }
    url
}

/// `re.sub(r"([,;])(?=https?://)", r"\1 ", task, flags=re.IGNORECASE)`: a list written without spaces still splits.
fn space_after_separators(task: &str) -> String {
    let mut out = String::with_capacity(task.len());
    for (at, c) in task.char_indices() {
        out.push(c);
        if matches!(c, ',' | ';') && scheme_length(&task[at + 1..]).is_some() {
            out.push(' ');
        }
    }
    out
}

/// The length of `https?://` at the start of `text`, ignoring case.
fn scheme_length(text: &str) -> Option<usize> {
    let mut at = text::starts_with_ignore_case(text, "http")?;
    if text[at..].starts_with(['s', 'S']) {
        at += 1;
    }
    text[at..].starts_with("://").then_some(at + 3)
}

/// Every `https?://[^\s<>"`]+` in `task`, ignoring case, as `(start, text)`.
fn absolute_addresses(task: &str) -> Vec<(usize, &str)> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < task.len() {
        if let Some(scheme) = scheme_length(&task[at..]) {
            let body = &task[at + scheme..];
            let length = body.find(ends_address).unwrap_or(body.len());
            if length > 0 {
                let end = at + scheme + length;
                found.push((at, &task[at..end]));
                at = end;
                continue;
            }
        }
        at += task[at..].chars().next().map_or(1, char::len_utf8);
    }
    found
}

/// A path the task names on its own, such as "sign in at /login": `(?<![^\s"'`(\[<*_])/[A-Za-z0-9_~-][^\s<>"`]*`.
///
/// It must open a word, so a fraction, a rate or "and/or" is not one, and its first character must not be a slash, so
/// it never names another host. Its body is read like an absolute address's, so "/wiki/Example_(topic)" or
/// "/search?tags=red,blue" is not cut short.
fn task_paths(task: &str) -> Vec<(usize, &str)> {
    let mut found = Vec::new();
    let mut previous: Option<char> = None;
    let mut chars = task.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let opens =
            previous.is_none_or(|p| text::is_space(p) || matches!(p, '"' | '\'' | '`' | '(' | '[' | '<' | '*' | '_'));
        let first = task[at + 1..].chars().next();
        if c == '/' && opens && first.is_some_and(|f| f.is_ascii_alphanumeric() || matches!(f, '_' | '~' | '-')) {
            let body = &task[at + 1..];
            let length = body.find(ends_address).unwrap_or(body.len());
            let end = at + 1 + length;
            found.push((at, &task[at..end]));
            while chars.peek().is_some_and(|(next, _)| *next < end) {
                previous = chars.next().map(|(_, c)| c);
            }
            continue;
        }
        previous = Some(c);
    }
    found
}

/// The addresses a task names, in order, each once: written out in full, or as a path on the caller's `start`.
pub fn task_urls(task: &str, start: Option<&str>, include_start: bool) -> Vec<String> {
    let mut urls: IndexSet<String> = IndexSet::new();
    let task = space_after_separators(task).replace("](", "] (");
    for (at, found) in absolute_addresses(&task) {
        let url = trimmed(&task, at, found, SENTENCE_END);
        if valid_address(url) {
            urls.insert(url.to_string());
        }
    }
    if let Some(start) = start.filter(|start| valid_address(start)) {
        // An entry page with nothing on it left a run told to "sign in at /login" with no address to open, so it
        // stopped stuck. The caller gave the path for the site they started the run on, so it resolves there.
        if let Ok(base) = Url::parse(start) {
            for (at, found) in task_paths(&task) {
                // A path asked about in a question ends the sentence with "?", which an address does not.
                let path = trimmed(&task, at, found, QUESTION_END);
                if let Ok(joined) = base.join(path)
                    && valid_address(joined.as_str())
                {
                    urls.insert(joined.into());
                }
            }
        }
        if include_start {
            urls.insert(start.to_string());
        }
    }
    urls.into_iter().collect()
}

/// Eligible destinations, excluding the current document and its initial redirect alias.
pub fn navigation_urls(
    task: &str,
    current: &str,
    start: Option<&str>,
    start_landing: Option<&str>,
    include_start: bool,
) -> Vec<String> {
    task_urls(task, start, include_start)
        .into_iter()
        .filter(|url| {
            !same_address(url, current)
                && !matches!((start, start_landing), (Some(start), Some(landing))
                    if same_address(url, start) && same_address(landing, current))
        })
        .collect()
}

/// Whether the authority as written carries userinfo. WHATWG cannot tell `https://@host/` from `https://host/` once
/// parsed, and an address the caller wrote with credentials in it is never one to open.
fn writes_userinfo(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else { return false };
    let authority = rest.split(['/', '?', '#', '\\']).next().unwrap_or("");
    authority.contains('@')
}

/// An http(s) address with a host, no credentials and a usable port, as Chrome would parse it.
pub fn valid_address(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else { return false };
    if origin_of(url).is_none() || writes_userinfo(url) {
        return false;
    }
    let host_ok = match parsed.host() {
        Some(Host::Domain(domain)) => {
            !domain.is_empty() && domain.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        }
        Some(Host::Ipv4(_) | Host::Ipv6(_)) => true,
        None => false,
    };
    host_ok && parsed.port() != Some(0)
}

/// The path (`/` when empty), query and fragment, as the comparison reads them.
fn location(url: &str) -> (String, String, String) {
    match Url::parse(url) {
        Ok(parsed) => (
            if parsed.path().is_empty() { "/".into() } else { parsed.path().into() },
            parsed.query().unwrap_or("").into(),
            parsed.fragment().unwrap_or("").into(),
        ),
        Err(_) => (url.into(), String::new(), String::new()),
    }
}

/// Whether two addresses open the same document: one origin, one path, query and fragment.
pub fn same_address(left: &str, right: &str) -> bool {
    origin_of(left) == origin_of(right) && location(left) == location(right)
}

#[cfg(test)]
mod tests {
    //! The task_urls and navigation_urls cases of tests/test_navigation.py; the rest drive the policy and the agent.
    use super::*;

    const ALPHA: &str = "https://alpha.example.test/project";
    const BETA: &str = "https://beta.example.test/project?tab=issues#top";
    const HERE: &str = "https://example.test/";
    const ENTRY: &str = "https://example.test/entry";

    fn urls(task: &str) -> Vec<String> {
        task_urls(task, None, true)
    }

    #[test]
    fn test_task_urls_extracts_literal_urls_in_order_and_dedupes() {
        assert_eq!(urls(&format!("Visit {ALPHA}, then {BETA}, then {ALPHA} again")), [ALPHA, BETA]);
    }

    #[test]
    fn test_task_urls_strips_wrapping_and_punctuation_but_keeps_query_and_fragment() {
        for (task, expected) in [
            (format!("See <{ALPHA}>"), vec![ALPHA]),
            (format!("See [the project]({ALPHA})"), vec![ALPHA]),
            (format!("See [{ALPHA}]({ALPHA})"), vec![ALPHA]),
            (format!("See ({ALPHA})."), vec![ALPHA]),
            (format!("Open {ALPHA}."), vec![ALPHA]),
            (format!("Open {ALPHA}, please"), vec![ALPHA]),
            (format!("Compare **{ALPHA}** and __{BETA}__"), vec![ALPHA, BETA]),
            (format!("Compare {ALPHA},{BETA}"), vec![ALPHA, BETA]),
            (format!("Compare {ALPHA};{BETA}"), vec![ALPHA, BETA]),
            (
                "Read https://a.example.test/wiki/Example_(topic).".into(),
                vec!["https://a.example.test/wiki/Example_(topic)"],
            ),
            ("Read https://a.example.test/path_".into(), vec!["https://a.example.test/path_"]),
            ("Open https://a.example.test/x?q=1&r=2#frag now".into(), vec!["https://a.example.test/x?q=1&r=2#frag"]),
        ] {
            assert_eq!(urls(&task), expected, "{task}");
        }
    }

    #[test]
    fn test_task_urls_rejects_unsafe_or_invalid_urls() {
        for task in [
            "Open ftp://example.test/file",
            "Open javascript:alert(1)",
            "Open file:///etc/passwd",
            "Open data:text/html,hi",
            "Open https://user:pass@example.test/",
            "Open https://user@example.test/",
            "Open https://@example.test/",
            "Open https://example.test:99999/",
            "Open https://example.test:notaport/",
            "Open https://example.test:0/",
            "Open https://bad_host!.test/",
            "Open example.test without a scheme",
            "No links here",
        ] {
            assert_eq!(urls(task), Vec::<String>::new(), "{task}");
        }
    }

    #[test]
    fn test_a_path_named_in_the_task_is_an_address_on_the_caller_start_site() {
        for (task, expected) in [
            ("Sign in at /login as a@example.test".to_string(), vec!["https://example.test/login".to_string()]),
            (
                "Open `/account/orders?page=2`, then read it.".into(),
                vec!["https://example.test/account/orders?page=2".into()],
            ),
            ("Sign in at (/login).".into(), vec!["https://example.test/login".into()]),
            (format!("Open /login, then {ALPHA}"), vec![ALPHA.to_string(), "https://example.test/login".into()]),
            ("Read /wiki/Example_(topic).".into(), vec!["https://example.test/wiki/Example_(topic)".into()]),
            ("Open /search?tags=red,blue and report".into(), vec!["https://example.test/search?tags=red,blue".into()]),
            ("Sign in at /login.".into(), vec!["https://example.test/login".into()]),
            ("Sign in at the form (see /login)".into(), vec!["https://example.test/login".into()]),
            ("Sign in at **/login** now".into(), vec!["https://example.test/login".into()]),
            (
                "Open /login?next=/settings#top, then report".into(),
                vec!["https://example.test/login?next=/settings#top".into()],
            ),
        ] {
            assert_eq!(navigation_urls(&task, ENTRY, Some(ENTRY), None, false), expected, "{task}");
        }
    }

    #[test]
    fn test_text_that_is_not_another_path_on_the_site_names_no_address() {
        for (task, expected) in [
            ("Open it 24/7 and/or tell me 1/2 of the price in $/kg".to_string(), vec![]),
            ("Read the / page".into(), vec![]),
            ("Open //other.example.test/login".into(), vec![]),
            ("Read the review at /entry".into(), vec![]),
            (format!("Open {ALPHA}/issues"), vec![format!("{ALPHA}/issues")]),
        ] {
            assert_eq!(navigation_urls(&task, ENTRY, Some(ENTRY), None, false), expected, "{task}");
        }
    }

    #[test]
    fn test_a_path_names_no_address_without_a_caller_start() {
        assert!(navigation_urls("Sign in at /login", ENTRY, None, None, false).is_empty());
    }

    #[test]
    fn test_explicit_start_is_available_without_repeating_it_in_task() {
        assert_eq!(task_urls("Fill the form and return", Some(ALPHA), true), [ALPHA]);
    }

    #[test]
    fn test_structured_start_preserves_url_punctuation() {
        for start in [format!("{ALPHA}."), format!("{ALPHA})"), format!("{ALPHA}_"), format!("{ALPHA}?q=1,2;")] {
            assert_eq!(task_urls("Return to the initial page", Some(&start), true), std::slice::from_ref(&start));
        }
    }

    #[test]
    fn test_a_path_written_in_the_task_is_an_address_on_the_start_site() {
        let task = "Sign in at /login as grace@example.com, and/or read 3/4 of https://alpha.example/a/b.";
        assert_eq!(
            task_urls(task, Some("http://shop.example/entry"), true),
            ["https://alpha.example/a/b", "http://shop.example/login", "http://shop.example/entry"]
        );
        assert_eq!(urls(task), ["https://alpha.example/a/b"]);
    }

    #[test]
    fn test_a_path_in_the_task_keeps_its_query_and_fragment() {
        let task = "Open /login?next=/settings#top, then report the plan.";
        assert_eq!(
            task_urls(task, Some("http://shop.example/entry"), true),
            ["http://shop.example/login?next=/settings#top", "http://shop.example/entry"]
        );
    }

    #[test]
    fn the_current_page_and_the_start_alias_are_not_destinations() {
        assert!(navigation_urls(&format!("Compare these projects: {HERE}"), HERE, None, None, false).is_empty());
        assert!(same_address("HTTPS://EXAMPLE.TEST:443", HERE));
        assert!(!same_address("https://example.test/#details", HERE));
        let task = "Fill the form and return to https://example.test/";
        let left =
            navigation_urls(task, "https://example.test/form", Some(HERE), Some("https://example.test/form"), false);
        assert!(left.is_empty());
    }

    /// Where WHATWG and `urlsplit` disagree, the address is the one Chrome opens.
    #[test]
    fn an_address_is_judged_as_chrome_parses_it() {
        // Extra slashes before the host are skipped: Chrome opens https://nohost/. Python found no host.
        assert_eq!(urls("Open https:///nohost"), ["https:///nohost"]);
        // An empty label is a host WHATWG keeps; IDNA 2003 refused it.
        assert_eq!(urls("Open https://a..example.test/"), ["https://a..example.test/"]);
        // A path joined onto the start is percent-encoded as Chrome writes it; urljoin kept the raw letters.
        assert_eq!(task_urls("Open /caf\u{e9}", Some(ENTRY), false), ["https://example.test/caf%C3%A9"]);
        // One address written two ways is one document.
        assert!(same_address("https://example.test/a b", "https://example.test/a%20b"));
    }
}
