use std::sync::Mutex;

use fastbrowse_core::BoxFuture;
use fastbrowse_core::models::{Artifact, Operation, StepOutcome};
use fastbrowse_core::page::{ActResult, Action, BrowserError, Capture, Control, Dialog, Observation, Page, PageResult};
use serde_json::json;

pub(crate) struct FormPage {
    pub(crate) page: Mutex<Observation>,
    pub(crate) confirms_post: bool,
}

impl Page for FormPage {
    fn artifacts(&self) -> Vec<Artifact> {
        Vec::new()
    }

    fn observe(&self) -> BoxFuture<'_, PageResult<Observation>> {
        let page = self.page.lock().unwrap().clone();
        Box::pin(async move { Ok(page) })
    }

    fn document_changed<'a>(&'a self, _: &'a Observation) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn navigate<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, PageResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn capture(&self) -> BoxFuture<'_, PageResult<Capture>> {
        let page = self.page.lock().unwrap().clone();
        Box::pin(async move {
            Ok(serde_json::from_value(json!({
                "url": page.url, "title": page.title, "captured_at": page.captured_at,
                "sha256": "empty", "text": "", "blocks": [],
            }))
            .unwrap())
        })
    }

    fn act<'a>(&'a self, action: &'a Action, _: &'a Observation) -> BoxFuture<'a, PageResult<ActResult>> {
        let mut page = self.page.lock().unwrap();
        match (action.operation, action.target_id.as_deref()) {
            (Operation::Wait, _) => {}
            (_, Some("choice")) => {
                page.controls[0].value = Some("One way".into());
                page.controls.retain(|control| control.id != "choice");
            }
            (Operation::Dialog, _) => {
                assert_eq!(action.accept_dialog, Some(true));
                page.dialog = None;
                page.viewport_text = "Comment posted".into();
            }
            (_, Some("draft")) => page.controls[0].checked = Some(true),
            (_, Some("post")) if self.confirms_post => {
                page.dialog =
                    Some(Dialog { kind: "confirm".into(), message: "Post this comment?".into(), default_prompt: None });
            }
            (_, Some("post")) => page.viewport_text = "Comment posted".into(),
            (_, Some("next")) => {
                page.url = "https://example.test/second".into();
                page.document_key = "second-document".into();
                page.controls = vec![Control {
                    form_id: Some("1".into()),
                    ..Control::new("post", "button", "Post", [Operation::Click])
                }];
            }
            other => panic!("unexpected action: {other:?}"),
        }
        Box::pin(async {
            Ok(ActResult {
                outcome: StepOutcome::Executed,
                failure: None,
                page_changed: true,
                detail: None,
                moved: false,
            })
        })
    }

    fn redrawn<'a>(&'a self, _: &'a Observation, _: f64, _: Option<&'a str>) -> BoxFuture<'a, PageResult<bool>> {
        Box::pin(async { Ok(true) })
    }

    fn screenshot(&self) -> BoxFuture<'_, PageResult<Vec<u8>>> {
        Box::pin(async { Err(BrowserError::ScreenshotsUnavailable("scripted page".into())) })
    }

    fn withhold_frames(&self, _: bool) {}

    fn address(&self) -> BoxFuture<'_, PageResult<String>> {
        let url = self.page.lock().unwrap().url.clone();
        Box::pin(async move { Ok(url) })
    }

    fn response_status(&self) -> BoxFuture<'_, PageResult<Option<i64>>> {
        Box::pin(async { Ok(Some(200)) })
    }

    fn lifecycle_mark(&self) -> u64 {
        0
    }

    fn wait_for_lifecycle(&self, _: u64, _: f64) -> BoxFuture<'_, bool> {
        Box::pin(async { true })
    }
}
