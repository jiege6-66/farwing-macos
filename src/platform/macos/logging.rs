use anyhow::{Context, Result, ensure};
use std::{path::Path, process::Command};

/// Open a directory already resolved to its physical path.
pub(crate) fn open_directory(directory: &Path) -> Result<()> {
    let status = Command::new("/usr/bin/open")
        .arg("--")
        .arg(directory)
        .status()
        .context("/usr/bin/open")?;
    ensure!(
        status.success(),
        "{}",
        (crate::i18n::t().session.log_folder_open_failed)(status.code().unwrap_or(-1) as isize)
    );
    Ok(())
}
