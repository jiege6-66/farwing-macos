//! Windows GUI ownership and shared application controls.
//!
//! The shell is gated with `#[cfg(windows)]` until the native macOS device center arrives in M5.
pub(crate) mod branding;
pub(crate) mod controls;
pub(crate) mod theme;

#[cfg(windows)]
mod app;
#[cfg(windows)]
pub(crate) mod chrome;
#[cfg(windows)]
pub(crate) mod fonts;
#[cfg(windows)]
pub(crate) mod tray;
#[cfg(windows)]
pub(crate) mod video_textures;
#[cfg(windows)]
pub(crate) mod window_manager;
#[cfg(windows)]
mod windows;

#[cfg(windows)]
use crate::platform::graphics as d3d11;
#[cfg(windows)]
use anyhow::Result;
#[cfg(windows)]
use app::{AppFactory, AppSession};
#[cfg(windows)]
pub(crate) use app::{App, WindowConfig};

#[cfg(windows)]
pub(crate) fn run(config: WindowConfig, factory: AppFactory) -> Result<()> {
    windows::run(config, factory)
}
