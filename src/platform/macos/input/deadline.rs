//! One-shot wakeups for the controller's motion throttle.
//!
//! Unlike the native Windows timer, this requires a time-enabled Tokio runtime
//! when arming and follows Tokio's timer granularity and task scheduling.
use anyhow::Result;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::{runtime::Handle, sync::Notify, task::JoinHandle, time::sleep_until};

pub(crate) struct Timer {
    wake: Arc<Notify>,
    armed: Option<Armed>,
    #[cfg(test)]
    hooks: TestHooks,
}

struct Armed {
    deadline: Instant,
    state: Arc<Mutex<WakeState>>,
    task: JoinHandle<()>,
}

enum WakeState {
    Pending,
    Fired,
    Cancelled,
}

#[cfg(test)]
type TestHook = Box<dyn FnOnce() + Send>;

#[cfg(test)]
#[derive(Default)]
struct TestHooks {
    before_wake: Option<TestHook>,
    after_wake: Option<TestHook>,
}

impl Timer {
    pub fn new(wake: Arc<Notify>) -> Result<Self> {
        // Preferences can create a timer outside the runtime; the sender arms it.
        Ok(Self {
            wake,
            armed: None,
            #[cfg(test)]
            hooks: TestHooks::default(),
        })
    }

    pub fn arm(&mut self, deadline: Instant) -> Result<()> {
        // Runtime shutdown can finish a pending task without publishing a wake.
        if self.armed.as_ref().is_some_and(|armed| {
            armed.deadline == deadline
                && matches!(
                    *armed.state.lock().unwrap_or_else(|p| p.into_inner()),
                    WakeState::Pending
                )
                && !armed.task.is_finished()
        }) {
            return Ok(());
        }
        let runtime = Handle::try_current()?;
        self.disarm();
        let wake = self.wake.clone();
        let state = Arc::new(Mutex::new(WakeState::Pending));
        let task_state = state.clone();
        #[cfg(test)]
        let TestHooks {
            before_wake,
            after_wake,
        } = std::mem::take(&mut self.hooks);
        let task = runtime.spawn(async move {
            sleep_until(deadline.into()).await;
            #[cfg(test)]
            if let Some(hook) = before_wake {
                hook();
            }
            {
                let mut state = task_state.lock().unwrap_or_else(|p| p.into_inner());
                if matches!(*state, WakeState::Pending) {
                    // Rearm is allowed as soon as the wake is published, even
                    // if this task has not finished. Serialize notify with cancel.
                    *state = WakeState::Fired;
                    wake.notify_one();
                }
            }
            #[cfg(test)]
            if let Some(hook) = after_wake {
                hook();
            }
        });
        self.armed = Some(Armed {
            deadline,
            state,
            task,
        });
        Ok(())
    }

    pub fn disarm(&mut self) {
        if let Some(armed) = self.armed.take() {
            // Windows drains callbacks only on Drop. Here both disarm and Drop
            // exclude later notifications without joining the task. A Notify
            // permit already published before cancellation is not revoked.
            *armed.state.lock().unwrap_or_else(|p| p.into_inner()) = WakeState::Cancelled;
            armed.task.abort();
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
    use super::{TestHook, Timer};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};
    use tokio::runtime::Builder;
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

    const WAIT: Duration = Duration::from_secs(2);

    #[test]
    fn same_deadline_rearms_after_runtime_shutdown() {
        let new_runtime = || Builder::new_current_thread().enable_time().build().unwrap();
        let wake = Arc::new(Notify::new());
        let mut timer = Timer::new(wake.clone()).unwrap();
        let deadline = Instant::now();
        let runtime_a = new_runtime();
        {
            let _entered = runtime_a.enter();
            timer.arm(deadline).unwrap();
        }
        // Without driving this current-thread runtime, shutdown cancels the
        // queued task before it can publish a wake.
        drop(runtime_a);

        new_runtime().block_on(async {
            timer.arm(deadline).unwrap();
            timeout(WAIT, wake.notified())
                .await
                .expect("the same deadline must fire on the new runtime");
        });
    }

    fn pause_task() -> (TestHook, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, paused) = mpsc::channel();
        let (resume, released) = mpsc::channel();
        let hook = Box::new(move || {
            entered.send(()).unwrap();
            // Dropping the sender also resumes the task if the test panics.
            assert!(
                !matches!(
                    released.recv_timeout(WAIT * 2),
                    Err(mpsc::RecvTimeoutError::Timeout),
                ),
                "test did not release the paused task"
            );
        });
        (hook, paused, resume)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn same_deadline_rearms_after_wakeup_even_while_task_is_running() {
        let wake = Arc::new(Notify::new());
        let mut timer = Timer::new(wake.clone()).unwrap();
        let (hook, paused, _resume) = pause_task();
        timer.hooks.after_wake = Some(hook);
        let deadline = Instant::now();
        timer.arm(deadline).unwrap();
        paused.recv_timeout(WAIT).unwrap();
        // Consume the first wake while its task is still paused after notify_one.
        timeout(WAIT, wake.notified()).await.unwrap();
        timer.arm(deadline).unwrap();
        timeout(WAIT, wake.notified())
            .await
            .expect("the same deadline must fire again before the old task exits");
    }

    async fn cancel_running_task(cancel: impl FnOnce(Timer) -> Option<Timer>) {
        let wake = Arc::new(Notify::new());
        let mut timer = Timer::new(wake.clone()).unwrap();
        let (hook, paused, resume) = pause_task();
        let (finished, attempted) = mpsc::channel();
        timer.hooks.before_wake = Some(hook);
        timer.hooks.after_wake = Some(Box::new(move || finished.send(()).unwrap()));
        timer.arm(Instant::now()).unwrap();
        paused.recv_timeout(WAIT).unwrap();
        // Cancellation must return without joining this still-running task.
        let _retained = cancel(timer);
        resume.send(()).unwrap();
        // The task has now passed its wakeup point. No permit may be present.
        attempted.recv_timeout(WAIT).unwrap();
        assert!(
            timeout(Duration::ZERO, wake.notified()).await.is_err(),
            "a cancelled task notified after cancellation returned"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disarm_prevents_notification_from_running_task() {
        cancel_running_task(|mut timer| {
            timer.disarm();
            Some(timer)
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drop_prevents_notification_from_running_task() {
        cancel_running_task(|timer| {
            drop(timer);
            None
        })
        .await;
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
