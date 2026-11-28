//! One software playback decoder across Farwing processes of one profile:
//! session-local on Windows, per-user on macOS.
//! The owner stays on its decoder worker thread.
use crate::platform::locks::SoftwarePlaybackLock;
use anyhow::Result;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

static OCCUPIED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub(crate) struct SoftwarePlaybackBusy;
impl std::fmt::Display for SoftwarePlaybackBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(crate::i18n::t().session.software_playback_busy)
    }
}
impl std::error::Error for SoftwarePlaybackBusy {}

pub(crate) struct SoftwareSlot {
    lock: Option<SoftwarePlaybackLock>,
    _thread_bound: PhantomData<Rc<()>>,
}
impl SoftwareSlot {
    pub(crate) fn acquire() -> Result<Rc<Self>> {
        // Also disallow Win32 mutex recursion on the same thread. Existing
        // playback explicitly shares an Rc across decoder reconfiguration.
        if OCCUPIED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SoftwarePlaybackBusy.into());
        }
        let result = SoftwarePlaybackLock::try_acquire().and_then(|lock| {
            let lock = lock.ok_or(SoftwarePlaybackBusy)?;
            Ok(Rc::new(Self {
                lock: Some(lock),
                _thread_bound: PhantomData,
            }))
        });
        if result.is_err() {
            OCCUPIED.store(false, Ordering::Release);
        }
        result
    }
}
impl Drop for SoftwareSlot {
    fn drop(&mut self) {
        // Rc/PhantomData keep final release on the acquiring worker. Release
        // the cross-process lock before another worker can take the local slot.
        drop(self.lock.take());
        OCCUPIED.store(false, Ordering::Release);
    }
}
