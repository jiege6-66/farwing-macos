//! File-backed locks for cross-process exclusion within a user's profile.
use anyhow::{Context, Result, anyhow};
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::Path,
};
use uuid::Uuid;

pub(crate) struct SoftwarePlaybackLock {
    _file: File,
}

impl SoftwarePlaybackLock {
    /// Try to take the profile's software playback slot without waiting.
    pub(crate) fn try_acquire() -> Result<Option<Self>> {
        let lock = open_profile_lock("software-video-playback.v1.lock")
            .context(crate::i18n::t().session.software_slot_create_failed)?;
        let lock = try_lock(lock).map_err(|error| {
            anyhow!(
                "{}",
                (crate::i18n::t().session.software_slot_failed)(&error.to_string())
            )
        })?;
        Ok(lock.map(|file| Self { _file: file }))
    }
}

pub(crate) struct Reservation {
    _file: File,
}

impl Reservation {
    /// Windows uses a machine-wide `Global\` name for the resident handoff. On
    /// macOS the client ID is stored in the user's profile-specific Keychain
    /// entry, so a lock in that user's profile data directory is sufficient.
    pub(crate) fn try_acquire(key: &str) -> Result<Option<Self>> {
        let filename = presence_lock_filename(key)?;
        let lock = open_profile_lock(&filename).context("open presence coordination lock")?;
        Ok(try_lock(lock)?.map(|file| Self { _file: file }))
    }
}

fn presence_lock_filename(key: &str) -> Result<String> {
    let key = Uuid::parse_str(key).context("presence key is not a UUID")?;
    Ok(format!("presence.{}.lock", key.hyphenated()))
}

fn open_profile_lock(filename: &str) -> Result<File> {
    let directory = crate::product::local_data_dir()?.join("locks");
    Ok(open_lock(&directory, filename)?)
}

fn open_lock(directory: &Path, filename: &str) -> io::Result<File> {
    fs::create_dir_all(directory)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(filename))
}

fn try_lock(file: File) -> io::Result<Option<File>> {
    // On macOS this is flock(LOCK_EX | LOCK_NB). The kernel releases it when
    // the file is closed, including on process exit. Leave the lock file in place
    // so every contender opens the same inode.
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lock_is_busy_until_owner_drops() {
        let directory =
            TempDir(std::env::temp_dir().join(format!("farwing-lock-{}", Uuid::new_v4())));
        let attempt = || try_lock(open_lock(&directory.0, "test.lock")?);
        let owner = attempt().unwrap().unwrap();
        assert!(attempt().unwrap().is_none());
        drop(owner);
        assert!(attempt().unwrap().is_some());
    }

    #[test]
    fn presence_lock_rejects_non_uuid_keys() {
        assert_eq!(
            presence_lock_filename("../x").unwrap_err().to_string(),
            "presence key is not a UUID"
        );
    }
}
