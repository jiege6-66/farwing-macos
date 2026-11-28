//! macOS system integrations.
pub(crate) mod console;
pub(crate) mod device_profile;
pub(crate) mod display;
pub(crate) mod folders;
pub(crate) mod input;
pub(crate) mod locale;
pub(crate) mod locks;
pub(crate) mod logging;

// T2a: keep the bitstream helpers local until the VT decoder is implemented.
pub(crate) mod decoder;
