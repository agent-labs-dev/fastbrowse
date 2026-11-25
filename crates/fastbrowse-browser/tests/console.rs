//! The CurseForge author console journeys over CDP; `fastbrowse-cli/tests/extension_backend.rs` runs the same ones
//! through the extension.

mod common;
mod console_support;

use common::{harness, sites};

#[tokio::test(flavor = "multi_thread")]
async fn test_a_description_is_replaced_and_saved_through_its_controlled_editor() {
    let Some(h) = harness().await else { return };
    console_support::description(&h.page, &h.session, &sites().main).await;
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_reply_is_posted_once_in_the_thread_it_answers() {
    let Some(h) = harness().await else { return };
    console_support::reply(&h.page, &h.session, &sites().main).await;
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_gallery_image_is_deleted_through_its_hover_revealed_control() {
    let Some(h) = harness().await else { return };
    console_support::gallery(&h.page, &h.session, &sites().main).await;
    h.close().await;
}
