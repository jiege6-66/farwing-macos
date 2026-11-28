//! One-shot wakeups for the controller's motion throttle.
use anyhow::Result;
use std::sync::Arc;
use std::time::Instant;
use tokio::{runtime::Handle, sync::Notify, task::JoinHandle, time::sleep_until};

pub(crate) struct Timer {
    wake: Arc<Notify>,
    armed: Option<(Instant, JoinHandle<()>)>,
}

impl Timer {
    pub fn new(wake: Arc<Notify>) -> Result<Self> {
        // Preferences can create a timer outside the runtime; the sender arms it.
        Ok(Self { wake, armed: None })
    }

    pub fn arm(&mut self, deadline: Instant) -> Result<()> {
        if self
            .armed
            .as_ref()
            .is_some_and(|(armed, task)| *armed == deadline && !task.is_finished())
        {
            return Ok(());
        }
        let runtime = Handle::try_current()?;
        self.disarm();
        let wake = self.wake.clone();
        let task = runtime.spawn(async move {
            sleep_until(deadline.into()).await;
            // A wake racing disarm is harmless: RemoteInput rechecks its deadline.
            wake.notify_one();
        });
        self.armed = Some((deadline, task));
        Ok(())
    }

    pub fn disarm(&mut self) {
        if let Some((_, task)) = self.armed.take() {
            task.abort();
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.disarm();
    }
}

#[cfg(test)]
mod tests {
    use super::Timer;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::Notify;
    use tokio::time::timeout;

    #[tokio::test]
    async fn wakes_once_at_deadline_and_can_be_rearmed() {
        let wake = Arc::new(Notify::new());
        let mut timer = Timer::new(wake.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_millis(20);
        timer.arm(deadline).unwrap();
        timer.arm(deadline).unwrap();
        timeout(Duration::from_secs(1), wake.notified())
            .await
            .unwrap();
        assert!(Instant::now() >= deadline);
        assert!(
            timeout(Duration::from_millis(20), wake.notified())
                .await
                .is_err()
        );
        timer.arm(deadline).unwrap();
        timeout(Duration::from_secs(1), wake.notified())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn resetting_replaces_the_pending_deadline() {
        let wake = Arc::new(Notify::new());
        let mut timer = Timer::new(wake.clone()).unwrap();
        // A due timer has not run yet on this single-threaded runtime.
        timer.arm(Instant::now()).unwrap();
        let deadline = Instant::now() + Duration::from_millis(50);
        timer.arm(deadline).unwrap();
        timeout(Duration::from_secs(1), wake.notified())
            .await
            .unwrap();
        assert!(Instant::now() >= deadline);
    }

    #[tokio::test]
    async fn disarm_and_drop_cancel_pending_wakeups() {
        let wake = Arc::new(Notify::new());
        let mut timer = Timer::new(wake.clone()).unwrap();
        timer.arm(Instant::now()).unwrap();
        timer.disarm();
        assert!(
            timeout(Duration::from_millis(20), wake.notified())
                .await
                .is_err()
        );
        timer.arm(Instant::now()).unwrap();
        drop(timer);
        assert!(
            timeout(Duration::from_millis(20), wake.notified())
                .await
                .is_err()
        );
    }
}
