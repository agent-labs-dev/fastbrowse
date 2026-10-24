mod common;

use common::{harness, sites};

#[tokio::test(flavor = "multi_thread")]
async fn observes_the_fixture_page_with_its_cross_origin_frame() {
    let Some(h) = harness().await else { return };
    h.goto("/index.html").await;
    let observation = h.observe().await;
    println!("{}", serde_json::to_string_pretty(&observation).unwrap());
    assert!(observation.url.starts_with(&sites().main));
    assert!(!observation.controls.is_empty());
    let capture = h.page.capture().await.expect("capture");
    println!("{}", capture.text);
    h.close().await;
}
