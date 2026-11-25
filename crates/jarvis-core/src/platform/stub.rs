//! Placeholder backend for non-Windows targets.
//!
//! It exists so the crate builds and its tests run on any machine; every OS
//! action reports clearly that the platform is not supported yet, rather than
//! silently doing nothing.

use super::{Platform, PlatformFuture};

pub struct UnsupportedPlatform;

fn unsupported() -> anyhow::Error {
    anyhow::anyhow!(
        "desktop control is only implemented for Windows in this build; \
         run Jarvis on Windows or add a platform backend for this OS"
    )
}

macro_rules! unsupported_method {
    ($name:ident, $($arg:ident : $ty:ty),*) => {
        fn $name<'a>(&'a self, $($arg: $ty),*) -> PlatformFuture<'a> {
            $(let _ = $arg;)*
            Box::pin(async move { Err::<String, _>(unsupported()) })
        }
    };
}

impl Platform for UnsupportedPlatform {
    fn name(&self) -> &'static str {
        std::env::consts::OS
    }

    unsupported_method!(open_app, name: &'a str);
    unsupported_method!(close_app, target: &'a str);
    unsupported_method!(focus_window, target: &'a str);
    unsupported_method!(type_text, text: &'a str);
    unsupported_method!(press_keys, combo: &'a str);

    fn list_windows(&self) -> PlatformFuture<'_> {
        Box::pin(async { Err(unsupported()) })
    }

    fn click(&self, _x: i32, _y: i32) -> PlatformFuture<'_> {
        Box::pin(async { Err(unsupported()) })
    }

    fn screenshot(&self) -> PlatformFuture<'_> {
        Box::pin(async { Err(unsupported()) })
    }
}
