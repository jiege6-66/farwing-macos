use anyhow::{Result, bail};
use std::{io, os::windows::ffi::OsStrExt, path::Path};
use windows::{
    Win32::{
        Storage::FileSystem::{
            FILE_ATTRIBUTE_HIDDEN, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
            SetFileAttributesW,
        },
        UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
    },
    core::{PCWSTR, w},
};

pub(crate) fn hide_state_directory(state: &Path) -> io::Result<()> {
    let path: Vec<u16> = state.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe { SetFileAttributesW(PCWSTR(path.as_ptr()), FILE_ATTRIBUTE_HIDDEN) }
        .map_err(io::Error::other)
}

pub(crate) fn replace_config(temporary: &Path, path: &Path) -> Result<()> {
    let from: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }?;
    Ok(())
}

pub(crate) fn open_directory(directory: &Path) -> Result<()> {
    // Runtime.directory has already been resolved to the physical directory.
    let path: Vec<u16> = directory.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(path.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        bail!(
            "{}",
            (crate::i18n::t().session.log_folder_open_failed)(result.0 as isize)
        );
    }
    Ok(())
}
