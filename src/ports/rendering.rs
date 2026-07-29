//! Owned by U01 (sol) — compile-time page rendering contract.

use crate::{
    error::AppError,
    render::view_models::{GalleryView, SettingsView, ShellView},
};

pub trait PageRenderer: Send + Sync {
    fn gallery(&self, view: &GalleryView) -> Result<String, AppError>;
    fn shell(&self, view: &ShellView) -> Result<String, AppError>;
    fn settings(&self, view: &SettingsView) -> Result<String, AppError>;
    fn not_found(&self, message: Option<&str>) -> Result<String, AppError>;
    fn not_signed_in(&self) -> Result<String, AppError>;
    fn access_retry(&self, target: &str) -> Result<String, AppError>;
}
