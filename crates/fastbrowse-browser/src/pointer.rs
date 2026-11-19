use std::time::Instant;

use fastbrowse_core::models::Operation;

#[derive(Debug, Clone, Copy)]
pub struct PointerMark {
    pub at: Instant,
    pub x: f64,
    pub y: f64,
    pub operation: Operation,
    /// Whether the action pressed the mouse button at this point. Set where the action is produced: a click, a fill
    /// that clicks the field before typing, and a drag press; a hover only moves the pointer.
    pub pressed: bool,
    pub width: f64,
    pub height: f64,
}
