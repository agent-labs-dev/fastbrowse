//! A page read over HTTP has to produce the blocks a browser capture produces, or quotes break. `golden/fetch.json`
//! holds the blocks the reader makes of every fixture page.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fastbrowse_browser::session::CheckAccess;
use fastbrowse_clients::http::{Http, Request, Response, TransportError};
use fastbrowse_core::BoxFuture;
use fastbrowse_core::models::Operation;
use fastbrowse_core::page::{Action, BlockKind, BrowserError, Page};
use fastbrowse_core::py::sha256_hex;
use fastbrowse_run::fetch::html::blocks;
use fastbrowse_run::fetch::{FetchedPage, NotFetched, eligible, fetch_capture};
use serde_json::Value;

const PAGE: &str = r#"<!doctype html>
<html><head><title>Orders report</title>
<script>window.x = {"nested": "\"noise\""}</script>
<style>.a { color: red }</style></head>
<body>
<nav><a href="/">Home</a></nav>
<h1>Orders</h1>
<p>The total of the orders is <strong>251.00</strong>.</p>
<h2>Lines</h2>
<table>
  <thead><tr><th>Item</th><th>Amount</th></tr></thead>
  <tbody>
    <tr><td>Kettle</td><td>34.50</td></tr>
    <tr><td>Cups</td><td>216.50</td></tr>
  </tbody>
</table>
<ul><li>One</li><li>Two</li></ul>
<p>See the <a href="/policy">returns policy</a> for details.</p>
<a href="/download/report.csv">Download the report</a>
</body></html>"#;

fn serving(answer: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Http {
    let answer = Arc::new(answer);
    Http::new(Arc::new(move |request: Request| -> BoxFuture<'static, Result<Response, TransportError>> {
        let response = answer(&request);
        Box::pin(async move { Ok(response) })
    }))
}

fn client(body: &str, status: u16, content_type: &str) -> Http {
    let (body, content_type) = (body.to_string(), content_type.to_string());
    serving(move |_| Response::new(status, body.clone()).with_header("content-type", &content_type))
}

fn html(body: &str) -> Http {
    client(body, 200, "text/html; charset=utf-8")
}

async fn fetched(http: &Http, url: &str) -> Result<fastbrowse_core::page::Capture, NotFetched> {
    fetch_capture(http, url, None, 15.0).await
}

#[test]
fn test_blocks_keep_the_shapes_a_quote_needs() {
    let (_title, found) = blocks(PAGE, "https://shop.example.com/orders");
    let texts: Vec<(BlockKind, &str)> = found.iter().map(|b| (b.kind, b.text.as_str())).collect();
    assert!(texts.contains(&(BlockKind::Heading, "Orders")));
    // A value inside markup stays in the block of its label, which is what names it.
    assert!(texts.contains(&(BlockKind::Paragraph, "The total of the orders is 251.00.")));
    // Scripts state nothing a reader can quote, and a CSS rule is not page text.
    assert!(!texts.iter().any(|(_, text)| text.contains("window.x") || text.contains("color: red")));
    // Each row holds its cells in one line, under the header that names them.
    assert!(texts.contains(&(BlockKind::Table, "| Item | Amount |")));
    assert!(texts.contains(&(BlockKind::Table, "| --- | --- |")));
    assert!(texts.contains(&(BlockKind::Table, "| Kettle | 34.50 |")));
    assert!(texts.contains(&(BlockKind::ListItem, "One")));
    // A link's destination is kept where the link is the whole block.
    assert!(texts.contains(&(BlockKind::Paragraph, "See the returns policy for details.")));
    let links: Vec<(&str, Option<&str>)> =
        found.iter().filter(|b| b.kind == BlockKind::Link).map(|b| (b.text.as_str(), b.href.as_deref())).collect();
    assert_eq!(
        links,
        [
            ("Home", Some("https://shop.example.com/")),
            ("Download the report", Some("https://shop.example.com/download/report.csv")),
        ]
    );
}

#[test]
fn test_a_value_under_a_heading_carries_the_path_that_names_it() {
    let (_title, found) = blocks(PAGE, "https://shop.example.com/orders");
    let row = found.iter().find(|b| b.text == "| Kettle | 34.50 |").expect("row");
    assert_eq!(row.heading_path, ["Orders", "Lines"]);
}

#[tokio::test]
async fn test_a_fetched_page_is_a_capture_with_spans_and_a_hash() {
    let capture = fetched(&html(PAGE), "https://shop.example.com/orders").await.expect("fetched");
    assert_eq!(capture.title, "Orders report");
    for block in &capture.blocks {
        assert!(block.end > block.start);
    }
    assert!(capture.text.contains("The total of the orders is 251.00."));
    // A quote is validated against this hash, so it has to be the hash of exactly this text.
    assert_eq!(capture.sha256, sha256_hex(capture.text.as_bytes()));
}

#[tokio::test]
async fn test_plain_text_is_read_as_one_block() {
    let http = client("total,amount\nkettle,34.50\n", 200, "text/plain");
    let capture = fetched(&http, "https://shop.example.com/report.txt").await.expect("fetched");
    assert_eq!(capture.blocks.iter().map(|b| b.kind).collect::<Vec<_>>(), [BlockKind::Paragraph]);
}

#[tokio::test]
async fn test_a_page_that_is_not_html_is_left_to_the_browser() {
    let error = fetched(&client("{}", 200, "application/json"), "https://shop.example.com/api").await.unwrap_err();
    assert!(error.0.contains("not a document read as text"), "{error}");
    let error = fetched(&client("gone", 404, "text/html"), "https://shop.example.com/missing").await.unwrap_err();
    assert!(error.0.contains("HTTP 404"), "{error}");
}

#[tokio::test]
async fn test_a_shell_or_a_challenge_needs_the_browser_that_renders_it() {
    let shell = format!("<html><body><div id=root></div></body></html>{}", "<script>a=1</script>".repeat(200));
    let error = fetched(&html(&shell), "https://shop.example.com/app").await.unwrap_err();
    assert!(error.0.contains("scripts"), "{error}");
    // A bot check answers with its status, as Cloudflare's 403 and 503 challenges do, so it is never read as the page.
    let wall = "<html><body><h1>Just a moment</h1><p>Checking your browser</p></body></html>";
    for status in [403, 503] {
        let page = FetchedPage::new(client(wall, status, "text/html"), None, None);
        let error = page.navigate("https://shop.example.com/app", None).await.unwrap_err();
        assert!(error.needs_browser() && error.to_string().contains(&format!("HTTP {status}")), "{error}");
    }
}

#[test]
fn test_an_address_is_refused_by_what_it_would_hide() {
    let shop = ["https://shop.example.com".to_string()];
    assert!(eligible("https://user:pass@shop.example.com/orders", None).unwrap_err().0.contains("credentials"));
    assert!(eligible("file:///etc/passwd", None).unwrap_err().0.contains("not an address"));
    assert!(
        eligible("https://elsewhere.example.com/orders", Some(&shop)).unwrap_err().0.contains("outside the origins")
    );
    eligible("https://shop.example.com/orders", Some(&shop)).expect("inside the grant");
}

#[tokio::test]
async fn test_a_redirect_outside_the_grant_is_refused() {
    let http = serving(|request| {
        if request.url.starts_with("https://shop.example.com") {
            Response::new(302, "").with_header("location", "https://elsewhere.example.com/orders")
        } else {
            Response::new(200, "<p>elsewhere</p>").with_header("content-type", "text/html")
        }
    });
    let shop = ["https://shop.example.com".to_string()];
    let error = fetch_capture(&http, "https://shop.example.com/orders", Some(&shop), 15.0).await.unwrap_err();
    assert!(error.0.contains("outside the origins"), "{error}");
}

#[tokio::test]
async fn test_a_redirect_inside_the_grant_is_followed() {
    let http = serving(|request| {
        if request.url.ends_with("/old") {
            Response::new(301, "").with_header("location", "/new")
        } else {
            Response::new(200, "<p>moved here</p>").with_header("content-type", "text/html")
        }
    });
    let capture = fetched(&http, "https://shop.example.com/old").await.expect("fetched");
    assert_eq!(capture.url, "https://shop.example.com/new");
    assert_eq!(capture.text, "moved here\n\n");
}

#[tokio::test]
async fn test_a_fetched_page_reads_but_cannot_act() {
    let page = FetchedPage::new(html(PAGE), None, None);
    page.navigate("https://shop.example.com/orders", None).await.expect("navigate");
    let observation = page.observe().await.expect("observe");
    assert_eq!(observation.url, "https://shop.example.com/orders");
    assert!(observation.controls.is_empty());
    assert!(!observation.viewport_text.is_empty());
    assert_eq!(page.address().await.expect("address"), observation.url);
    assert!(page.capture().await.expect("capture").text.contains("| Kettle | 34.50 |"));
    assert!(!page.document_changed(&observation).await.expect("changed"));
    // A step that would act has nothing to act on, and a run that pretends otherwise answers from a page it never saw.
    let action = Action { target_id: Some("1".into()), ..Action::new(Operation::Click) };
    let error = page.act(&action, &observation).await.unwrap_err();
    assert!(matches!(&error, BrowserError::NeedsBrowser(message) if message.contains("no controls")), "{error}");
    let error = page.screenshot().await.unwrap_err();
    assert!(matches!(&error, BrowserError::NeedsBrowser(message) if message.contains("no pixels")), "{error}");
}

#[tokio::test]
async fn test_a_page_that_cannot_be_fetched_needs_the_browser_unless_it_was_a_guess() {
    let page = FetchedPage::new(client("gone", 404, "text/html"), None, None);
    let error = page.navigate("https://shop.example.com/missing", None).await.unwrap_err();
    assert!(error.needs_browser(), "{error}");
    // A guessed address falls back to the start page, as a browser failure does.
    let error = page.navigate("https://shop.example.com/guess", Some("https://shop.example.com/")).await.unwrap_err();
    assert!(matches!(error, BrowserError::Failed(_)), "{error}");
}

#[test]
fn test_an_element_that_hides_itself_is_not_read() {
    let page = r#"<html><body><p>Shown price: 10.00</p><p style="display: none">Old price: 99.00</p>
    <div hidden><p>Template: 0.00</p></div><p class="gone">Note: read</p></body></html>"#;
    let (_title, found) = blocks(page, "https://shop.example.com/orders");
    let texts: Vec<&str> = found.iter().map(|b| b.text.as_str()).collect();
    // What the element itself says is not rendered is not read; a stylesheet's cascade is the browser's to compute.
    assert_eq!(texts, ["Shown price: 10.00", "Note: read"]);
}

#[tokio::test]
async fn test_a_genuinely_short_document_is_read() {
    let page = "<html><body><h1>Status</h1><p>All systems are up.</p></body></html>";
    let capture = fetched(&html(page), "https://status.example.com/").await.expect("fetched");
    assert!(capture.text.contains("All systems are up."));
}

#[tokio::test]
async fn test_a_body_past_the_size_bound_is_refused() {
    let huge = format!("<html><body>{}</body></html>", "a".repeat(6 * 1024 * 1024));
    let error = fetched(&html(&huge), "https://shop.example.com/huge").await.unwrap_err();
    assert!(error.0.contains("past"), "{error}");
}

#[tokio::test]
async fn test_a_charset_is_decoded_by_the_encoding_standard() {
    let served = |content_type: &'static str, body: &'static [u8]| {
        serving(move |_| Response::new(200, body.to_vec()).with_header("content-type", content_type))
    };
    let read =
        |http: Http| async move { fetched(&http, "https://a.example/").await.map(|capture| capture.text).unwrap() };
    assert_eq!(read(html("<p>caf\u{e9}</p>")).await, "caf\u{e9}\n\n");
    // The standard reads ISO-8859-1 and US-ASCII as windows-1252, as every browser does.
    assert_eq!(read(served("text/html; charset=ISO-8859-1", b"<p>caf\xe9 \x80</p>")).await, "caf\u{e9} \u{20ac}\n\n");
    assert_eq!(
        read(served("text/html; charset=\"Shift_JIS\"", b"<p>\x93\xfa\x96\x7b</p>")).await,
        "\u{65e5}\u{672c}\n\n"
    );
    // With no charset in the response, the document's own `<meta>` names it.
    let meta = b"<meta charset=windows-1251><p>\xcf\xf0\xe8</p>";
    assert_eq!(read(served("text/html", meta)).await, "\u{41f}\u{440}\u{438}\n\n");
    // A label the standard does not know is ignored, and the body reads as UTF-8.
    assert_eq!(read(served("text/html; charset=klingon", "<p>caf\u{e9}</p>".as_bytes())).await, "caf\u{e9}\n\n");
}

#[tokio::test]
async fn test_an_address_is_sent_as_whatwg_serializes_it() {
    let http = serving(|request| {
        Response::new(200, format!("<p>{}</p>", request.url)).with_header("content-type", "text/html")
    });
    let capture = fetched(&http, "https://a.example/caf\u{e9} menu").await.expect("fetched");
    assert_eq!(capture.text, "https://a.example/caf%C3%A9%20menu\n\n");
    assert_eq!(capture.url, "https://a.example/caf%C3%A9%20menu");
}

fn gzipped(body: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(body).unwrap();
    encoder.finish().unwrap()
}

fn compressed(coding: &'static str, body: Vec<u8>) -> Http {
    serving(move |request| {
        assert_eq!(request.header("accept-encoding"), Some("gzip, deflate"), "the request asks for what it decodes");
        Response::new(200, body.clone())
            .with_header("content-type", "text/html")
            .with_header("content-encoding", coding)
    })
}

#[tokio::test]
async fn test_a_compressed_body_is_read_as_the_page() {
    // python.org's CDN gzips a response whatever the request asks for, and a refusal left its pages to the browser.
    let page = fetched(&compressed("gzip", gzipped(PAGE.as_bytes())), "https://a.example/").await.expect("fetched");
    assert_eq!(page.blocks, fetched(&html(PAGE), "https://a.example/").await.expect("fetched").blocks);
    let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    zlib.write_all(PAGE.as_bytes()).unwrap();
    let page = fetched(&compressed("deflate", zlib.finish().unwrap()), "https://a.example/").await.expect("fetched");
    assert!(page.text.contains("The total of the orders is 251.00."));
}

#[tokio::test]
async fn test_a_compressed_body_is_bounded_by_what_it_decodes_to() {
    // Five megabytes of spaces gzip to a few kilobytes, which the transport's bound on the received body never sees.
    let bomb = gzipped(&vec![b' '; 5 * 1024 * 1024 + 1]);
    let refusal = fetched(&compressed("gzip", bomb), "https://a.example/").await.unwrap_err();
    assert!(refusal.0.contains("bytes"), "{refusal}");
    let refusal = fetched(&compressed("br", vec![0x0b]), "https://a.example/").await.unwrap_err();
    assert!(refusal.0.contains("compression"), "{refusal}");
    let refusal = fetched(&compressed("gzip", vec![0x1f, 0x8b, 0x08]), "https://a.example/").await.unwrap_err();
    assert!(refusal.0.contains("corrupt"), "{refusal}");
}

#[tokio::test]
async fn test_the_request_names_the_client() {
    // A server refuses a client that does not name itself: Wikipedia answers 403 to a request carrying no User-Agent
    // and 200 to one that names it, and httpx sends its own default where this sends none.
    let http = serving(|request| {
        let agent = request.header("user-agent").unwrap_or_default();
        assert!(agent.starts_with("fastbrowse/"), "the request names the client, got {agent:?}");
        Response::new(200, "<p>the page</p>").with_header("content-type", "text/html")
    });
    fetched(&http, "https://a.example/").await.expect("fetched");
}

#[tokio::test]
async fn test_access_withdrawn_after_the_navigation_stops_the_read() {
    let allowed = Arc::new(AtomicBool::new(true));
    let gate = allowed.clone();
    let check: CheckAccess = Arc::new(move || {
        let open = gate.load(Ordering::SeqCst);
        Box::pin(async move { if open { Ok(()) } else { Err("withdrawn".into()) } })
    });
    let page = FetchedPage::new(html(PAGE), None, Some(check));
    page.navigate("https://shop.example.com/orders", None).await.expect("navigate");
    page.observe().await.expect("observe");
    allowed.store(false, Ordering::SeqCst);
    assert!(page.observe().await.unwrap_err().to_string().contains("check_access refused"));
    assert!(page.capture().await.unwrap_err().to_string().contains("check_access refused"));
}

/// Every case in `golden/fetch.json`, or the file `FASTBROWSE_FETCH_GOLDEN` names, read into the blocks it recorded.
/// `FASTBROWSE_UPDATE_GOLDEN=1` records what the reader makes now, after an intended change.
#[test]
fn test_blocks_match_the_recorded_blocks_of_every_fixture_page() {
    let path = std::env::var("FASTBROWSE_FETCH_GOLDEN")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/fetch.json").into());
    let mut cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&path).expect("golden")).expect("json");
    let update = std::env::var("FASTBROWSE_UPDATE_GOLDEN").as_deref() == Ok("1");
    let mut differing = Vec::new();
    for case in &mut cases {
        let (title, found) = blocks(case["html"].as_str().unwrap(), case["url"].as_str().unwrap());
        let got: Vec<Value> = found
            .iter()
            .map(|b| {
                serde_json::json!({
                    "kind": serde_json::to_value(b.kind).unwrap(),
                    "text": b.text,
                    "heading_path": b.heading_path,
                    "href": b.href,
                })
            })
            .collect();
        let expected = case["blocks"].as_array().unwrap();
        if title != case["title"] || got != *expected {
            let first = got.iter().zip(expected).position(|(a, b)| a != b).unwrap_or(got.len().min(expected.len()));
            differing.push(format!(
                "{}: title {title:?} vs {}; block {first}: {:?} vs {:?} ({} vs {} blocks)",
                case["name"],
                case["title"],
                got.get(first),
                expected.get(first),
                got.len(),
                expected.len()
            ));
            case["title"] = Value::from(title);
            case["blocks"] = Value::from(got);
        }
    }
    if update {
        std::fs::write(&path, serde_json::to_string_pretty(&cases).unwrap() + "\n").unwrap();
        return;
    }
    assert!(differing.is_empty(), "{} of {} differ:\n{}", differing.len(), cases.len(), differing.join("\n"));
}
