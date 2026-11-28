use anyhow::{Context, Result, ensure};
use objc2::rc::autoreleasepool;
use objc2_foundation::{NSFileManager, NSSearchPathDirectory, NSSearchPathDomainMask};
use std::path::PathBuf;

/// The user's `~/Library/Application Support`, validated as an absolute directory.
pub(crate) fn application_support() -> Result<PathBuf> {
    user_directory(
        NSSearchPathDirectory::ApplicationSupportDirectory,
        crate::i18n::t().system.application_support_unknown,
    )
}

/// The user's `~/Library/Logs`, validated as an absolute directory.
pub(crate) fn logs() -> Result<PathBuf> {
    Ok(user_directory(
        NSSearchPathDirectory::LibraryDirectory,
        crate::i18n::t().session.log_folder_unresolved,
    )?
    .join("Logs"))
}

/// The first matching directory in the user domain, validated as an absolute path.
fn user_directory(directory: NSSearchPathDirectory, unavailable: &'static str) -> Result<PathBuf> {
    let path = autoreleasepool(|_| {
        NSFileManager::defaultManager()
            .URLsForDirectory_inDomains(directory, NSSearchPathDomainMask::UserDomainMask)
            .firstObject()
            .and_then(|url| url.to_file_path())
    })
    .context(unavailable)?;
    ensure!(
        path.is_absolute(),
        "{}",
        (crate::i18n::t().system.system_folder_invalid)(&path.display().to_string())
    );
    Ok(path)
}
