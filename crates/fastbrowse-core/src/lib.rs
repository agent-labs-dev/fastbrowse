//! The fastbrowse core, ported module by module from `src/fastbrowse`.
//!
//! URLs follow the WHATWG URL Standard (the `url` crate, UTS #46 hosts), as Chrome parses them, rather than
//! CPython's `urlsplit`. scripts/rust_core_diff.py reports where the two disagree.

pub mod citations;
pub mod origins;
