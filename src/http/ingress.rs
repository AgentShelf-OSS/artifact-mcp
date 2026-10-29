//! Origin-side request admission controls.
//!
//! The implementation is kept in child modules so this path remains the stable public surface.

mod admission;

pub use admission::*;
