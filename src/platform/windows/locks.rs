//! Named kernel objects for cross-process exclusion.
use super::host_service::pipe::Handle;
use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0,
            WAIT_TIMEOUT,
        },
        System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
    },
    core::PCWSTR,
};

// Profile-specific so concurrent development profiles do not block each other.
static SLOT_NAME: crate::product::WideName =
    crate::product::WideName::new(|| crate::product::local_object("SoftwareVideoPlayback.v1"));

pub(crate) struct SoftwarePlaybackLock {
    handle: HANDLE,
}

impl SoftwarePlaybackLock {
    /// Try to take the session-local slot without waiting. An abandoned mutex
    /// can be taken by the next owner if the previous owner process dies.
    pub(crate) fn try_acquire() -> Result<Option<Self>> {
        let handle = unsafe { CreateMutexW(None, false, SLOT_NAME.pcwstr()) }
            .context(crate::i18n::t().session.software_slot_create_failed)?;
        let status = unsafe { WaitForSingleObject(handle, 0) };
        if status == WAIT_OBJECT_0 || status == WAIT_ABANDONED {
            return Ok(Some(Self { handle }));
        }
        let _ = unsafe { CloseHandle(handle) };
        if status == WAIT_TIMEOUT {
            return Ok(None);
        }
        bail!(
            "{}",
            (crate::i18n::t().session.software_slot_failed)(&format!("{status:?}"))
        )
    }
}

impl Drop for SoftwarePlaybackLock {
    fn drop(&mut self) {
        let _ = unsafe { ReleaseMutex(self.handle) };
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

// This mutex is never acquired: its HANDLE only reserves the object name.
pub(crate) struct Reservation {
    _handle: Handle,
}
// SAFETY: the handle is only closed on drop; no thread owns the mutex.
unsafe impl Send for Reservation {}

impl Reservation {
    /// Try to reserve the machine-wide name for this client's presence room.
    pub(crate) fn try_acquire(key: &str) -> Result<Option<Self>> {
        let name: Vec<u16> = crate::product::global_object(&format!("Presence.{}", key))
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let handle = Reservation {
            _handle: Handle(unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr()))? }),
        };
        if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
            return Ok(Some(handle));
        }
        drop(handle);
        Ok(None)
    }
}
