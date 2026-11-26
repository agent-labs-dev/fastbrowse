//! A search view keeps the existing page alive while real result links are chosen.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fastbrowse_clients::search::{Exa, SEARCH_DOLLARS};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::decision::Edges;
use fastbrowse_core::models::{Artifact, Operation, StepFailure, StepOutcome};
use fastbrowse_core::page::{
    ActResult, Action, BrowserError, CaptchaSolve, Capture, Control, Observation, Page, PageResult, SearchOutcome,
    Unread,
};

use super::{BlockKind, FetchedPage, Found, capture, eligible};

pub struct SearchPage {
    inner: Arc<dyn Page>,
    results: FetchedPage,
    provider: Exa,
    active: AtomicBool,
}

impl SearchPage {
    pub fn new(inner: Arc<dyn Page>, results: FetchedPage, provider: Exa) -> Self {
        Self { inner, results, provider, active: AtomicBool::new(false) }
    }

    fn view(&self) -> &dyn Page {
        if self.active.load(Ordering::Relaxed) { &self.results } else { &*self.inner }
    }

    pub fn needed_browser(&self) -> bool {
        self.results.needed_browser()
    }
}

impl Page for SearchPage {
    fn search_price(&self) -> Option<f64> {
        Some(SEARCH_DOLLARS)
    }

    fn search<'a>(&'a self, query: &'a str, seconds: f64) -> BoxFuture<'a, SearchOutcome> {
        Box::pin(async move {
            if let Err(error) = self.results.gate().await {
                return SearchOutcome {
                    cost: fastbrowse_core::models::CostLine::new(
                        fastbrowse_core::models::CostComponent::Search,
                        fastbrowse_core::models::CostBasis::Metered,
                        Some(0.0),
                    ),
                    error: Some(error.to_string()),
                    page_changed: false,
                };
            }
            match self.provider.search(query, seconds).await {
                Err(failure) => {
                    SearchOutcome { cost: *failure.cost, error: Some(failure.message), page_changed: false }
                }
                Ok(response) => {
                    let mut controls = Vec::new();
                    let blocks = response
                        .results
                        .iter()
                        .enumerate()
                        .map(|(index, result)| {
                            let href = result
                                .get("url")
                                .and_then(serde_json::Value::as_str)
                                .filter(|url| eligible(url, None).is_ok());
                            if let Some(url) = href {
                                let title = result.get("title").and_then(serde_json::Value::as_str).unwrap_or(url);
                                let mut control =
                                    Control::new(&format!("search/{index}"), "link", title, [Operation::Click]);
                                control.href = Some(url.into());
                                controls.push(control);
                            }
                            Found {
                                text: result.to_string(),
                                kind: BlockKind::Record,
                                frame_id: None,
                                heading_path: vec![format!("Search results for {query}")],
                                href: href.map(str::to_owned),
                                links: Vec::new(),
                                whole_record: true,
                                item_place: None,
                                fields: Vec::new(),
                                advice: Vec::new(),
                            }
                        })
                        .collect();
                    let capture = capture("https://api.exa.ai/search", format!("Web search: {query}"), blocks, 0);
                    let mut loaded = self.results.loaded.lock().unwrap();
                    let page_changed = !self.active.load(Ordering::Relaxed)
                        || loaded.capture.as_ref().is_none_or(|previous| previous.sha256 != capture.sha256);
                    loaded.capture = Some(capture);
                    loaded.requested = None;
                    loaded.controls = controls;
                    loaded.scripts = false;
                    loaded.generation += 1;
                    drop(loaded);
                    // Search observations describe the results, not any secret still visible in the retained browser.
                    self.inner.withhold_frames(true);
                    self.active.store(true, Ordering::Relaxed);
                    SearchOutcome { cost: response.cost, error: None, page_changed }
                }
            }
        })
    }

    fn over_http(&self) -> bool {
        !self.active.load(Ordering::Relaxed) && self.inner.over_http()
    }
    fn unread(&self) -> Option<Unread> {
        self.view().unread()
    }
    fn needs_browser(&self, message: String) -> BrowserError {
        self.inner.needs_browser(message)
    }
    fn browser_address(&self) -> Option<String> {
        if self.active.load(Ordering::Relaxed) {
            self.results.browser_address().or_else(|| self.inner.browser_address())
        } else {
            self.inner.browser_address()
        }
    }
    fn artifacts(&self) -> Vec<Artifact> {
        self.inner.artifacts()
    }
    fn keep_capture<'a>(&'a self, name: &'a str, text: &'a str) -> BoxFuture<'a, PageResult<Option<Artifact>>> {
        self.inner.keep_capture(name, text)
    }
    fn keep_screenshot<'a>(&'a self, name: &'a str, png: &'a [u8]) -> BoxFuture<'a, PageResult<Option<Artifact>>> {
        self.inner.keep_screenshot(name, png)
    }
    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        self.view().observe()
    }
    fn glance(&self) -> BoxFuture<'_, PageResult<Observation>> {
        self.view().glance()
    }
    fn document_changed<'a>(&'a self, observation: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        self.view().document_changed(observation)
    }
    fn navigate<'a>(&'a self, url: &'a str, back_to: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        Box::pin(async move {
            self.results.loaded.lock().unwrap().requested = Some(url.to_owned());
            self.inner.navigate(url, back_to).await?;
            self.active.store(false, Ordering::Relaxed);
            Ok(())
        })
    }
    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        self.view().capture()
    }
    fn act<'a>(&'a self, action: &'a Action, observation: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        Box::pin(async move {
            if !self.active.load(Ordering::Relaxed) {
                return self.inner.act(action, observation).await;
            }
            self.results.gate().await?;
            if self.results.document_changed(observation).await? {
                return Ok(ActResult {
                    outcome: StepOutcome::Stale,
                    failure: None,
                    page_changed: false,
                    detail: Some("search results changed since observation".into()),
                    moved: false,
                    landed: None,
                });
            }
            let url = match action.operation {
                Operation::Navigate => action.url.clone(),
                Operation::Click => action.target_id.as_deref().and_then(|id| self.results.link_target(id)),
                _ => None,
            };
            let Some(url) = url.filter(|url| eligible(url, None).is_ok()) else {
                return Ok(ActResult {
                    outcome: StepOutcome::Failed,
                    failure: Some(StepFailure::BrowserError),
                    page_changed: false,
                    detail: Some("Search results are fixed data. Open a result link or choose another search.".into()),
                    moved: false,
                    landed: None,
                });
            };
            self.results.loaded.lock().unwrap().requested = Some(url.clone());
            let navigation = Action { url: Some(url), ..Action::new(Operation::Navigate) };
            let retained = if self.inner.over_http() { observation.clone() } else { self.inner.observe().await? };
            let result = self.inner.act(&navigation, &retained).await?;
            if result.outcome == StepOutcome::Executed {
                self.active.store(false, Ordering::Relaxed);
            }
            Ok(result)
        })
    }
    fn wait<'a>(
        &'a self,
        action: &'a Action,
        observation: &'a Observation,
        seconds: f64,
    ) -> BoxFuture<'a, PageResult<ActResult>> {
        if self.active.load(Ordering::Relaxed) {
            self.act(action, observation)
        } else {
            self.inner.wait(action, observation, seconds)
        }
    }
    fn redrawn<'a>(
        &'a self,
        observation: &'a Observation,
        seconds: f64,
        id: Option<&'a str>,
    ) -> BoxFuture<'a, PageResult<bool>> {
        self.view().redrawn(observation, seconds, id)
    }
    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        if self.active.load(Ordering::Relaxed) {
            Box::pin(async {
                Err(BrowserError::ScreenshotsUnavailable(
                    "Search results have no rendered viewport. Open a result link to inspect its page.".into(),
                ))
            })
        } else {
            self.inner.screenshot()
        }
    }
    fn reveal<'a>(&'a self, id: &'a str) -> BoxFuture<'a, PageResult<()>> {
        self.view().reveal(id)
    }
    fn edges<'a>(&'a self, id: &'a str) -> BoxFuture<'a, PageResult<Option<Edges>>> {
        self.view().edges(id)
    }
    fn click_viewport<'a>(&'a self, x: f64, y: f64, within: Option<&'a str>) -> BoxFuture<'a, PageResult<ActResult>> {
        self.view().click_viewport(x, y, within)
    }
    fn withhold_frames(&self, withheld: bool) {
        self.inner.withhold_frames(withheld || self.active.load(Ordering::Relaxed));
    }
    fn hold_images(&self) {
        self.inner.hold_images();
    }
    fn hand_over_tab(&self) {
        self.inner.hand_over_tab();
    }
    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        self.view().address()
    }
    fn active_url(&self) -> BoxFuture<'_, PageResult<Option<String>>> {
        self.view().active_url()
    }
    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>> {
        self.view().response_status()
    }
    fn lifecycle_mark(&self) -> u64 {
        self.view().lifecycle_mark()
    }
    fn wait_for_lifecycle(&self, mark: u64, seconds: f64) -> BoxFuture<'_, bool> {
        self.view().wait_for_lifecycle(mark, seconds)
    }
    fn captcha_solve(&self) -> Option<CaptchaSolve> {
        self.view().captcha_solve()
    }
    fn loading(&self) -> Option<f64> {
        self.view().loading()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastbrowse_clients::environment::Secret;
    use fastbrowse_clients::http::{Http, Request, Response, TransportError};

    #[derive(Default)]
    struct RetainedBrowser {
        frames_withheld: AtomicBool,
        surface_revealed: AtomicBool,
    }

    impl Page for RetainedBrowser {
        fn artifacts(&self) -> Vec<Artifact> {
            Vec::new()
        }
        fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
            Box::pin(async {
                Ok(serde_json::from_value(serde_json::json!({
                    "url":"https://retained.example/", "title":"Retained", "page_key":"retained",
                    "captured_at":"2026-11-12T00:00:00Z", "controls":[], "viewport_text":"Retained", "tabs":[]
                }))
                .unwrap())
            })
        }
        fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
            Box::pin(async { unreachable!() })
        }
        fn navigate<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
            Box::pin(async { Ok(()) })
        }
        fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
            Box::pin(async { unreachable!() })
        }
        fn act<'a>(&'a self, action: &'a Action, observation: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
            assert_eq!(action.operation, Operation::Navigate);
            assert_eq!(observation.page_key, "retained");
            Box::pin(async {
                Ok(ActResult {
                    outcome: StepOutcome::Failed,
                    failure: Some(StepFailure::BrowserError),
                    page_changed: false,
                    detail: Some("retained navigation check".into()),
                    moved: false,
                    landed: None,
                })
            })
        }
        fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
            Box::pin(async { unreachable!() })
        }
        fn reveal<'a>(&'a self, id: &'a str) -> BoxFuture<'a, PageResult<()>> {
            Box::pin(async move {
                assert_eq!(id, "drawing-pad");
                self.surface_revealed.store(true, Ordering::Relaxed);
                Ok(())
            })
        }
        fn edges<'a>(&'a self, id: &'a str) -> BoxFuture<'a, PageResult<Option<Edges>>> {
            Box::pin(async move {
                assert_eq!(id, "drawing-pad");
                Ok(self.surface_revealed.load(Ordering::Relaxed).then_some(Edges {
                    left: 0.1,
                    top: 0.1,
                    right: 0.5,
                    bottom: 0.5,
                }))
            })
        }
        fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
            Box::pin(async { unreachable!() })
        }
        fn withhold_frames(&self, withheld: bool) {
            self.frames_withheld.store(withheld, Ordering::Relaxed);
        }
        fn address(&self) -> BoxFuture<'_, PageResult<String>> {
            Box::pin(async { unreachable!() })
        }
        fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>> {
            Box::pin(async { unreachable!() })
        }
        fn lifecycle_mark(&self) -> u64 {
            0
        }
        fn wait_for_lifecycle(&self, _: u64, _: f64) -> BoxFuture<'_, bool> {
            Box::pin(async { unreachable!() })
        }
    }

    #[tokio::test]
    async fn search_enabled_browser_reveals_the_surface_before_measuring_it() {
        let http = Http::new(Arc::new(|_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
            Box::pin(async { panic!("preparing a browser gesture should make no search request") })
        }));
        let inner = Arc::new(RetainedBrowser::default());
        let page = SearchPage::new(
            inner.clone(),
            FetchedPage::new(http.clone(), None, None),
            Exa::new(http, Secret::new("key")),
        );
        page.reveal("drawing-pad").await.unwrap();
        assert!(inner.surface_revealed.load(Ordering::Relaxed));
        assert!(page.edges("drawing-pad").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn observing_search_results_cannot_release_the_retained_browsers_secret_frames() {
        let http = Http::new(Arc::new(|_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
            Box::pin(async {
                Ok(Response::json(200, &serde_json::json!({"results":[],"costDollars":{"total":0.007}})))
            })
        }));
        let inner = Arc::new(RetainedBrowser { frames_withheld: AtomicBool::new(true), ..RetainedBrowser::default() });
        let page = SearchPage::new(
            inner.clone(),
            FetchedPage::new(http.clone(), None, None),
            Exa::new(http, Secret::new("key")),
        );
        assert!(page.search("question", 30.0).await.error.is_none());
        page.observe().await.unwrap();
        page.withhold_frames(false);
        assert!(inner.frames_withheld.load(Ordering::Relaxed));
        page.navigate("https://example.org/", None).await.unwrap();
        assert!(inner.frames_withheld.load(Ordering::Relaxed));
        page.withhold_frames(false);
        assert!(!inner.frames_withheld.load(Ordering::Relaxed));
    }
    /// Search results are code's own page, with no script: the unrun scripts of the document read before the search
    /// are that document's alone, and come back with it (#1380).
    #[tokio::test]
    async fn search_results_report_none_of_the_previous_documents_unrun_scripts() {
        let http = Http::new(Arc::new(|request: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
            let response = if request.url.contains("exa.ai") {
                Response::json(200, &serde_json::json!({"results":[],"costDollars":{"total":0.007}}))
            } else {
                Response::new(200, "<p>Text</p><script src=/s.js></script>").with_header("content-type", "text/html")
            };
            Box::pin(async move { Ok(response) })
        }));
        let inner = Arc::new(FetchedPage::new(http.clone(), None, None));
        let page =
            SearchPage::new(inner, FetchedPage::new(http.clone(), None, None), Exa::new(http, Secret::new("key")));
        page.navigate("https://docs.example.test/", None).await.unwrap();
        assert!(page.unread().is_some_and(|unread| unread.scripts));
        assert!(page.search("question", 30.0).await.error.is_none());
        assert_eq!(page.unread(), None);
        page.navigate("https://docs.example.test/", None).await.unwrap();
        assert!(page.unread().is_some_and(|unread| unread.scripts));
    }

    #[tokio::test]
    async fn search_navigation_preserves_the_retained_pages_failed_step_checks() {
        let http = Http::new(Arc::new(|_: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
            Box::pin(async {
                Ok(Response::json(200, &serde_json::json!({"results":[],"costDollars":{"total":0.007}})))
            })
        }));
        let inner = Arc::new(RetainedBrowser::default());
        let page = SearchPage::new(
            inner.clone(),
            FetchedPage::new(http.clone(), None, None),
            Exa::new(http, Secret::new("key")),
        );
        let navigation = Action { url: Some("https://source.example/".into()), ..Action::new(Operation::Navigate) };
        let underlying = inner.observe().await.unwrap();
        assert_eq!(
            page.act(&navigation, &underlying).await.unwrap().detail.as_deref(),
            Some("retained navigation check")
        );
        page.search("question", 30.0).await;
        assert!(!page.over_http());
        assert!(matches!(page.screenshot().await, Err(BrowserError::ScreenshotsUnavailable(_))));
        assert!(!page.needed_browser());
        let results = page.observe().await.unwrap();
        assert_eq!(page.act(&navigation, &results).await.unwrap().detail.as_deref(), Some("retained navigation check"));
        assert_eq!(page.act(&Action::new(Operation::Wait), &results).await.unwrap().outcome, StepOutcome::Failed);
        // Fixed results never change, so a WAIT on them is refused rather than handed to the browser behind them.
        let wait = page.wait(&Action::new(Operation::Wait), &results, f64::INFINITY).await.unwrap();
        assert_eq!(wait.outcome, StepOutcome::Failed);
        assert!(!page.needed_browser());
        page.search("another question", 30.0).await;
        assert!(page.browser_address().is_none());
    }
}
