//! Public facade for thumbnail integration types and helpers.
//!
//! The implementation is split by responsibility under this module while preserving the
//! established `integrations::thumbnails::*` API.

mod implementation;

pub use implementation::*;

#[cfg(test)]
mod characterization;
