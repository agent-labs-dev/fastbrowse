mod common;
mod misc_support;

use common::{eval_active, harness, sites};
use fastbrowse_core::models::Operation;
use fastbrowse_core::page::{Action, BlockKind};
use misc_support::text_of;

// -- test_comment_context.py: reply controls belong to their comment record, including when only one form is open --

#[tokio::test(flavor = "multi_thread")]
async fn test_one_open_reply_form_does_not_inherit_the_first_commenter() {
    for record in ["li", "article", r#"div role="listitem""#] {
        let Some(h) = harness().await else { return };
        let tag = record.split(' ').next().unwrap();
        h.page
            .navigate(
                &format!(
                    "data:text/html,<form><textarea aria-label='Comment text area'></textarea><button>Post</button></form>\
                     <section><h2>Comments</h2><ul>\
                     <{record}><a>First commenter</a><p>First request</p><button>Reply</button></{tag}>\
                     <{record}><a>Merlinqt</a><p>Scale map icons</p><button>Reply</button>\
                     <form><textarea aria-label='Comment text area'>@Merlinqt </textarea>\
                     <p>Please follow our Terms of Service</p><button>Cancel</button><button>Post</button></form></{tag}>\
                     </ul></section>"
                ),
                None,
            )
            .await
            .expect("navigate");
        let observation = h.observe().await;
        let field = observation
            .controls
            .iter()
            .find(|c| c.label == "Comment text area" && c.value.as_deref() == Some("@Merlinqt "))
            .unwrap();
        let posts: Vec<_> = observation.controls.iter().filter(|c| c.label == "Post").collect();
        assert_eq!(field.context.as_deref(), Some("Merlinqt"), "{record}");
        assert_eq!(
            posts.iter().map(|c| c.context.as_deref()).collect::<Vec<_>>(),
            [None, Some("Merlinqt")],
            "{record}"
        );
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_captured_list_block_carries_no_list_fields() {
    let Some(h) = harness().await else { return };
    h.page
        .navigate(
            "data:text/html,<h1>Birch</h1><ul><li><p>Elm: The map crashes.</p></li><li><p>Oak: Thanks.</p></li></ul>",
            None,
        )
        .await
        .expect("navigate");
    let script = format!("({})(null).blocks", fastbrowse_browser::session::CAPTURE_JS);
    let blocks = eval_active(&h.session, &script).await;
    let blocks = blocks.as_array().expect("capture.js returns its blocks");
    assert!(blocks.iter().any(|block| block["text"].as_str().is_some_and(|text| text.contains("Elm"))), "{blocks:?}");
    let keys: Vec<&String> =
        blocks.iter().filter_map(|block| block.as_object()).flat_map(|block| block.keys()).collect();
    assert!(keys.iter().all(|key| !key.starts_with("list_")), "{keys:?}");
    h.close().await;
}

// -- test_image_evidence.py: rendered image elements need DOM evidence even when they contain no text -----------

#[tokio::test(flavor = "multi_thread")]
async fn test_a_drawing_in_the_viewport_is_visual_content() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<h1>Report</h1><p>' + 'word '.repeat(40) + '</p>' +
          '<canvas width="320" height="240"></canvas>';
        "#,
    )
    .await;
    assert!(h.observe().await.visual_content);
    eval_active(&h.session, "document.querySelector('canvas').remove()").await;
    assert!(!h.observe().await.visual_content);
    h.close().await;
}

/// A streamed remote browser draws its page as an image that takes clicks, and a canvas app draws on a canvas
/// (#1084): each is marked drawn, so the run knows there are no elements inside it to act on. A button showing an icon
/// is an element like any other.
#[tokio::test(flavor = "multi_thread")]
async fn test_an_image_or_canvas_that_takes_input_is_a_drawn_control() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<img alt="Remote browser view" width="320" height="240" ' +
          'src="data:image/gif;base64,R0lGODlhAQABAAAAACw=">' +
          '<canvas aria-label="Board" width="320" height="240"></canvas>' +
          '<button><img alt="" width="16" height="16" src="data:image/gif;base64,R0lGODlhAQABAAAAACw=">Cart</button>';
        document.querySelector('img').onclick = () => {};
        document.querySelector('canvas').addEventListener('mousedown', () => {});
        "#,
    )
    .await;
    let obs = h.observe().await;
    let drawn = |label: &str| obs.controls.iter().find(|c| c.label == label).map(|c| c.drawn);
    assert_eq!(drawn("Remote browser view"), Some(Some(true)), "{:?}", obs.controls);
    assert_eq!(drawn("Board"), Some(Some(true)), "{:?}", obs.controls);
    assert_eq!(drawn("Cart"), Some(None), "{:?}", obs.controls);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_open_modal_is_named_and_the_page_behind_it_leaves() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<main><a href="/a">Alpha</a><a href="/b">Beta</a></main>' +
          '<div role="dialog" aria-modal="true" aria-labelledby="t"><h2 id="t">Stops</h2><button>Close dialog</button></div>';
        document.querySelector('main').setAttribute('aria-hidden', 'true');
        "#,
    )
    .await;
    let open = h.observe().await;
    assert_eq!(open.modal.as_deref(), Some("Stops"));
    let labels: Vec<&str> = open.controls.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, ["Close dialog"]);
    eval_active(
        &h.session,
        "document.querySelector('[role=dialog]').remove(); document.querySelector('main').removeAttribute('aria-hidden')",
    )
    .await;
    assert_eq!(h.observe().await.modal, None);
    eval_active(
        &h.session,
        "document.body.insertAdjacentHTML('beforeend', '<dialog><button>OK</button></dialog>'); document.querySelector('dialog').showModal()",
    )
    .await;
    assert_eq!(h.observe().await.modal.as_deref(), Some(""));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_drawing_below_the_fold_does_not_ask_for_a_screenshot() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<h1>Report</h1><p>' + 'word '.repeat(40) + '</p>' +
          '<div style="height:4000px"></div><canvas width="320" height="240"></canvas>';
        "#,
    )
    .await;
    assert!(!h.observe().await.visual_content);
    // Scrolled into view, the same drawing is page state the screenshot does carry.
    eval_active(&h.session, "scrollTo(0, document.body.scrollHeight)").await;
    assert!(h.observe().await.visual_content);
    h.close().await;
}

// A media QA run pressed Play and Pause until its step limit: native players draw their clock in shadow DOM, so
// neither the page text nor a read said whether anything played.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_players_state_is_observed_and_read() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        const rate = 8000, n = rate * 30, wav = new DataView(new ArrayBuffer(44 + n));
        const tag = (at, s) => [...s].forEach((c, i) => wav.setUint8(at + i, c.charCodeAt(0)));
        tag(0, 'RIFF'); wav.setUint32(4, 36 + n, true); tag(8, 'WAVEfmt '); wav.setUint32(16, 16, true);
        wav.setUint16(20, 1, true); wav.setUint16(22, 1, true); wav.setUint32(24, rate, true);
        wav.setUint32(28, rate, true); wav.setUint16(32, 1, true); wav.setUint16(34, 8, true);
        tag(36, 'data'); wav.setUint32(40, n, true); new Uint8Array(wav.buffer, 44).fill(128);
        const src = URL.createObjectURL(new Blob([wav.buffer], { type: 'audio/wav' }));
        document.body.innerHTML = '<h2>Lesson one</h2><audio controls aria-label="Lesson one audio">Not supported</audio>' +
          '<h2>Lesson two</h2><table><tr><th>Comment</th></tr></table><fieldset hidden><legend>Font Family</legend></fieldset>' +
          '<audio controls>Not supported</audio>';
        for (const player of document.querySelectorAll('audio')) player.src = src;
        const first = document.querySelector('audio');
        first.muted = true;
        const play = document.createElement('button');
        play.textContent = 'Play lesson one';
        play.onclick = () => first.play();
        document.body.prepend(play);
        "#,
    )
    .await;
    // Chrome plays only after a user gesture, so the player starts from a click the way a run would start it.
    let page = h.observe().await;
    let play = page.controls.iter().find(|c| c.label == "Play lesson one").expect("play button");
    let click = Action { target_id: Some(play.id.clone()), ..Action::new(Operation::Click) };
    h.page.act(&click, &page).await.expect("act");
    let mut media = Vec::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        media = h.observe().await.media;
        if media.len() == 2 && media[0].elapsed_seconds > 0.5 && media[1].duration_seconds.is_some() {
            break;
        }
    }
    assert_eq!(media.len(), 2, "{media:?}");
    let (playing, idle) = (&media[0], &media[1]);
    assert_eq!(playing.kind, "audio");
    assert_eq!(playing.label.as_deref(), Some("Lesson one audio"));
    assert!(!playing.paused && playing.muted && playing.elapsed_seconds > 0.5, "{playing:?}");
    assert_eq!(playing.duration_seconds, Some(30.0));
    assert_eq!((idle.label.as_deref(), idle.heading.as_deref()), (None, Some("Lesson two")));
    assert!(idle.paused && idle.elapsed_seconds == 0.0, "{idle:?}");
    let capture = h.page.capture().await.expect("capture");
    let read: Vec<String> = capture
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Observation)
        .map(|b| text_of(&capture, b))
        .filter(|t| t.starts_with("Audio element state"))
        .collect();
    assert_eq!(read.len(), 2, "{read:?}");
    assert!(read[0].contains(r#""label":"Lesson one audio""#) && read[0].contains(r#""paused":false"#), "{read:?}");
    assert!(read[1].contains(r#""heading":"Lesson two""#) && read[1].contains(r#""paused":true"#), "{read:?}");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_reports_visible_image_metadata_without_inventing_pixels() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<h1>README</h1><a href="/map.png">' +
          '<img src="/map.png" alt="Minimap screenshot"></a>' +
          '<img src="/hidden.png" alt="Private image" style="display:none">';
    "#,
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("Minimap screenshot"));
    assert!(capture.text.contains("/map.png"));
    assert!(!capture.text.contains("Private image"));
    assert!(capture.text.contains("Image element"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_does_not_copy_inline_image_payload() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<img alt="Inline chart">';
        document.querySelector('img').src = 'data:image/png;base64,' + 'PAYLOAD'.repeat(100000);
        "#,
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    assert!(capture.text.contains("Inline chart"));
    assert!(capture.text.contains("data:image/png;base64,[payload omitted]"));
    assert!(!capture.text.contains("PAYLOAD"));
    assert!(capture.text.chars().count() < 2000);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_image_line_names_the_image_and_not_what_it_cannot_show() {
    // The absence sentence was cut as a span and quoted as a fact: a Street View run answered "Pixels are not
    // described" 33 times over a task about what the view shows. The metadata is the quotable part.
    let Some(h) = harness().await else { return };
    eval_active(
        &h.session,
        r#"document.body.innerHTML = '<img alt="Chart of exports" src="https://example.test/a.png">';"#,
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    let line = capture.text.lines().find(|line| line.starts_with("Image element DOM metadata")).expect("an image line");
    assert!(line.contains("\"alt\":\"Chart of exports\""), "{line}");
    assert!(line.contains("\"pixels_described\":false"), "{line}");
    assert!(!line.contains("Pixels are not described"), "{line}");
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_image_line_quotes_the_item_it_sits_in() {
    // A gallery's captions were read as text and its sources as image lines with nothing joining them, so a caption
    // paired with a source by position failed its support check and the row was left out of the answer (#929).
    let Some(h) = harness().await else { return };
    eval_active(
        &h.session,
        r#"document.body.innerHTML = '<ul>' +
          '<li><a class="tile"><span><img alt="thumbnail" src="https://example.test/01.gif"></span><p>Dungeon entrances</p></a></li>' +
          '<li><a class="tile"><span><img alt="thumbnail" src="https://example.test/map.webp"></span><p>The Eastern Kingdoms</p></a></li>' +
          '</ul><p><img alt="divider" src="https://example.test/rule.png"></p><img alt="lone" src="https://example.test/lone.png">' +
          '<div class="card"><img src="https://example.test/a.png"><p>Card A</p></div>' +
          '<div class="card"><img src="https://example.test/b.png"><p>Card B</p></div>' +
          '<div class="card"><img src="https://example.test/c.png"><p>Card C</p></div>' +
          '<figure><img src="https://example.test/fig.png"><figcaption>A captioned figure</figcaption></figure>' +
          '<header><img alt="logo" src="https://example.test/logo.png"><nav>Home About Contact</nav></header>' +
          '<article><h1>Title</h1><p>First paragraph.</p><img src="https://example.test/inline.png"><p>Second.</p></article>' +
          '<section><div><div><img src="https://example.test/wrapped.png"></div></div><p>One.</p><p>Two.</p></section>' +
          '<ul><li><h2>Gallery</h2><div><img src="https://example.test/ga.png"><p>Caption A</p></div>' +
          '<div><img src="https://example.test/gb.png"><p>Caption B</p></div>' +
          '<div><img src="https://example.test/gc.png"><p>Caption C</p></div></li><li>Other</li></ul>';"#,
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    let images: Vec<&str> =
        capture.text.lines().filter(|line| line.starts_with("Image element DOM metadata")).collect();
    assert_eq!(images.len(), 14, "{images:?}");
    let near = |source: &str| {
        let line = images.iter().find(|line| line.contains(source)).expect(source);
        let metadata: serde_json::Value =
            serde_json::from_str(line.trim_start_matches("Image element DOM metadata: ")).expect("metadata");
        metadata["near"].as_str().map(str::to_string)
    };
    // A list item, a repeated record and a figure are each one item, and quote their own text.
    assert_eq!(near("01.gif").as_deref(), Some("Dungeon entrances"));
    assert_eq!(near("map.webp").as_deref(), Some("The Eastern Kingdoms"));
    assert_eq!(near("b.png").as_deref(), Some("Card B"));
    assert_eq!(near("fig.png").as_deref(), Some("A captioned figure"));
    // An image in no item quotes nothing: not an empty paragraph's neighbours, a menu beside a logo, an article it is
    // placed in, nor the section around empty wrappers. A list item holding blocks is read as those blocks, not as one
    // item, so it does not tie each of its images to every caption in it.
    for source in ["rule.png", "lone.png", "logo.png", "inline.png", "wrapped.png", "ga.png", "gb.png", "gc.png"] {
        assert_eq!(near(source), None, "{source}");
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_every_image_is_observed_and_decode_state_is_ignored() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = Array.from({length: 300}, (_, i) =>
          `<img alt="Chart ${i}" src="blob:${'x'.repeat(4000)}">`).join('');
        "#,
    )
    .await;
    let before = h.page.capture().await.expect("capture");
    assert_eq!(before.text.matches("Image element DOM metadata").count(), 300);
    assert!(before.text.contains("blob:[identifier omitted]"));
    eval_active(
        &h.session,
        r#"
        for (const image of document.querySelectorAll('img')) {
          Object.defineProperty(image, 'complete', {value: true});
          Object.defineProperty(image, 'naturalWidth', {value: 100});
        }
        "#,
    )
    .await;
    assert_eq!(h.page.capture().await.expect("capture").text, before.text);
    h.close().await;
}

// -- test_link_evidence.py: a link drawn only as an icon says where it goes and nothing else, so its destination
// must reach the capture ----------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_reports_icon_link_destination_with_its_row() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r##"
        document.body.innerHTML = '<h1>Releases</h1><div class="row"><span>v2.1.0</span> <span>12 KB</span> ' +
          '<span><a href="/files/abc.zip?sig=1"><svg viewBox="0 0 8 8"><path d="M0 0h8v8z"></path></svg></a>' +
          '</span></div><a href="/about">About</a>' +
          '<a href="/private.zip" style="display:none"><svg></svg></a><a href="#menu"><svg></svg></a>' +
          '<a href="#"><svg></svg></a><a href="http://["><svg></svg></a>';
    "##,
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    let destination = format!("{}/files/abc.zip?sig=1", sites().main.trim_end_matches('/'));
    let observed: Vec<String> = capture
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Observation)
        .map(|b| text_of(&capture, b))
        .filter(|t| t.contains(&destination))
        .collect();
    assert_eq!(observed.len(), 1);
    assert!(observed[0].contains("v2.1.0 12 KB"));
    // A link with words says what it is in the text already; a hidden one is not on the page a user sees.
    assert!(!capture.text.contains("/about"));
    assert!(!capture.text.contains("/private.zip"));
    assert!(!capture.text.contains("#menu"));
    // A bare `#` is a placeholder whose parsed hash is empty, and an unparsable address must not end the capture.
    assert_eq!(capture.text.matches("Link element DOM metadata").count(), 1);
    assert!(!capture.text.contains("http://["));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_keeps_a_time_elements_datetime_with_its_text() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<div><a href="/u">rustbot</a> released this ' +
          '<relative-time id=rt datetime="2026-10-16T04:47:44Z">16 Oct 04:47</relative-time></div>' +
          '<p>Posted <time datetime="2026-10-16">3 weeks ago</time></p>' +
          '<ol><li>Opened <time datetime="2026-02-02">Feb 2</time>, closed <time datetime="2026-02-02">today</time></li></ol>' +
          '<p><time datetime="2026-01-02">2026-01-02</time></p>' +
          '<p>Edited <del datetime="2020-01-01">old text</del></p>' +
          '<p><time datetime="1999-01-01" style="display:none">x</time>Kept</p>' +
          '<a href="/c"><time datetime="2026-11-01">5 days ago</time></a>' +
          '<ul><li>Shipped <relative-time id=lt datetime="2026-09-01">Sep 1</relative-time></li></ul>' +
          '<table><tr><th>When</th></tr><tr><td><time datetime="2026-03-04">March</time></td></tr></table>';
        document.getElementById('rt').attachShadow({ mode: 'open' }).textContent = '3 weeks ago';
        document.getElementById('lt').attachShadow({ mode: 'open' }).textContent = 'last month';
        "#,
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    // A shadow-drawn relative time reads in its row, followed by the exact date the element holds.
    assert!(capture.text.contains("rustbot released this 3 weeks ago (2026-10-16T04:47:44Z)"), "{}", capture.text);
    assert!(capture.text.contains("Posted 3 weeks ago (2026-10-16)"), "{}", capture.text);
    // Each time in a list item is stamped, even when an earlier one holds the same date.
    assert!(capture.text.contains("Opened Feb 2 (2026-02-02), closed today (2026-02-02)"), "{}", capture.text);
    // Shown text that already states the date, an edit's date and a hidden time add nothing.
    assert!(!capture.text.contains("2026-01-02 (2026-01-02)"));
    assert!(!capture.text.contains("2020-01-01"));
    assert!(!capture.text.contains("1999-01-01"));
    let link = capture.blocks.iter().find(|b| b.kind == BlockKind::Link && b.href.as_deref() == Some("/c")).unwrap();
    assert_eq!(text_of(&capture, link), "5 days ago (2026-11-01)");
    // The detached shadow block of a host its list item read without that text still carries the date.
    assert!(capture.text.contains("last month (2026-09-01)"), "{}", capture.text);
    let table = capture.blocks.iter().find(|b| b.kind == BlockKind::Table).unwrap();
    assert!(text_of(&capture, table).contains("March (2026-03-04)"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_image_link_does_not_borrow_its_articles_text() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = '<article><p><a href="/demo.gif"><img alt="" src="data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7"></a></p>' +
          '<h1>Project</h1><p>A package with its own installation instructions.</p></article>' +
          '<table><tr><td>Release 2</td><td><span><a href="/release.zip"><svg></svg></a></span></td></tr></table>' +
          '<div style="display:flex"><span>Flex release</span><span><a href="/flex.zip"><svg></svg></a></span></div>' +
          '<div style="display:grid"><span>Grid release</span><div><a href="/grid.zip"><svg></svg></a></div></div>' +
          '<table><tr><td><ul><li>First release <a href="/first.zip"><svg></svg></a></li>' +
          '<li>Second release <a href="/second.zip"><svg></svg></a></li></ul></td></tr></table>' +
          '<div>Wrapped release<div><a href="/wrapped.zip"><svg></svg></a></div></div>' +
          '<li>Floating release<div style="float:right"><a href="/floating.zip"><svg></svg></a></div></li>' +
          '<article style="display:flex;flex-direction:column"><p><a href="/flex-demo.gif"><img alt=""></a></p><p>Unrelated article text</p></article>' +
          '<div><a href="/empty.zip"><svg></svg></a></div>' +
          '<article><h2>Card release</h2><a href="/card.zip"><svg></svg></a></article>' +
          '<dl><dt>Definition release</dt><dd><a href="/definition.zip"><svg></svg></a></dd></dl>';
        "#,
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    let links: Vec<_> = capture.text.lines().filter(|line| line.starts_with("Link element DOM metadata")).collect();
    let demo = links.iter().find(|line| line.contains("/demo.gif")).unwrap();
    assert!(demo.contains("\"near\":null"), "{demo}");
    assert!(!demo.contains("installation instructions"));
    let release = links.iter().find(|line| line.contains("/release.zip")).unwrap();
    assert!(release.contains("Release 2"), "{release}");
    for (href, own, other) in [
        ("/flex.zip", "Flex release", "Grid release"),
        ("/grid.zip", "Grid release", "Flex release"),
        ("/first.zip", "First release", "Second release"),
        ("/second.zip", "Second release", "First release"),
        ("/wrapped.zip", "Wrapped release", "Floating release"),
        ("/floating.zip", "Floating release", "Wrapped release"),
        ("/card.zip", "Card release", "Definition release"),
        ("/definition.zip", "Definition release", "Card release"),
    ] {
        let link = links.iter().find(|line| line.contains(href)).unwrap();
        assert!(link.contains(own), "{link}");
        assert!(!link.contains(other), "{link}");
    }
    for href in ["/flex-demo.gif", "/empty.zip"] {
        let link = links.iter().find(|line| line.contains(href)).unwrap();
        assert!(link.contains("\"near\":null"), "{link}");
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_every_icon_link_destination_is_observed_whole() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    eval_active(
        &h.session,
        r#"
        document.body.innerHTML = Array.from({length: 100}, (_, i) =>
          `<p>Row ${i} <a href="/f/${i}?k=${'x'.repeat(4000)}"><svg></svg></a></p>`).join('');
        "#,
    )
    .await;
    let capture = h.page.capture().await.expect("capture");
    assert_eq!(capture.text.matches("Link element DOM metadata").count(), 100);
    assert!(capture.text.contains(&format!("/f/99?k={}", "x".repeat(4000))));
    h.close().await;
}
