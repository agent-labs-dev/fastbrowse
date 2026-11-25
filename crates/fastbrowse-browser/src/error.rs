//! The browser layer's failures: `page.py`'s `BrowserError` family, which fastbrowse-core owns as
//! [`fastbrowse_core::page::BrowserError`], plus the subclasses only `browser/` raises and tells apart.

use std::fmt;

use fastbrowse_core::models::{FailureKind, MaybeUnavailable};
use fastbrowse_core::page::BrowserError as PageError;
pub use fastbrowse_core::page::Failure;

/// Every browser failure, with a message safe to put in a run result: never page text.
///
/// The first six are core's variants. The rest are `session.py`'s `BrowserUnresponsive`, `PageUnresponsive` and
/// `OriginNotAllowed` and `page.py`'s `_ScriptError`, which this crate matches on; above the `Page` seam they become
/// the core variant their Python base class is, except `PageUnresponsive`, which ends the run `unavailable`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserError {
    Failed(Failure),
    /// CDP's -32000: the document a command addressed was replaced while it ran.
    ContextLost(String),
    /// `BrowserUnavailable`: the session has no page left for this run.
    Unavailable(String),
    ScreenshotsUnavailable(String),
    NavigationTimeout(Failure),
    SiteUnreachable(Failure),
    /// The browser stopped answering: an outage, so `Unavailable` above the seam.
    BrowserUnresponsive(String),
    /// A tab that stopped answering while the browser behind it still answers.
    PageUnresponsive(Failure),
    /// A document outside `allowed_origins`. The message never names it: an address can carry a credential.
    OriginNotAllowed,
    /// A script this layer evaluated threw in the page. Holds where it threw (the script's line and column), never the
    /// exception's message, which a page can write: the failure says which branch threw.
    Script(String),
}

impl BrowserError {
    pub fn failed(reason: impl Into<String>, message: impl Into<String>) -> BrowserError {
        BrowserError::Failed(Failure::new(reason, message))
    }

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
            Self::Script(_) => "_ScriptError",
        }
    }
}

impl fmt::Display for BrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Failed(failure)
            | Self::NavigationTimeout(failure)
            | Self::SiteUnreachable(failure)
            | Self::PageUnresponsive(failure) => f.write_str(&failure.message),
            Self::ContextLost(m)
            | Self::Unavailable(m)
            | Self::ScreenshotsUnavailable(m)
            | Self::BrowserUnresponsive(m) => f.write_str(m),
            Self::OriginNotAllowed => f.write_str("the page is outside allowed_origins"),
            Self::Script(detail) => write!(f, "Runtime.evaluate failed (JavaScriptError): {detail}"),
        }
    }
}

impl std::error::Error for BrowserError {}

impl FailureKind for BrowserError {
    fn failure_kind(&self) -> String {
        match self {
            Self::BrowserUnresponsive(_) => "BrowserUnresponsive".into(),
            Self::OriginNotAllowed => "OriginNotAllowed".into(),
            Self::Script(_) => "Script".into(),
            other => PageError::from(other.clone()).failure_kind(),
        }
    }
}

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
            // Kept apart so the agent can reopen a page that hung during an action once; any other, and a second,
            // ends the run `unavailable` rather than retrying a dead page (#737).
            BrowserError::PageUnresponsive(failure) => PageError::PageUnresponsive(failure),
            BrowserError::ScreenshotsUnavailable(_) => PageError::ScreenshotsUnavailable(message),
            BrowserError::NavigationTimeout(failure) => PageError::NavigationTimeout(failure),
            BrowserError::SiteUnreachable(failure) => PageError::SiteUnreachable(failure),
            BrowserError::Failed(failure) => PageError::Failed(failure),
            BrowserError::OriginNotAllowed => PageError::failed("OriginNotAllowed", message),
            BrowserError::Script(_) => PageError::failed("Runtime.evaluate/JavaScriptError", message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_error_crosses_the_seam_as_the_core_variant_it_ends_the_run_as() {
        let cases = [
            (BrowserError::failed("r", "f"), PageError::failed("r", "f")),
            (BrowserError::ContextLost("c".into()), PageError::ContextLost("c".into())),
            (BrowserError::BrowserUnresponsive("b".into()), PageError::Unavailable("b".into())),
            (
                BrowserError::PageUnresponsive(Failure::new("r", "p")),
                PageError::PageUnresponsive(Failure::new("r", "p")),
            ),
            (
                BrowserError::OriginNotAllowed,
                PageError::failed("OriginNotAllowed", "the page is outside allowed_origins"),
            ),
            (BrowserError::SiteUnreachable(Failure::new("r", "s")), PageError::SiteUnreachable(Failure::new("r", "s"))),
            (
                BrowserError::Script("thrown at line 3, column 7".into()),
                PageError::failed(
                    "Runtime.evaluate/JavaScriptError",
                    "Runtime.evaluate failed (JavaScriptError): thrown at line 3, column 7",
                ),
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(PageError::from(error), expected);
        }
    }
}
