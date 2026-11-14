mod common;
mod misc_support;

use std::collections::HashSet;

use common::{Harness, eval_active, harness, sites, wait_until};
use fastbrowse_core::page::BlockKind;
use misc_support::{content_capture, path, set_body, text_of, texts};

// -- test_capture.py: capture blocks preserve the records a reader needs to cite together -----------------------

/// This binary registers no PDF helper, as a Rust program that never calls `run_if_requested` does not. The PDF is
/// still kept, and the run is told its text went unread instead of seeing a file that seems to hold nothing.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_pdf_with_no_helper_to_read_it_is_kept_and_said_unread() {
    let Some(h) = harness().await else { return };
    h.goto("/map.pdf").await;
    let url = format!("{}/map.pdf", sites().main);
    let glance = h.page.glance().await.expect("PDF safety reading");
    assert!(!glance.viewport_text.contains("The file was kept, but its text was not read"));
    let text = h.observe().await.viewport_text;
    let notice = format!(
        "[Downloaded file: map.pdf from {url}] The file was kept, but its text was not read: this program has no document \
         reader set up."
    );
    assert!(text.contains(&notice), "{text}");
    assert!(!text.contains("Fairbanks Wind"), "{text}");
    assert!(h.sink.artifacts.lock().unwrap().iter().any(|artifact| artifact.name == "map.pdf"));
    h.close().await;
}

const PACKAGE_JSON: &str =
    r#"{"info":{"name":"httpx","version":"0.28.1"},"urls":[{"filename":"httpx-0.28.1.tar.gz"}]}"#;

/// A JSON document is read by its structure, as a browserless fetch reads it, so a single value is its own block (#1070).
#[tokio::test(flavor = "multi_thread")]
async fn test_a_json_document_reads_each_value_under_its_keys() {
    let Some(h) = harness().await else { return };
    h.goto("/package.json").await;
    let capture = content_capture(&h.page).await;
    let read: Vec<_> = capture.blocks.iter().map(|b| (b.kind, text_of(&capture, b), path(b).join("/"))).collect();
    let (heading, paragraph) = (BlockKind::Heading, BlockKind::Paragraph);
    assert_eq!(
        read,
        [
            (heading, "info".into(), String::new()),
            (heading, "name".into(), "info".into()),
            (paragraph, "httpx".into(), "info/name".into()),
            (heading, "version".into(), "info".into()),
            (paragraph, "0.28.1".into(), "info/version".into()),
            (heading, "urls".into(), String::new()),
            (BlockKind::Record, "filename: httpx-0.28.1.tar.gz".into(), "urls/0".into()),
        ]
    );
    assert!(capture.blocks.iter().all(|b| b.source_id.starts_with("main/json:") && b.frame_id.is_none()));
    assert!(capture.blocks.last().unwrap().whole_record);
    h.close().await;
}

/// Text served as JSON that does not all parse is read as Chrome renders it, so none of it is lost.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_json_typed_document_that_does_not_parse_is_read_as_rendered() {
    let Some(h) = harness().await else { return };
    h.goto("/package.json?broken").await;
    let capture = content_capture(&h.page).await;
    let code: Vec<_> = capture.blocks.iter().filter(|b| b.kind == BlockKind::Code).collect();
    assert_eq!(code.iter().map(|b| text_of(&capture, b)).collect::<Vec<_>>(), [r#"{"info":{"name":"httpx"}"#]);
    assert!(capture.blocks.iter().all(|b| !b.source_id.contains("/json:")));
    h.close().await;
}

/// A JSON example inside an HTML page is the page's code, not a JSON document.
#[tokio::test(flavor = "multi_thread")]
async fn test_json_in_an_html_pre_stays_one_code_block() {
    let Some(h) = harness().await else { return };
    h.page.navigate(&sites().main, None).await.expect("navigate");
    set_body(&h.session, &format!("<pre>{PACKAGE_JSON}</pre>")).await;
    let capture = content_capture(&h.page).await;
    assert_eq!(texts(&capture), [PACKAGE_JSON]);
    assert_eq!(capture.blocks[0].kind, BlockKind::Code);
    h.close().await;
}

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
    let cases: [(&str, &str, &[&str]); 12] = [
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
            "icon-library-only-the-snapshot-knew",
            "<tr><th>Name</th><th>Ok</th></tr><tr><td>Ada</td><td><i class='lucide lucide-check'></i></td></tr>",
            &["| Name | Ok |\n| --- | --- |\n| Ada | [check icon] |"],
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
    assert!(capture.blocks.iter().all(|b| b.kind == BlockKind::Table));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_records_keep_single_links_whatever_their_length() {
    for (tag, kind) in [("div", BlockKind::Record), ("li", BlockKind::ListItem)] {
        let cards: String = [40, 40, 5000]
            .iter()
            .map(|length| {
                format!(r#"<{tag} class="card"><div>{}</div><a href="/author">Author</a></{tag}>"#, "x".repeat(*length))
            })
            .collect();
        let Some(h) = icons_with(&format!("<ul>{cards}</ul>")).await else { return };
        let capture = content_capture(&h.page).await;
        assert_eq!(capture.blocks.iter().map(|b| b.kind).collect::<Vec<_>>(), [kind, kind, kind], "{tag}");
        assert!(capture.blocks.iter().all(|b| b.href.as_deref() == Some("/author")), "{tag}");
        h.close().await;
    }
}

/// #992: CurseForge renders each comment as an `li` holding its text, its actions and an open replies region with nothing
/// in it. The comment is a `list_item` read whole, and the capture marks it so; a plain list item is not marked.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_list_item_record_is_marked_whole() {
    let comment = |name: &str, body: &str| {
        format!(
            "<li class='comment'><div class='meta'><a href='/m/{name}'>{name}</a><span>2 minutes ago</span></div>\
             <div class='body'><p>{body}</p></div><div class='actions'><button>Reply</button></div>\
             <div class='replies-collapse expanded'><ul class='comments-list'></ul></div></li>"
        )
    };
    let comments = [comment("ada", "it broke"), comment("bob", "thanks"), comment("cy", "nice")].concat();
    let markup = format!("<ul class='comments-list'>{comments}</ul><ul><li>Plain</li><li>Items</li></ul>");
    let Some(h) = icons_with(&markup).await else { return };
    let capture = content_capture(&h.page).await;
    let marked: Vec<_> =
        capture.blocks.iter().filter(|b| b.whole_record).map(|b| (b.kind, text_of(&capture, b))).collect();
    assert_eq!(marked.len(), 3, "{:?}", capture.blocks);
    assert!(marked.iter().all(|(kind, _)| *kind == BlockKind::ListItem), "{marked:?}");
    assert!(marked[0].1.ends_with("[replies-collapse expanded: no text]"), "{marked:?}");
    h.close().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn test_sibling_records_do_not_inherit_an_earlier_records_heading() {
    for level in [3, 5] {
        let markup = include_str!("fixtures/sibling-record-headings.html")
            .replace("<h3>", &format!("<h{level}>"))
            .replace("</h3>", &format!("</h{level}>"));
        let Some(h) = icons_with(&markup).await else {
            return;
        };
        let capture = content_capture(&h.page).await;
        let records: Vec<_> = capture.blocks.iter().filter(|block| block.whole_record).collect();
        assert_eq!(records.len(), 3);
        for (record, version) in records.iter().zip(["3.0", "2.0", "1.0"]) {
            assert!(text_of(&capture, record).contains(&format!("Version {version}")));
            assert_eq!(path(record), ["Package", "Release history"]);
        }
        let after = capture.blocks.iter().find(|block| text_of(&capture, block) == "After the releases.").unwrap();
        assert_eq!(path(after), ["Package", "Release history"]);
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_record_heading_boundaries_follow_visible_heading_ranks() {
    let cases = [
        ("<h2>Group</h2><h3>Earlier</h3><h3></h3><p>After</p>".to_string(), vec!["Group"]),
        (format!("<h2>Group</h2>{}<p>After</p>", "<div class='card'><h2></h2><p>Value</p></div>".repeat(3)), vec![]),
        ("<h2>Group</h2><h4>Earlier</h4><h3>Later</h3><p>After</p>".to_string(), vec!["Group", "Later"]),
        (
            format!("<h2>Group</h2>{}<p>After</p>", "<div class='card'><h2>Item</h2><p>Value</p></div>".repeat(3)),
            vec![],
        ),
        (
            format!(
                "<h2>Group</h2>{}<p>After</p>",
                "<div class='card'><span hidden><h2>Hidden</h2></span><p>Value</p></div>".repeat(3)
            ),
            vec!["Group"],
        ),
    ];
    for (markup, expected) in cases {
        let Some(h) = icons_with(&markup).await else { return };
        let capture = content_capture(&h.page).await;
        let after = capture.blocks.iter().find(|block| text_of(&capture, block) == "After").unwrap();
        assert_eq!(path(after), expected, "{markup}");
        h.close().await;
    }
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

/// A footnote reference points at a note elsewhere, so the cell it marks reads without it (#862). An in-page link
/// whose target does not link back to it, such as a table of contents entry, is page text and stays.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_footnote_reference_is_not_read_as_part_of_the_text_it_marks() {
    let Some(h) = harness().await else { return };
    h.page.navigate("about:blank", None).await.expect("navigate");
    let markup = r##"<div id="top"><p><a href="#history">[History]</a></p>
        <table><tr><th>Population</th>
        <td>41,798,407<sup id="cite_ref-3" class="reference"><a href="#cite_note-3"><span>[</span>3<span>]</span></a></sup> (<a href="/rank">37th</a>)</td></tr>
        <tr><th>Completed</th><td>31 March 1889<sup id="cite_ref-:0_1-0"><a href="#cite_note-:0-1">[1]</a></sup></td></tr></table>
        <p>Dune is a novel.<sup><a href="#fn1" role="doc-noteref">2</a></sup> It sold well.</p></div>
        <h2 id="history">History</h2><p><a href="#top">Back to top</a></p>
        <ol><li id="cite_note-3"><a href="#cite_ref-3">^</a> Statistics Canada.</li>
        <li id="cite_note-:0-1"><a href="#cite_ref-:0_1-0"><sup>a</sup></a> Tour Eiffel.</li></ol>"##;
    set_body(&h.session, markup).await;
    let read = texts(&content_capture(&h.page).await);
    assert!(read.contains(&"| Population | 41,798,407 (37th) |".to_string()), "{read:?}");
    assert!(read.iter().any(|text| text.ends_with("\n| Completed | 31 March 1889 |")), "{read:?}");
    assert!(read.contains(&"Dune is a novel. It sold well.".to_string()), "{read:?}");
    assert!(read.contains(&"[History]".to_string()) && read.contains(&"Back to top".to_string()), "{read:?}");
    assert!(read.iter().any(|text| text.contains("Statistics Canada.")), "{read:?}");
    h.close().await;
}

/// A link's text says neither where it leads nor where a value the page set apart as its own element ends, so each
/// link inside a block is kept with its text's place in the capture and its address, as a fetched page's are.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_link_inside_a_block_keeps_its_text_and_address() {
    let Some(h) = harness().await else { return };
    h.goto("/icons.html").await;
    let markup = "<p>Support me on <a href=\"https://ko-fi.com/x\">Ko-fi</a> or <a href=\"/sponsor\">GitHub</a>.</p>\
         <table><tr><th>Name</th><th>Price</th></tr><tr><td><a href=\"/p/1\">Mug</a></td><td>8.75</td></tr></table>\
         <p><a href=\"javascript:void(0)\">Menu</a> and <a href=\"/shop\">the shop</a></p>\
         <ul><li><a href=\"/product/7\">Paper Filters</a> - GBP 4.60</li><li><a href=\"/product/8\">Bar Cloth</a> - GBP 6.20</li>\
         <li><a href=\"/product/9\">Drip Tray</a> - GBP 9.95</li></ul>";
    set_body(&h.session, markup).await;
    let capture = content_capture(&h.page).await;
    let origin = capture.url.split("/icons.html").next().unwrap().to_string();
    for link in capture.blocks.iter().flat_map(|block| &block.links) {
        let shown: String =
            capture.text.chars().skip(link.start as usize).take((link.end - link.start) as usize).collect();
        assert_eq!(shown, link.text, "a link's offsets are where its text is");
    }
    let links: Vec<(&str, String)> = capture
        .blocks
        .iter()
        .flat_map(|block| &block.links)
        .map(|link| (link.text.as_str(), link.href.replace(&origin, "")))
        .collect();
    assert_eq!(
        links,
        [
            ("Ko-fi", "https://ko-fi.com/x".to_string()),
            ("GitHub", "/sponsor".to_string()),
            ("Mug", "/p/1".to_string()),
            ("the shop", "/shop".to_string()),
            ("Paper Filters", "/product/7".to_string()),
            ("Bar Cloth", "/product/8".to_string()),
            ("Drip Tray", "/product/9".to_string()),
        ],
        "a script link leads nowhere"
    );
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
                common::js(boundary)
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

/// Read after the whole page, a subscription form's heading followed the footer, and the reader gave the page's
/// heading above the frame as the heading the form shows. The form's own h1 stays under the page's headings above the
/// frame: ranked against them, it ended them, and its note no longer said which form it heads.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_same_origin_frame_is_read_where_it_stands() {
    let Some(h) = harness().await else { return };
    h.page.navigate("about:blank", None).await.expect("navigate");
    eval_active(
        &h.session,
        "document.body.innerHTML = '<h1>Frames</h1><h2>Email Subscription</h2><iframe></iframe><p>Footer</p>';
        document.querySelector('iframe').srcdoc = '<h1>Send updates to my inbox</h1><p>Your email</p>';",
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
    assert_eq!(texts(&capture), ["Frames", "Email Subscription", "Send updates to my inbox", "Your email", "Footer"]);
    let inside = &capture.blocks[2];
    assert!(inside.frame_id.as_deref().is_some_and(|f| !f.is_empty()));
    assert_eq!(path(inside), ["Frames", "Email Subscription"]);
    assert_eq!(path(&capture.blocks[3]), ["Frames", "Email Subscription", "Send updates to my inbox"]);
    assert_eq!(path(&capture.blocks[4]), ["Frames", "Email Subscription"]);
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

// -- snapshot.js output on fixed pages, against the JSON it produced when last reviewed ----------------------------
//
// A speedup to snapshot.js has to leave every byte the snapshot reports alone, and these files are where a change in
// what it reports shows up as a diff. After an intended change, rewrite benchmarks/golden and review the diff:
// `FASTBROWSE_UPDATE_GOLDEN=1 cargo test -p fastbrowse-browser --test capture test_snapshot_matches_golden`.

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

    /// Page name to (golden n, benchmark n), as benchmarks/fixtures.py's `PAGES`.
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
    /// `view` as the golden files are written: one-space indents and sorted keys, ending in a newline.
    pub fn write_golden(path: &Path, view: &Value) {
        fn sorted(value: &Value) -> Value {
            match value {
                Value::Object(map) => {
                    let mut keys: Vec<_> = map.keys().collect();
                    keys.sort();
                    Value::Object(keys.into_iter().map(|k| (k.clone(), sorted(&map[k]))).collect())
                }
                Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
                other => other.clone(),
            }
        }
        let mut written = Vec::new();
        let mut serializer =
            serde_json::Serializer::with_formatter(&mut written, serde_json::ser::PrettyFormatter::with_indent(b" "));
        serde::Serialize::serialize(&sorted(view), &mut serializer).expect("golden JSON");
        written.push(b'\n');
        std::fs::write(path, written).expect("golden file");
    }

    pub fn golden_view(result: Value, guards: Value, fingerprint: Value, origin: &str) -> Value {
        let Value::Object(mut view) = result else { panic!("snapshot is not an object: {result}") };
        for digest in ["document_key", "page_key", "retarget_keys", "read"] {
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
        if std::env::var("FASTBROWSE_UPDATE_GOLDEN").as_deref() == Ok("1") {
            benchmark::write_golden(&golden_path, &view);
        }
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
async fn test_a_kept_capture_goes_to_the_sessions_artifact_sink() {
    use fastbrowse_core::models::ArtifactKind;
    use fastbrowse_core::page::Page;
    let Some(h) = harness().await else { return };
    h.goto("/index.html").await;
    let capture = h.page.capture().await.expect("capture");
    let kept = Page::keep_capture(&h.page, "capture-index.txt", &capture.text).await.expect("kept").expect("a sink");
    assert_eq!((kept.kind, kept.size_bytes), (ArtifactKind::Capture, capture.text.len() as i64));
    assert_eq!(kept.sha256, fastbrowse_core::py::sha256_hex(capture.text.as_bytes()));
    assert!(h.sink.artifacts.lock().unwrap().contains(&kept));
    h.close().await;
}
