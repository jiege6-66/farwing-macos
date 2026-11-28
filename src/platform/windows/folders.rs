use anyhow::{Context, Result, ensure};
use std::path::PathBuf;

/// `%LOCALAPPDATA%`, validated as an absolute directory.
pub(crate) fn local_app_data() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Ok(path);
    }
    known_folder(&windows::Win32::UI::Shell::FOLDERID_LocalAppData)
        .context(crate::i18n::t().system.local_app_data_unknown)
}

pub(crate) fn program_data() -> Result<PathBuf> {
    known_folder(&windows::Win32::UI::Shell::FOLDERID_ProgramData)
        .context(crate::i18n::t().system.program_data_unknown)
}

fn known_folder(id: &windows::core::GUID) -> Result<PathBuf> {
    use windows::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath},
    };
    let raw = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)? };
    let text = unsafe { raw.to_string() };
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    let path = PathBuf::from(text?);
    ensure!(
        path.is_absolute(),
        "{}",
        (crate::i18n::t().system.system_folder_invalid)(&path.display().to_string())
    );
    Ok(path)
}
