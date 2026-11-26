//! Everything that touches the operating system sits behind [`Platform`].
//!
//! Only Windows is implemented for v1. The trait exists so the agent loop,
//! policy and tests never depend on the OS, and so a macOS or Linux backend is
//! an added file rather than a rewrite.

pub mod combo;
pub mod input;
pub mod recorder;
pub mod screenshot;
pub mod transcribe;

#[cfg(not(windows))]
mod stub;
#[cfg(windows)]
mod win;

use std::sync::Arc;

use anyhow::Result;
use futures_util::future::BoxFuture;

/// Result text for the model; empty when the action has nothing to report.
pub type PlatformFuture<'a> = BoxFuture<'a, Result<String>>;

pub trait Platform: Send + Sync {
    /// Human name of the backend, shown to the model so it knows what it can do.
    fn name(&self) -> &'static str;

    fn open_app<'a>(&'a self, name: &'a str) -> PlatformFuture<'a>;
    fn close_app<'a>(&'a self, target: &'a str) -> PlatformFuture<'a>;
    fn focus_window<'a>(&'a self, target: &'a str) -> PlatformFuture<'a>;
    fn list_windows(&self) -> PlatformFuture<'_>;
    fn type_text<'a>(&'a self, text: &'a str) -> PlatformFuture<'a>;
    fn press_keys<'a>(&'a self, combo: &'a str) -> PlatformFuture<'a>;
    fn click(&self, x: i32, y: i32) -> PlatformFuture<'_>;
    /// Full-screen capture, returned as a `data:image/jpeg;base64,...` URL.
    fn screenshot(&self) -> PlatformFuture<'_>;
    /// Wheel notches at the pointer, positive = down. Moves the cursor first
    /// when a position is given, because the wheel goes wherever it points.
    fn scroll(&self, clicks: i32, position: Option<(i32, i32)>) -> PlatformFuture<'_>;
    /// Text on the clipboard; empty when there is nothing on it.
    fn get_clipboard(&self) -> PlatformFuture<'_>;
    /// Replace the clipboard with `text`.
    fn set_clipboard<'a>(&'a self, text: &'a str) -> PlatformFuture<'a>;
}

pub fn current() -> Arc<dyn Platform> {
    #[cfg(windows)]
    {
        Arc::new(win::WindowsPlatform)
    }
    #[cfg(not(windows))]
    {
        Arc::new(stub::UnsupportedPlatform)
    }
}
