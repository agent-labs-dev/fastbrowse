//! The browser layer's failures: `page.py`'s `BrowserError` family, which fastbrowse-core owns as
//! [`fastbrowse_core::page::BrowserError`], plus the subclasses only `browser/` raises and tells apart.

use std::fmt;

use fastbrowse_core::models::MaybeUnavailable;
use fastbrowse_core::page::BrowserError as PageError;

/// Every browser failure, with a message safe to put in a run result: never page text.
///
/// The first six are core's variants. The rest are `session.py`'s `BrowserUnresponsive`, `PageUnresponsive` and
/// `OriginNotAllowed` and `page.py`'s `_ScriptError`, which this crate matches on; above the `Page` seam they become
/// the core variant their Python base class is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserError {
    Failed(String),
    /// CDP's -32000: the document a command addressed was replaced while it ran.
    ContextLost(String),
    /// `BrowserUnavailable`: the session has no page left for this run.
    Unavailable(String),
    ScreenshotsUnavailable(String),
    NavigationTimeout(String),
    SiteUnreachable(String),
    /// The browser stopped answering: an outage, so `Unavailable` above the seam.
    BrowserUnresponsive(String),
    /// A tab that stopped answering while the browser behind it still answers.
    PageUnresponsive(String),
    /// A document outside `allowed_origins`. The message never names it: an address can carry a credential.
    OriginNotAllowed,
    /// A script this layer evaluated threw in the page.
    Script,
}

impl BrowserError {
    /// Whether Python's `isinstance(error, Unavailable)` holds: an outage, not a failed task.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_) | Self::BrowserUnresponsive(_))
    }

    pub fn is_context_lost(&self) -> bool {
        matches!(self, Self::ContextLost(_))
    }

    /// The Python class name, for logs and tests.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Failed(_) => "BrowserError",
            Self::ContextLost(_) => "ContextLost",
            Self::Unavailable(_) => "BrowserUnavailable",
            Self::ScreenshotsUnavailable(_) => "ScreenshotsUnavailable",
            Self::NavigationTimeout(_) => "NavigationTimeout",
            Self::SiteUnreachable(_) => "SiteUnreachable",
            Self::BrowserUnresponsive(_) => "BrowserUnresponsive",
            Self::PageUnresponsive(_) => "PageUnresponsive",
            Self::OriginNotAllowed => "OriginNotAllowed",
            Self::Script => "_ScriptError",
        }
    }
}

impl fmt::Display for BrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(m)
            | Self::ContextLost(m)
            | Self::Unavailable(m)
            | Self::ScreenshotsUnavailable(m)
            | Self::NavigationTimeout(m)
            | Self::SiteUnreachable(m)
            | Self::BrowserUnresponsive(m)
            | Self::PageUnresponsive(m) => f.write_str(m),
            Self::OriginNotAllowed => f.write_str("the page is outside allowed_origins"),
            Self::Script => f.write_str("Runtime.evaluate failed (JavaScriptError)"),
        }
    }
}

impl std::error::Error for BrowserError {}

impl MaybeUnavailable for BrowserError {
    fn is_unavailable(&self) -> bool {
        BrowserError::is_unavailable(self)
    }
}

impl From<BrowserError> for PageError {
    fn from(error: BrowserError) -> Self {
        let message = error.to_string();
        match error {
            BrowserError::ContextLost(_) => PageError::ContextLost(message),
            BrowserError::Unavailable(_) | BrowserError::BrowserUnresponsive(_) => PageError::Unavailable(message),
            BrowserError::ScreenshotsUnavailable(_) => PageError::ScreenshotsUnavailable(message),
            BrowserError::NavigationTimeout(_) => PageError::NavigationTimeout(message),
            BrowserError::SiteUnreachable(_) => PageError::SiteUnreachable(message),
            BrowserError::Failed(_)
            | BrowserError::PageUnresponsive(_)
            | BrowserError::OriginNotAllowed
            | BrowserError::Script => PageError::Failed(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_error_crosses_the_seam_as_its_python_base_class() {
        let cases = [
            (BrowserError::Failed("f".into()), PageError::Failed("f".into())),
            (BrowserError::ContextLost("c".into()), PageError::ContextLost("c".into())),
            (BrowserError::BrowserUnresponsive("b".into()), PageError::Unavailable("b".into())),
            (BrowserError::PageUnresponsive("p".into()), PageError::Failed("p".into())),
            (BrowserError::OriginNotAllowed, PageError::Failed("the page is outside allowed_origins".into())),
            (BrowserError::SiteUnreachable("s".into()), PageError::SiteUnreachable("s".into())),
        ];
        for (error, expected) in cases {
            assert_eq!(PageError::from(error), expected);
        }
    }
}
