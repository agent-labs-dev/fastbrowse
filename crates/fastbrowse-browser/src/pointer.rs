use std::time::Instant;

use fastbrowse_core::models::Operation;

#[derive(Debug, Clone, Copy)]
pub struct PointerMark {
    pub at: Instant,
    pub x: f64,
    pub y: f64,
    pub operation: Operation,
    pub width: f64,
    pub height: f64,
}
