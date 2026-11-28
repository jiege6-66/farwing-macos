//! Native OS/driver implementations. Product policy belongs to sessions/features.
//! Select the real backend here; no placeholder implementations for future OSes.
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::{console, device_profile, display, input, locks};
#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use windows::{
    camera, capture, console, decoder, device_profile, display, display_hdr, encoder, graphics,
    input, locks, source, surface, swapchain, transfer, video_shader,
};

/// This installation's platform for registration and direct connections.
#[cfg(target_os = "macos")]
pub(crate) const PLATFORM: farwing_protocol::Platform = farwing_protocol::Platform::Macos;
#[cfg(windows)]
pub(crate) const PLATFORM: farwing_protocol::Platform = farwing_protocol::Platform::Windows;
