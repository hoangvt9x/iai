//! One shape for work the window hands to a worker thread (QC-9): the
//! thread is named, a panic in it comes back as a failure instead of the
//! result never arriving, and the window asks `poll` once a frame.

use std::sync::mpsc::{Receiver, TryRecvError};

/// What `Job::poll` found.
pub(in crate::app) enum Polled<T> {
    /// Still running.
    Pending,
    Done(T),
    /// The worker panicked or went away without an answer.
    Failed(String),
}

/// A worker thread's one result.
pub(in crate::app) struct Job<T> {
    rx: Receiver<std::thread::Result<T>>,
    label: &'static str,
}

impl<T: Send + 'static> Job<T> {
    /// Run `work` on a thread named `iai-<label>`.
    pub(in crate::app) fn spawn(
        label: &'static str,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let run = move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
            let _ = tx.send(result);
        };
        if let Err(e) = std::thread::Builder::new()
            .name(format!("iai-{label}"))
            .spawn(run)
        {
            // No thread to be had: the job fails at once (its sender is gone).
            crate::diag::note("job", &format!("{label}: no thread ({e})"));
        }
        Self { rx, label }
    }

    pub(in crate::app) fn poll(&self) -> Polled<T> {
        match self.rx.try_recv() {
            Ok(Ok(value)) => Polled::Done(value),
            Ok(Err(panic)) => {
                let why = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic".to_string());
                crate::diag::note("job", &format!("{} failed: {why}", self.label));
                Polled::Failed(why)
            }
            Err(TryRecvError::Empty) => Polled::Pending,
            Err(TryRecvError::Disconnected) => Polled::Failed("stopped".to_string()),
        }
    }
}

/// Something read off the disk that a window shows (a folder's listing,
/// say), read on a worker and read again once it is `fresh` no longer: the
/// window's frame never waits on the disk, and what was read last stays
/// shown while it is read again.
#[cfg(feature = "pro")]
pub(in crate::app) struct ReadAgain<T> {
    value: Option<T>,
    read_at: Option<std::time::Instant>,
    job: Option<Job<T>>,
}

#[cfg(feature = "pro")]
impl<T> Default for ReadAgain<T> {
    fn default() -> Self {
        Self {
            value: None,
            read_at: None,
            job: None,
        }
    }
}

#[cfg(feature = "pro")]
impl<T: Send + 'static> ReadAgain<T> {
    /// Once a frame while it is shown: take a read that ended, start one
    /// when due. True while a read is under way (the window keeps drawing
    /// to take it).
    pub(in crate::app) fn tick(
        &mut self,
        label: &'static str,
        fresh: std::time::Duration,
        read: fn() -> T,
    ) -> bool {
        if let Some(job) = &self.job {
            match job.poll() {
                Polled::Pending => return true,
                Polled::Done(value) => {
                    self.value = Some(value);
                    self.read_at = Some(std::time::Instant::now());
                }
                // Tried again when due, as an empty folder would be.
                Polled::Failed(_) => self.read_at = Some(std::time::Instant::now()),
            }
            self.job = None;
        }
        if self.read_at.is_none_or(|at| at.elapsed() >= fresh) {
            self.job = Some(Job::spawn(label, read));
            return true;
        }
        false
    }

    /// What was read last.
    pub(in crate::app) fn get(&self) -> Option<&T> {
        self.value.as_ref()
    }

    /// Read it again at the next frame (what is shown stays until then); a
    /// read under way, begun before the change, is let go.
    pub(in crate::app) fn stale(&mut self) {
        self.read_at = None;
        self.job = None;
    }

    /// Forget it: the window that showed it is closed.
    pub(in crate::app) fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait<T: Send + 'static>(job: &Job<T>) -> Polled<T> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match job.poll() {
                Polled::Pending if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                other => return other,
            }
        }
    }

    #[cfg(feature = "pro")]
    #[test]
    fn a_read_again_keeps_what_it_read_until_the_next_read_ends() {
        let mut list: ReadAgain<u32> = ReadAgain::default();
        let fresh = std::time::Duration::from_secs(60);
        assert!(list.tick("test-read", fresh, || 7));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while list.tick("test-read", fresh, || 7) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(list.get(), Some(&7));
        // Due again: what was read stays shown meanwhile.
        list.stale();
        assert!(list.tick("test-read", fresh, || 8));
        assert_eq!(list.get(), Some(&7));
        list.clear();
        assert_eq!(list.get(), None);
    }

    #[test]
    fn a_job_hands_back_its_value_and_a_panic_comes_back_as_a_failure() {
        let job = Job::spawn("test-ok", || 41 + 1);
        assert!(matches!(wait(&job), Polled::Done(42)));
        let job = Job::spawn("test-panic", || -> u8 { panic!("printer gone") });
        assert!(matches!(wait(&job), Polled::Failed(why) if why == "printer gone"));
    }
}
