//! What Python and pydantic do implicitly, made explicit so the ported modules produce the same bytes.

pub mod base64;
pub mod datetime;
pub mod dump;
pub mod json;
pub mod lax;
pub mod text;

use sha2::{Digest, Sha256};

/// `sum(values)` over floats. CPython 3.12 sums floats with Neumaier's compensation, so a plain fold can differ from
/// Python's total in the last place, and a total is what a dollar cap compares.
pub fn sum(values: impl IntoIterator<Item = f64>) -> f64 {
    let mut total = 0.0_f64;
    let mut compensation = 0.0_f64;
    for x in values {
        let t = total + x;
        if total.abs() >= x.abs() {
            compensation += (total - t) + x;
        } else {
            compensation += (x - t) + total;
        }
        total = t;
    }
    if compensation != 0.0 && compensation.is_finite() { total + compensation } else { total }
}

/// `round(value, digits)`: correctly rounded from the exact binary value, ties to even, as CPython does.
pub fn round(value: f64, digits: usize) -> f64 {
    if !value.is_finite() {
        return value;
    }
    format!("{value:.digits$}").parse().unwrap_or(value)
}

/// `hashlib.sha256(data).hexdigest()`.
pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}
