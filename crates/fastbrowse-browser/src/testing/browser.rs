//! Test seams added for the browser suites; see the module docs of `testing`.

use crate::session::BrowserSession;

/// `await asyncio.shield(session._popups[target][1])`: whether the popup was adopted, once that is known. False for
/// a target the session never began as a popup.
pub async fn popup_adopted(session: &BrowserSession, target_id: &str) -> bool {
    let adopted = {
        let state = super::shared(session).state.lock().unwrap();
        state.popups.iter().find(|p| p.target_id == target_id).map(|p| p.adopted.subscribe())
    };
    let Some(mut adopted) = adopted else { return false };
    adopted.wait_for(Option::is_some).await.map(|v| *v == Some(true)).unwrap_or(false)
}
