//! Port of `src/fastbrowse/addresses.py`: an address read for the search state a page keeps in it, its query and its
//! fragment alike.
//!
//! Everything that asks whether two addresses name the same search reads them through here. Lineage once dropped the
//! fragment while the search check kept it, so on a site routing its searches by fragment a corrected `/search#new`
//! matched a guessed `/search#old` and stayed guessed.

use url::Url;

/// The address split as Chrome parses it, or `None` for text that is not an absolute address.
pub fn search_parts(url: &str) -> Option<Url> {
    Url::parse(url).ok()
}

/// The address with its query and fragment, normalized as `search_parts` parses it. Text that does not parse is
/// returned as it is, so two such texts compare as written.
pub fn search_address(url: &str) -> String {
    search_parts(url).map_or_else(|| url.to_string(), String::from)
}

/// Whether the address carries search state, in its query or its fragment.
pub fn searched(url: &str) -> bool {
    match search_parts(url) {
        Some(parsed) => {
            parsed.query().is_some_and(|q| !q.is_empty()) || parsed.fragment().is_some_and(|f| !f.is_empty())
        }
        // A relative or broken address still says what it carries after `?` or `#`.
        None => {
            let (rest, fragment) = url.split_once('#').unwrap_or((url, ""));
            !fragment.is_empty() || rest.split_once('?').is_some_and(|(_, query)| !query.is_empty())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_state_is_the_query_or_the_fragment() {
        assert!(searched("https://example.test/search?q=kettle"));
        assert!(searched("https://example.test/search#q=kettle"));
        assert!(!searched("https://example.test/search"));
        assert!(!searched("https://example.test/search?#"));
        assert!(searched("/search?q=1"));
    }

    /// WHATWG normalizes what `urlsplit` kept as written, and keeps an empty `?` that `geturl` dropped.
    #[test]
    fn a_search_address_is_written_as_chrome_writes_it() {
        assert_eq!(search_address("HTTPS://Example.TEST/search?q=a b"), "https://example.test/search?q=a%20b");
        assert_eq!(search_address("https://example.test/search?"), "https://example.test/search?");
        assert_eq!(search_address("https://example.test"), "https://example.test/");
        assert_eq!(search_address("not an address"), "not an address");
    }
}
