//! Features ownership and module boundaries.

pub(crate) mod chat;
pub(crate) mod voice;
#[cfg(windows)]
pub(crate) mod clipboard;
#[cfg(windows)]
pub(crate) mod drag_drop;
// Host status IPC still carries transfer notices on every platform.
pub(crate) mod file_transfer;
pub(crate) mod host;
pub(crate) mod network_control;
pub(crate) mod participants;
pub(crate) mod port_mapping;
pub(crate) mod remote_cursor;
pub(crate) mod remote_input;
pub(crate) mod remote_upgrade;
pub mod stream_control;
pub(crate) mod viewing_settings;
pub(crate) mod wol;
