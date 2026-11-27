//! Voice channels and calls (`docs/protocol/chat-design.md` §7, §10).

#[cfg(windows)]
pub(crate) mod capture;
#[cfg(windows)]
pub(crate) mod engine;
#[cfg(windows)]
pub(crate) mod keys;
#[cfg(windows)]
pub(crate) mod session;
// Keep the on-disk settings schema on every platform.
pub(crate) mod settings;
#[cfg(windows)]
pub(crate) mod sounds;
#[cfg(windows)]
pub(crate) mod video;
