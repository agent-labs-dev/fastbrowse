mod common;
mod misc_support;

use std::collections::HashSet;
use std::time::Instant;

use common::{Harness, harness, sites};
use fastbrowse_browser::shared::BlockKind;
use misc_support::{content_capture, eval_active, path, set_body, text_of, texts, visible, wait_until};

// -- test_capture.py: capture blocks preserve the records a reader needs to cite together -----------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_quote_cards_are_whole_records_with_local_titles() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    let capture = content_capture(&h.page).await;
    let quotations = vec!["Fastbrowse fixture", "Quotations"];
    let records: Vec<_> =
        capture.blocks.iter().filter(|b| b.kind == BlockKind::Record && path(b) == quotations).collect();
    let expected = [
        ("Measure twice, cut once.", "by Ada (about)", "craft care"),
        ("Leave room for a second idea.", "by Grace (about)", "ideas work"),
        ("Ask what the evidence says.", "by Katherine (about)", "science care"),
    ];
    assert_eq!(records.len(), expected.len());
    for (block, (quotation, author, tags)) in records.iter().zip(expected) {
        let text = text_of(&capture, block);
        assert!(text.contains(&format!("{quotation}\n")) && text.contains(&format!("{author}\nTags: {tags}")));
        assert_eq!(path(block), quotations);
        assert!(block.frame_id.is_none() && block.source_id.starts_with("main/:"));
        assert!(block.href.is_none());
        assert_eq!(capture.text.matches(quotation).count(), 1);
        assert_eq!(capture.text.matches(author).count(), 1);
    }
    assert!(text_of(&capture, records.last().unwrap()).contains("A thought for today"));
    let after = capture.blocks.iter().find(|b| text_of(&capture, b) == "After the quotations.").unwrap();
    assert_eq!(path(after), quotations);
    let standalone: Vec<_> = capture.blocks.iter().filter(|b| text_of(&capture, b).contains("A standalone")).collect();
    assert_eq!(
        standalone.iter().map(|b| text_of(&capture, b)).collect::<Vec<_>>(),
        ["A standalone first paragraph.", "A standalone second paragraph."]
    );
    assert!(standalone.iter().all(|b| b.kind == BlockKind::Paragraph));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_each_table_row_repeats_its_header() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    let capture = content_capture(&h.page).await;
    let tables: Vec<_> = capture.blocks.iter().filter(|b| b.kind == BlockKind::Table).collect();
    assert_eq!(
        tables.iter().map(|b| text_of(&capture, b)).collect::<Vec<_>>(),
        [
            "| Name | Score |\n| --- | --- |\n| Ada | 10 |",
            "| Name | Score |\n| --- | --- |\n| Grace | 9 |",
            "| Team | Points |\n| --- | --- |\n| North | 12 |",
            "| Team | Points |\n| --- | --- |\n| East | 11 |",
            "| Team | Points |\n| --- | --- |\n| West | 10 |",
        ]
    );
    assert_eq!(
        tables.iter().map(|b| path(b)).collect::<Vec<_>>(),
        [
            ["Fastbrowse fixture", "Table"],
            ["Fastbrowse fixture", "Table"],
            ["Fastbrowse fixture", "Rankings"],
            ["Fastbrowse fixture", "Rankings"],
            ["Fastbrowse fixture", "Rankings"],
        ]
    );
    assert_eq!(tables.iter().map(|b| &b.source_id).collect::<HashSet<_>>().len(), 5);
    h.close().await;
}

/// `page.navigate(f"{main_site}/icons.html")`, then `document.body.innerHTML = markup`.
async fn icons_with(markup: &str) -> Option<Harness> {
    let h = harness().await?;
    h.goto("/icons.html").await;
    set_body(&h.session, markup).await;
    Some(h)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_table_header_variants() {
    let cases: [(&str, &str, &[&str]); 11] = [
        (
            "multiple-header-rows",
            "<thead><tr><td>Group</td></tr><tr><th>Name</th></tr></thead>\
             <tbody><tr><td>A | B</td></tr><tr><td>C</td></tr></tbody>",
            &["| Group |\n| Name |\n| --- |\n| A \\| B |", "| Group |\n| Name |\n| --- |\n| C |"],
        ),
        (
            "no-header",
            "<tr><td>A</td><td>1</td></tr><tr><th>B</th><td>2</td></tr>",
            &["| A | 1 |", "| A | 1 |\n| B | 2 |"],
        ),
        (
            "headerless-comparison",
            "<tr><td>Feature</td><td>Device Alpha</td><td>Device Beta</td></tr>\
             <tr><td>USB-C ports</td><td>1</td><td>3</td></tr>\
             <tr><td>USB-A ports</td><td>2</td><td>1</td></tr>",
            &[
                "| Feature | Device Alpha | Device Beta |",
                "| Feature | Device Alpha | Device Beta |\n| USB-C ports | 1 | 3 |",
                "| Feature | Device Alpha | Device Beta |\n| USB-A ports | 2 | 1 |",
            ],
        ),
        (
            "header-only",
            "<thead><tr><th>Name</th></tr><tr><th>Person</th></tr></thead>",
            &["| Name |\n| Person |\n| --- |"],
        ),
        ("leading-header-only", "<tr><th>Name | alias</th></tr>", &["| Name \\| alias |\n| --- |"]),
        (
            "hidden-rows",
            "<thead><tr hidden><th>Group</th></tr><tr><th>Name</th></tr></thead>\
             <tbody><tr style='display:none'><td>X</td></tr><tr><td>A</td></tr>\
             <tr style='visibility:collapse'><td>Y</td></tr></tbody>\
             <tfoot><tr style='visibility:hidden'><td>Z</td></tr><tr><td>Total</td></tr></tfoot>",
            &["| Name |\n| --- |\n| A |", "| Name |\n| --- |\n| Total |"],
        ),
        (
            "hidden-sections",
            "<thead style='display:none'><tr><th>Name</th></tr></thead><tbody><tr><td>A</td></tr></tbody>\
             <tfoot style='visibility:collapse'><tr><td>Total</td></tr></tfoot>",
            &["| A |"],
        ),
        (
            "every-data-row-hidden",
            "<tr><th>Name</th></tr><tr hidden><td>Ada</td></tr><tr style='display:none'><td>Grace</td></tr>",
            &["| Name |\n| --- |"],
        ),
        ("every-row-hidden", "<tr hidden><td>Ada</td></tr>", &[]),
        (
            "hidden-column",
            "<tr><th>Name</th><th style='display:none'>Id</th></tr>\
             <tr><td>Ada</td><td style='display:none'>7</td></tr>",
            &["| Name |\n| --- |\n| Ada |"],
        ),
        (
            "invisible-cell-keeps-its-column",
            "<tr><th>Name</th><th>Id</th><th>Team</th></tr>\
             <tr><td>Ada</td><td style='visibility:hidden'>7</td><td>North</td></tr>",
            &["| Name | Id | Team |\n| --- | --- | --- |\n| Ada |  | North |"],
        ),
    ];
    for (id, rows, expected) in cases {
        let Some(h) = icons_with(&format!("<table>{rows}</table>")).await else { return };
        let capture = content_capture(&h.page).await;
        assert_eq!(texts(&capture), expected, "{id}");
        assert!(capture.blocks.iter().all(|b| b.kind == BlockKind::Table), "{id}");
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_table_rows_below_the_viewport_are_captured() {
    let Some(h) = icons_with(r#"<table style="margin-top:200vh"><tr><td>Below the viewport</td></tr></table>"#).await
    else {
        return;
    };
    assert_eq!(
        eval_active(&h.session, "document.querySelector('tr').getBoundingClientRect().top > innerHeight").await,
        true
    );
    let capture = content_capture(&h.page).await;
    assert_eq!(texts(&capture), ["| Below the viewport |"]);
    assert_eq!(texts(&capture).join("\n\n"), "| Below the viewport |");
    assert!(capture.blocks.iter().all(|b| b.kind == BlockKind::Table));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_records_keep_single_links_and_a_page_sized_card_is_not_one_record() {
    for (tag, kind) in [("div", BlockKind::Record), ("li", BlockKind::ListItem)] {
        let cards: String = [40, 40, 5000]
            .iter()
            .map(|length| {
                format!(r#"<{tag} class="card"><div>{}</div><a href="/author">Author</a></{tag}>"#, "x".repeat(*length))
            })
            .collect();
        let Some(h) = icons_with(&format!("<ul>{cards}</ul>")).await else { return };
        let capture = content_capture(&h.page).await;
        assert_eq!(
            capture.blocks.iter().map(|b| b.kind).collect::<Vec<_>>(),
            [kind, kind, BlockKind::Paragraph, BlockKind::Link],
            "{tag}"
        );
        assert!(capture.blocks[..2].iter().all(|b| b.href.as_deref() == Some("/author")), "{tag}");
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_non_records_fall_back_to_the_walk() {
    let cases = [
        ("two-siblings", "<div class='card'><p>First</p><p>Second</p></div>".repeat(2)),
        (
            "different-classes",
            r#"<div class="one"><p>First</p></div><div class="two"><p>Second</p></div><div class="one two"><p>Third</p></div>"#
                .to_owned(),
        ),
        (
            "different-tags",
            "<div><p>First</p></div><section><p>Second</p></section><article><p>Third</p></article>".to_owned(),
        ),
        ("inline-only", "<div class='card'><span>Inline only</span></div>".repeat(3)),
        ("two-headings", "<div class='card'><h2>First title</h2><h3>Second title</h3><p>Body</p></div>".repeat(3)),
        ("table", "<div class='card'><p>Outside</p><table><tr><td>Inside</td></tr></table></div>".repeat(3)),
        ("list", "<div class='card'><p>Outside</p><ul><li>Inside</li><li>Another</li><li>Last</li></ul></div>".repeat(3)),
        ("classless", "<div><p>Layout</p><p>column</p></div>".repeat(3)),
        ("two-headed-columns", "<div class=column><h3>Column</h3><p>Content</p></div>".repeat(2)),
    ];
    for (id, cards) in cases {
        let Some(h) = icons_with(&cards).await else { return };
        let capture = content_capture(&h.page).await;
        assert!(!capture.blocks.is_empty(), "{id}");
        assert!(capture.blocks.iter().all(|b| b.kind != BlockKind::Record), "{id}");
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_two_item_styled_sections_keep_child_records_and_parent_headings() {
    for groups in [2, 3, 4] {
        for wrapped_heading in [false, true] {
            let mut markup = String::new();
            for group in 0..groups {
                let mut heading = format!("<h2>Section {group}</h2>");
                if wrapped_heading {
                    heading = format!("<div>{heading}<p>2 items</p></div>");
                }
                let children: String = (0..2)
                    .map(|item| {
                        format!(r#"<div class="item"><p>Item {group}.{item}</p><p>Detail {group}.{item}</p></div>"#)
                    })
                    .collect();
                markup.push_str(&format!(r#"<section class="section">{heading}{children}</section>"#));
            }
            let Some(h) = icons_with(&markup).await else { return };
            let capture = content_capture(&h.page).await;
            let records: Vec<_> = capture.blocks.iter().filter(|b| b.kind == BlockKind::Record).collect();
            let expected: Vec<String> = (0..groups)
                .flat_map(|group| (0..2).map(move |item| format!("Item {group}.{item}\n\nDetail {group}.{item}")))
                .collect();
            assert_eq!(records.iter().map(|b| text_of(&capture, b)).collect::<Vec<_>>(), expected);
            let headings: Vec<String> =
                (0..groups).flat_map(|group| (0..2).map(move |_| format!("Section {group}"))).collect();
            assert_eq!(records.iter().map(|b| b.heading_path.last().unwrap().clone()).collect::<Vec<_>>(), headings);
            h.close().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_two_linked_cards_inside_unlabelled_wrappers_remain_separate_records() {
    let markup = concat!(
        "<section><div><h2>Group</h2></div><div><div>",
        r#"<a class="card" href="/one"><div><p>First item</p><span>Type</span></div></a>"#,
        r#"<a class="card" href="/two"><div><p>Second item</p><span>Type</span></div></a>"#,
        "</div></div></section>",
    );
    let Some(h) = icons_with(markup).await else { return };
    let capture = content_capture(&h.page).await;
    let records: Vec<_> = capture.blocks.iter().filter(|b| b.kind == BlockKind::Record).collect();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records.iter().map(|b| text_of(&capture, b)).collect::<Vec<_>>(),
        ["First item\n\nType", "Second item\n\nType"]
    );
    h.close().await;
}

fn rated_card(rating: &str, price: &str) -> String {
    // books.toscrape.com: every rating draws the same five icons, whatever the class says; only the class or an
    // accessible label ever states which one it is.
    let stars = r#"<i class="icon-star"></i>"#.repeat(5);
    format!(r#"<article class="card"><h3>Book</h3><p class="star-rating {rating}">{stars}</p><p>{price}</p></article>"#)
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_class_only_star_rating_reaches_the_record() {
    let markup =
        rated_card("Five", "\u{a3}12.51") + &rated_card("Three", "\u{a3}9.99") + &rated_card("Five", "\u{a3}4.00");
    let Some(h) = icons_with(&markup).await else { return };
    let capture = content_capture(&h.page).await;
    let records: Vec<_> = capture.blocks.iter().filter(|b| b.kind == BlockKind::Record).collect();
    assert_eq!(records.len(), 3);
    let texts: Vec<String> = records.iter().map(|b| text_of(&capture, b)).collect();
    assert!(texts[0].contains("Five stars") && texts[0].contains("\u{a3}12.51"));
    assert!(texts[1].contains("Three stars") && texts[1].contains("\u{a3}9.99"));
    assert!(texts[2].contains("Five stars"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_accessible_rating_label_reaches_the_text() {
    let Some(h) = icons_with(r#"<p>Great product <span role="img" aria-label="4 out of 5 stars"></span></p>"#).await
    else {
        return;
    };
    let capture = content_capture(&h.page).await;
    assert!(texts(&capture).iter().any(|t| t.contains("4 out of 5 stars")));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_hidden_rating_widget_adds_nothing() {
    let Some(h) = icons_with(r#"<p>Item</p><p class="star-rating Five" hidden></p>"#).await else { return };
    let capture = content_capture(&h.page).await;
    assert!(!texts(&capture).iter().any(|t| t.contains("stars")));
    h.close().await;
}

fn card_with_hidden_wrapper() -> String {
    // A hidden wrapper (e.g. a template div a site keeps offscreen) holds a five-star widget; a visible
    // one-star widget follows. The hidden widget's own `hidden` check on itself passes (it isn't hidden),
    // so only checking ancestors up to the record root stops it from winning the descendant search.
    concat!(
        r#"<article class="card"><h3>Book</h3>"#,
        r#"<div hidden><p class="star-rating Five"></p></div>"#,
        r#"<p class="star-rating One"></p></article>"#,
    )
    .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_rating_hidden_by_an_ancestor_is_ignored() {
    let markup = card_with_hidden_wrapper() + &rated_card("Two", "\u{a3}1.00") + &rated_card("Three", "\u{a3}2.00");
    let Some(h) = icons_with(&markup).await else { return };
    let capture = content_capture(&h.page).await;
    let records: Vec<_> = capture.blocks.iter().filter(|b| b.kind == BlockKind::Record).collect();
    assert_eq!(records.len(), 3);
    let first = text_of(&capture, records[0]);
    assert!(!first.contains("Five stars"));
    assert!(first.contains("One stars"));
    h.close().await;
}

fn card_with_invisible_wrapper() -> String {
    // A wrapper hidden via CSS `visibility:hidden` (inherited by its children, unlike `display:none`) holds
    // a five-star widget; a visible one-star widget follows. ratingOf's own `hidden()` check only looks at
    // `hidden`/`display`, so the invisible five-star widget must be rejected by its own computed visibility.
    concat!(
        r#"<article class="card"><h3>Book</h3>"#,
        r#"<div style="visibility:hidden"><p class="star-rating Five"></p></div>"#,
        r#"<p class="star-rating One"></p></article>"#,
    )
    .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_rating_hidden_by_ancestor_visibility_is_ignored() {
    let markup = card_with_invisible_wrapper() + &rated_card("Two", "\u{a3}1.00") + &rated_card("Three", "\u{a3}2.00");
    let Some(h) = icons_with(&markup).await else { return };
    let capture = content_capture(&h.page).await;
    let records: Vec<_> = capture.blocks.iter().filter(|b| b.kind == BlockKind::Record).collect();
    assert_eq!(records.len(), 3);
    let first = text_of(&capture, records[0]);
    assert!(!first.contains("Five stars"));
    assert!(first.contains("One stars"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_arbitrary_class_naming_a_number_is_not_read_as_a_rating() {
    // Only the literal `star-rating` class is trusted; a differently named class that happens to carry a rating
    // word, or a plain icon count, must never be inferred as one.
    let Some(h) = icons_with(r#"<p class="badge Five">Featured</p>"#).await else { return };
    let capture = content_capture(&h.page).await;
    assert!(!texts(&capture).iter().any(|t| t.contains("stars")));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_nested_record_lists_keep_the_inner_records() {
    let Some(h) = harness().await else { return };
    h.page.navigate("about:blank", None).await.expect("navigate");
    let cards = "<article><p>Quotation</p><p>by Author</p></article>".repeat(3);
    set_body(&h.session, &format!("<div><p>Collection</p><section>{cards}</section></div>").repeat(3)).await;
    let capture = content_capture(&h.page).await;
    let records: Vec<_> = capture.blocks.iter().filter(|b| b.kind == BlockKind::Record).collect();
    assert_eq!(records.len(), 9);
    assert!(records.iter().all(|b| text_of(&capture, b) == "Quotation\n\nby Author"));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_record_candidates_preserve_frame_and_shadow_boundaries() {
    for boundary in ["iframe", "shadow-host", "shadow-descendant"] {
        let Some(h) = harness().await else { return };
        h.page.navigate("about:blank", None).await.expect("navigate");
        eval_active(
            &h.session,
            &format!(
                "document.body.innerHTML = '<article><p>Outer first</p><p>Outer second</p></article>'.repeat(3);
        for (const card of document.querySelectorAll('article')) {{
            const boundary = {};
            const text = '<p>Inner first</p><p>Inner second</p>';
            if (boundary === 'iframe') {{
                const frame = document.createElement('iframe');
                frame.srcdoc = text;
                card.append(frame);
            }} else {{
                const host = boundary === 'shadow-host' ? card : card.appendChild(document.createElement('div'));
                host.attachShadow({{mode: 'open'}}).innerHTML = '<slot></slot>' + text;
            }}
        }}",
                misc_support::js(boundary)
            ),
        )
        .await;
        let session = &h.session;
        wait_until(
            || async move {
                eval_active(
                    session,
                    "[...document.querySelectorAll('iframe')].every(\
                     f => f.contentDocument?.body?.innerText.includes('Inner first'))",
                )
                .await
                    == true
            },
            5.0,
        )
        .await;
        let capture = content_capture(&h.page).await;
        assert_eq!(capture.inaccessible_frames, 0, "{boundary}");
        assert!(capture.blocks.iter().all(|b| b.kind == BlockKind::Paragraph), "{boundary}");
        let inner: Vec<_> = capture.blocks.iter().filter(|b| text_of(&capture, b) == "Inner first").collect();
        assert_eq!(inner.len(), 3, "{boundary}");
        if boundary == "iframe" {
            assert!(inner.iter().all(|b| b.frame_id.as_deref().is_some_and(|f| !f.is_empty())));
            assert_eq!(inner.iter().map(|b| &b.frame_id).collect::<HashSet<_>>().len(), 3);
        } else {
            assert!(inner.iter().all(|b| b.frame_id.is_none() && b.source_id.contains("/shadow:")), "{boundary}");
            assert_eq!(
                inner.iter().map(|b| b.source_id.rsplit_once(':').unwrap().0).collect::<HashSet<_>>().len(),
                3,
                "{boundary}"
            );
        }
        h.close().await;
    }
}

/// A read of "Loading..." costs an LLM call and a second read once the page has drawn what it was loading.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_capture_waits_for_a_loading_indicator_an_action_gave_up_on() {
    let Some(h) = harness().await else { return };
    h.goto("/loading.html").await;
    visible(&h.session).await;
    let capture = content_capture(&h.page).await;
    assert!(capture.text.contains("Hello World!"));
    assert!(!capture.text.contains("Loading..."));
    h.close().await;
}

/// GitHub's empty progress bar and lazy skeletons below the fold cost every GitHub read the full wait.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_capture_does_not_wait_on_an_indicator_off_screen() {
    let Some(h) = harness().await else { return };
    h.goto("/offscreen-loading.html").await;
    let started = Instant::now();
    let capture = content_capture(&h.page).await;
    assert!(capture.text.contains("BSD-3-Clause"));
    assert!(started.elapsed().as_secs_f64() < 1.0);
    h.close().await;
}

/// Read after the whole page, a subscription form's heading followed the footer, and the reader gave the page's
/// heading above the frame as the heading the form shows.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_same_origin_frame_is_read_where_it_stands() {
    let Some(h) = harness().await else { return };
    h.page.navigate("about:blank", None).await.expect("navigate");
    eval_active(
        &h.session,
        "document.body.innerHTML = '<h1>Frames</h1><h2>Email Subscription</h2><iframe></iframe><p>Footer</p>';
        document.querySelector('iframe').srcdoc = '<p>Send updates to my inbox</p>';",
    )
    .await;
    let session = &h.session;
    wait_until(
        || async move {
            eval_active(
                session,
                "!!document.querySelector('iframe').contentDocument?.body?.innerText.includes('Send updates')",
            )
            .await
                == true
        },
        5.0,
    )
    .await;
    let capture = content_capture(&h.page).await;
    assert_eq!(texts(&capture), ["Frames", "Email Subscription", "Send updates to my inbox", "Footer"]);
    let inside = &capture.blocks[2];
    assert!(inside.frame_id.as_deref().is_some_and(|f| !f.is_empty()));
    assert_eq!(path(inside), ["Frames", "Email Subscription"]);
    assert_eq!(capture.inaccessible_frames, 0);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_navigation_headings_do_not_identify_main_content() {
    for navigation in ["nav", r#"div role="navigation""#] {
        let close = navigation.split(' ').next().unwrap();
        let markup = format!(
            "<h1>Catalog</h1><{navigation}><h2>Keyboard help</h2><p>Use arrow keys.</p></{close}>\
             <main><p>Atlas costs 12.</p></main>"
        );
        let Some(h) = icons_with(&markup).await else { return };
        let capture = content_capture(&h.page).await;
        let price = capture.blocks.iter().find(|b| text_of(&capture, b) == "Atlas costs 12.").unwrap();
        let help_text = capture.blocks.iter().find(|b| text_of(&capture, b) == "Use arrow keys.").unwrap();
        assert_eq!(path(price), ["Catalog"], "{navigation}");
        assert_eq!(path(help_text), ["Catalog", "Keyboard help"], "{navigation}");
        h.close().await;
    }
}

// -- test_snapshot_golden.py: snapshot.js output on fixed pages, against the JSON it produced when last reviewed --
//
// A speedup to snapshot.js has to leave every byte the snapshot reports alone, and these files are where a change in
// what it reports shows up as a diff. The golden files are Python's own (benchmarks/golden); after an intended change,
// rewrite them with `FASTBROWSE_UPDATE_GOLDEN=1 uv run pytest tests/browser/test_snapshot_golden.py -q`.

mod benchmark {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::thread;

    use fastbrowse_browser::page::CdpPage;
    use fastbrowse_browser::testing::misc::{fingerprint_js, page_js};
    use serde_json::Value;

    pub fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks")
    }

    /// benchmarks/fixtures.py's `PAGES`: page name to (golden n, benchmark n).
    pub const PAGES: [(&str, u32); 5] = [("links", 40), ("cards", 8), ("calendar", 2), ("forms", 4), ("frames", 6)];

    /// Every frame has finished loading, so the snapshot sees the same-origin frame's controls.
    const LOADED_JS: &str = "new Promise(resolve => { const poll = () => [document, ...[...document.querySelectorAll('iframe')]\
        .map(f => f.contentDocument)].every(d => d?.readyState === 'complete') ? resolve(true) : setTimeout(poll, 10); \
        poll(); })";

    /// `serve()`: benchmarks/sites on a local port, typed by extension as `SimpleHTTPRequestHandler` types them, since
    /// Chrome ignores a stylesheet served as HTML and the snapshot would see another layout.
    pub fn origin() -> &'static str {
        static ORIGIN: OnceLock<String> = OnceLock::new();
        ORIGIN.get_or_init(|| {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let sites = root().join("sites");
            thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let sites = sites.clone();
                    thread::spawn(move || {
                        let mut stream = stream;
                        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                        let mut request = String::new();
                        if reader.read_line(&mut request).is_err() {
                            return;
                        }
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                                break;
                            }
                        }
                        let target = request.split_whitespace().nth(1).unwrap_or("/");
                        let file = sites.join(target.split('?').next().unwrap_or("/").trim_start_matches('/'));
                        let mime = match file.extension().and_then(|e| e.to_str()) {
                            Some("css") => "text/css",
                            Some("js") => "text/javascript",
                            _ => "text/html",
                        };
                        let (status, body) = match std::fs::read(&file) {
                            Ok(body) if file.is_file() => ("200 OK", body),
                            _ => ("404 Not Found", vec![]),
                        };
                        let head = format!(
                            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&body);
                    });
                }
            });
            format!("http://127.0.0.1:{port}")
        })
    }

    async fn evaluate(page: &CdpPage, expression: &str) -> Value {
        let session_id = page.session().active_session_id().expect("active tab");
        page.evaluate(&session_id, expression).await.expect("evaluate")
    }

    pub async fn open_page(page: &CdpPage, origin: &str, name: &str, n: u32) {
        page.navigate(&format!("{origin}/{name}.html?n={n}"), None).await.expect("navigate");
        evaluate(page, LOADED_JS).await;
    }

    pub async fn snapshot(page: &CdpPage) -> Value {
        evaluate(page, &format!("{}('snapshot')", page_js(page))).await
    }

    /// The guards themselves, which the snapshot sends to Python only as digests.
    pub async fn guards(page: &CdpPage, ids: &[Value]) -> Value {
        let ids = serde_json::to_string(ids).unwrap();
        evaluate(
            page,
            &format!("(r => Object.fromEntries({ids}.map(id => [id, r.guard(r.nodes.get(id))])))(window.__fastbrowse)"),
        )
        .await
    }

    pub async fn fingerprint(page: &CdpPage) -> Value {
        evaluate(page, fingerprint_js(page)).await
    }

    /// What a run on another port or at another moment would equally produce.
    fn normalize(value: Value, origin: &str) -> Value {
        match value {
            Value::String(s) => Value::String(s.replace(origin, "http://fixture")),
            Value::Array(items) => Value::Array(items.into_iter().map(|v| normalize(v, origin)).collect()),
            Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, normalize(v, origin))).collect()),
            other => other,
        }
    }

    /// The snapshot as the golden file records it. The document's time origin is a clock reading, and the digests
    /// hash the fixture server's port, so the guards are recorded whole in their place.
    pub fn golden_view(result: Value, guards: Value, fingerprint: Value, origin: &str) -> Value {
        let Value::Object(mut view) = result else { panic!("snapshot is not an object: {result}") };
        for digest in ["document_key", "page_key", "retarget_keys"] {
            view.remove(digest);
        }
        // A guard holds its document's `performance.timeOrigin` at index 15.
        let Value::Object(guards) = guards else { panic!("guards are not an object: {guards}") };
        let guards = guards
            .into_iter()
            .map(|(k, g)| {
                let g = match g {
                    Value::Array(items) => {
                        let mut kept: Vec<Value> = items.iter().take(15).cloned().collect();
                        kept.push(Value::Null);
                        kept.extend(items.into_iter().skip(16));
                        Value::Array(kept)
                    }
                    other => other,
                };
                (k, g)
            })
            .collect();
        view.insert("guards".into(), Value::Object(guards));
        view.insert("fingerprint".into(), fingerprint);
        normalize(Value::Object(view), origin)
    }

    /// Python's `==` on parsed JSON: 1 equals 1.0, which serde_json's `Value` keeps apart.
    pub fn same(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
            (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y)),
            (Value::Object(x), Value::Object(y)) => {
                x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
            }
            _ => a == b,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_snapshot_matches_golden() {
    for (name, n) in benchmark::PAGES {
        let Some(h) = harness().await else { return };
        let origin = benchmark::origin();
        benchmark::open_page(&h.page, origin, name, n).await;
        let result = benchmark::snapshot(&h.page).await;
        let ids: Vec<_> = result["controls"].as_array().unwrap().iter().map(|c| c["id"].clone()).collect();
        let raw = benchmark::guards(&h.page, &ids).await;
        let fingerprint = benchmark::fingerprint(&h.page).await;
        let view = benchmark::golden_view(result, raw, fingerprint, origin);
        let golden_path = benchmark::root().join("golden").join(format!("{name}.json"));
        let golden: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&golden_path).expect("golden file")).expect("golden JSON");
        assert!(
            benchmark::same(&view, &golden),
            "{name}: snapshot differs from {}\n{}",
            golden_path.display(),
            serde_json::to_string_pretty(&view).unwrap()
        );
        h.close().await;
    }
}

// -- test_snapshot_reuse.py: snapshot.js reuses what it reads within one call; a guard read in a later call sees the
// page as it is then ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_a_guard_after_the_snapshot_reads_the_changed_page() {
    for change in ["label", "card"] {
        let Some(h) = harness().await else { return };
        benchmark::open_page(&h.page, benchmark::origin(), "cards", 4).await;
        let result = benchmark::snapshot(&h.page).await;
        let target = result["controls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["label"] == "Details" && c["context"] == "Product 1")
            .unwrap()
            .clone();
        // The label is read through the reused label, and the card's text through the reused innerText.
        let edit = if change == "label" {
            "e.textContent = 'More'"
        } else {
            "e.closest('.card').querySelector('.price').textContent = '$1.00'"
        };
        let id = &target["id"];
        let session_id = h.session.active_session_id().unwrap();
        let changed = h
            .page
            .evaluate(
                &session_id,
                &format!(
                    "(() => {{ const r = window.__fastbrowse; const e = r.nodes.get({id}); {edit}; \
                     return r.guardKey(e) !== r.observed.get({id}); }})()"
                ),
            )
            .await
            .expect("evaluate");
        assert_eq!(changed, true, "{change}: {target}");
        h.close().await;
    }
}

// -- test_snapshot_caps.py: the snapshot sends whole rows only within the observation caps; the observation is the
// one sending all would give ---------------------------------------------------------------------------------------

fn capped_config() -> fastbrowse_browser::shared::Config {
    let mut config = fastbrowse_browser::shared::Config::default();
    config.observation.max_controls = 12;
    config.observation.max_offscreen_controls = 4;
    config
}

#[tokio::test(flavor = "multi_thread")]
async fn test_capped_snapshot_observes_what_an_uncapped_one_would() {
    use fastbrowse_browser::session::SessionOptions;
    use fastbrowse_browser::testing::misc::{last_controls, page_js, set_page_js};
    for name in ["links", "cards", "forms", "frames"] {
        let Some(mut h) = common::harness_with(capped_config(), SessionOptions::default()).await else { return };
        benchmark::open_page(&h.page, benchmark::origin(), name, 200).await;
        // A load-more button and a next-page link at the foot of the page, past both caps, which only a pager keeps.
        eval_active(
            &h.session,
            "(() => { const foot = document.createElement('div'); foot.style.marginTop = '3000px'; \
             foot.innerHTML = '<button>Show 20 more</button><a href=\"/page/3\">Next page</a>'; \
             document.body.append(foot); })()",
        )
        .await;
        let capped = h.observe().await;
        let capped_state = last_controls(&h.page);

        let sends_all = format!("((mode) => ({})(mode))", page_js(&h.page));
        set_page_js(&mut h.page, sends_all);
        let whole = h.observe().await;
        let whole_state = last_controls(&h.page);

        assert!(capped.controls.iter().any(|c| c.label == "Show 20 more"), "{name}");
        assert!(capped.controls.iter().any(|c| c.label == "Next page"), "{name}");
        assert_eq!(capped.controls, whole.controls, "{name}");
        assert_eq!(capped.omitted_controls, whole.omitted_controls, "{name}");
        assert_eq!(capped.page_key, whole.page_key, "{name}");
        assert_eq!(capped_state, whole_state, "{name}");
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_rows_lost_before_they_are_read_make_observe_read_the_page_again() {
    use fastbrowse_browser::session::SessionOptions;
    use fastbrowse_browser::testing::misc::{page_js, set_page_js};
    let Some(mut h) = common::harness_with(capped_config(), SessionOptions::default()).await else { return };
    benchmark::open_page(&h.page, benchmark::origin(), "links", 200).await;
    // As if another snapshot, or a new document, had come between the read and the fetch: the first snapshot's kept
    // rows are gone by the time observe asks for them.
    let losing = format!(
        "((mode, limits) => {{ const out = ({})(mode, limits); \
         if (mode === 'snapshot' && !window.__lost) {{ window.__lost = 1; delete window.__fastbrowse.sent; }} \
         return out; }})",
        page_js(&h.page)
    );
    set_page_js(&mut h.page, losing);
    let observation = h.observe().await;
    assert_eq!(eval_active(&h.session, "window.__lost").await, serde_json::json!(1));
    assert!(observation.controls.iter().any(|c| c.next_page == Some(true)));
    h.close().await;
}

// -- test_snapshot_install.py: snapshot.js is compiled once per document, and a document that never installed it
// still gets observed ----------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn test_a_new_document_installs_snapshot_as_it_starts() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    let installed = eval_active(&h.session, "Object.keys(globalThis.__fastbrowseScripts || {}).length").await;
    assert_eq!(installed, serde_json::json!(1));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_document_without_snapshot_installs_it_on_the_first_call() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    // As a document older than the session would be, or one whose script removed the global.
    eval_active(&h.session, "delete globalThis.__fastbrowseScripts").await;
    let observation = h.observe().await;
    assert!(!observation.controls.is_empty());
    assert_eq!(eval_active(&h.session, "!!globalThis.__fastbrowseScripts").await, serde_json::json!(true));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_settle_waits_for_a_shadow_root_added_while_it_reads_the_clock() {
    let Some(h) = harness().await else { return };
    // A hidden tab returns one sample instead of waiting for quiet.
    visible(&h.session).await;
    h.page.navigate(&sites().main, None).await.expect("navigate");
    // The host lands in the light DOM once; everything after happens inside its shadow root, which the document's
    // observer cannot see until a full sample starts watching it.
    eval_active(
        &h.session,
        "(() => { const host = document.createElement('div'); document.body.append(host); \
         const root = host.attachShadow({mode: 'open'}); let n = 0; \
         const tick = () => { root.textContent = String(n++); if (n < 16) setTimeout(tick, 50); }; tick(); })()",
    )
    .await;
    let started = Instant::now();
    let (stable, _) = h.page.settled_fingerprint(3.0).await.expect("settle");
    assert!(stable);
    // 16 changes 50ms apart end about 750ms in; settling needs 200ms of quiet after the last.
    assert!(started.elapsed().as_secs_f64() >= 0.8);
    h.close().await;
}
