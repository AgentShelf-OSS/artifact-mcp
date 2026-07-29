//! U01 contract root for the artifact-mcp Rust rebuild.

#![forbid(unsafe_code)]

pub mod app;
pub mod artifacts;
pub mod config;
pub mod error;
pub mod http;
pub mod integrations;
pub mod mcp;
pub mod model;
pub mod observability;
pub mod persistence;
pub mod ports;
pub mod render;
pub mod security;

pub use app::{AppDeps, build_router};
