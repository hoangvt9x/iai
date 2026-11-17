//! Long commands: the work runs on a worker thread, the change it makes is
//! applied on the session's thread when the session is next polled, as one
//! undo step. One frame of the GUI polls; a script waits.

use std::cell::Cell;
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Instant;

use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::command::{Apply, Job};
use crate::{CmdError, ErrorCode};

/// A job, as long as the session lives.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct JobId(pub u64);

struct Shared {
    cancel: AtomicBool,
    progress: Mutex<(f32, String)>,
}

/// What a job's work sees: where to report progress, and whether it was
/// cancelled.
#[derive(Clone)]
pub struct JobCtx {
    shared: Arc<Shared>,
}

impl JobCtx {
    pub(crate) fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                cancel: AtomicBool::new(false),
                progress: Mutex::new((0.0, String::new())),
            }),
        }
    }

    /// `fraction` from 0 to 1, and a short Vietnamese line for the status bar.
    pub fn progress(&self, fraction: f32, vi: impl Into<String>) {
        let fraction = if fraction.is_finite() {
            fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        *self.shared.progress.lock() = (fraction, vi.into());
    }

    pub fn cancelled(&self) -> bool {
        self.shared.cancel.load(Ordering::Relaxed)
    }

    /// `Err(cancelled)` once the job was cancelled: `?` it between steps.
    pub fn check_cancelled(&self) -> Result<(), CmdError> {
        if self.cancelled() {
            Err(CmdError::new(ErrorCode::Cancelled, "the job was cancelled").vi("Đã hủy"))
        } else {
            Ok(())
        }
    }

    fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }

    fn read_progress(&self) -> (f32, String) {
        self.shared.progress.lock().clone()
    }
}

/// Where a job is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JobState {
    Running {
        command: String,
        /// 0 to 1.
        fraction: f32,
        /// What it is doing, in Vietnamese.
        vi: String,
        seconds: f32,
    },
    Done {
        result: Value,
        revision: u64,
    },
    Failed {
        error: CmdError,
    },
    Cancelled,
}

thread_local! {
    static CATCHING: Cell<u32> = const { Cell::new(0) };
}

/// True while the engine is catching panics on this thread: a panic here is
/// turned into an error and the document is put back. The app's panic hook
/// reads it to log such a panic instead of reporting a crash.
pub fn catching_panics() -> bool {
    CATCHING.with(|c| c.get() > 0)
}

/// Run `f`, a panic in it caught and returned as `Err`.
pub(crate) fn guarded<T>(f: impl FnOnce() -> T) -> std::thread::Result<T> {
    CATCHING.with(|c| c.set(c.get().saturating_add(1)));
    let out = catch_unwind(AssertUnwindSafe(f));
    CATCHING.with(|c| c.set(c.get().saturating_sub(1)));
    out
}

/// Text of a caught panic.
pub(crate) fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".to_string())
}

type WorkResult = std::thread::Result<Result<Apply<Value>, CmdError>>;

pub(crate) struct Running {
    pub id: JobId,
    pub command: &'static str,
    pub doc: Option<u32>,
    ctx: JobCtx,
    rx: Receiver<WorkResult>,
    started: Instant,
    /// Applied in its turn (see [`Jobs::apply_in_turn`]).
    in_turn: bool,
    /// Its work's result, kept while it waits for its turn.
    done: Option<WorkResult>,
}

/// A job whose work has ended, to apply.
pub(crate) struct Ended {
    pub id: JobId,
    pub command: &'static str,
    pub cancelled: bool,
    pub outcome: Result<Apply<Value>, CmdError>,
}

/// How many ended jobs are remembered for `job_status`.
const KEEP_ENDED: usize = 64;

/// Run a command's `job` here, now, as an engine does a call not sent to
/// the background: for a command that runs another (a pack's
/// `vector.build_region`). Its result is applied by calling what it gives
/// back on the session.
pub fn run_job_here(command: &'static str, job: Job<Value>) -> Result<Apply<Value>, CmdError> {
    Jobs::run_here(command, job)
}

#[derive(Default)]
pub(crate) struct Jobs {
    next: u64,
    running: Vec<Running>,
    ended: VecDeque<(JobId, JobState)>,
}

impl Jobs {
    pub fn busy(&self) -> bool {
        !self.running.is_empty()
    }

    pub fn working_on(&self, doc: u32) -> Option<&'static str> {
        self.running
            .iter()
            .find(|r| r.doc == Some(doc))
            .map(|r| r.command)
    }

    pub fn running_ids(&self) -> Vec<JobId> {
        self.running.iter().map(|r| r.id).collect()
    }

    fn next_id(&mut self) -> JobId {
        self.next = self.next.saturating_add(1);
        JobId(self.next)
    }

    /// Start `job`'s work on a thread of its own.
    pub fn spawn(&mut self, command: &'static str, job: Job<Value>) -> Result<JobId, CmdError> {
        let id = self.next_id();
        let ctx = JobCtx::new();
        let (tx, rx) = mpsc::channel();
        let worker_ctx = ctx.clone();
        let work = job.work;
        std::thread::Builder::new()
            .name(format!("job {}: {command}", id.0))
            .spawn(move || {
                let out = guarded(|| work(&worker_ctx));
                let _ = tx.send(out);
            })
            .map_err(|e| CmdError::internal(format!("could not start a thread: {e}")))?;
        self.running.push(Running {
            id,
            command,
            doc: job.doc,
            ctx,
            rx,
            started: Instant::now(),
            in_turn: false,
            done: None,
        });
        Ok(id)
    }

    /// Hold `id`'s result, once its work ends, until every job started
    /// before it that is also held in turn has ended (and been handed out).
    pub fn apply_in_turn(&mut self, id: JobId) -> bool {
        match self.running.iter_mut().find(|r| r.id == id) {
            Some(r) => {
                r.in_turn = true;
                true
            }
            None => false,
        }
    }

    /// Run `job`'s work here, now.
    pub fn run_here(command: &'static str, job: Job<Value>) -> Result<Apply<Value>, CmdError> {
        let ctx = JobCtx::new();
        match guarded(|| (job.work)(&ctx)) {
            Ok(out) => out,
            Err(payload) => Err(panicked(command, &panic_text(payload.as_ref()))),
        }
    }

    pub fn cancel(&mut self, id: JobId) -> bool {
        match self.running.iter().find(|r| r.id == id) {
            Some(r) => {
                r.ctx.cancel();
                true
            }
            None => false,
        }
    }

    /// The jobs whose work has ended, taken out of the running list. A job
    /// held in turn whose work ended stays until the in-turn jobs started
    /// before it have all ended.
    pub fn take_ended(&mut self) -> Vec<Ended> {
        let mut ended = Vec::new();
        // An in-turn job started earlier is still at work: later in-turn
        // jobs wait for it (`running` is in the order started).
        let mut earlier_turn_open = false;
        let mut i = 0;
        while let Some(r) = self.running.get_mut(i) {
            if r.done.is_none() {
                r.done = match r.rx.try_recv() {
                    Ok(out) => Some(out),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => Some(Ok(Err(CmdError::internal(format!(
                        "the worker of `{}` ended without an answer",
                        r.command
                    ))))),
                };
            }
            let waits = r.in_turn && earlier_turn_open;
            if r.in_turn && r.done.is_none() {
                earlier_turn_open = true;
            }
            if waits || r.done.is_none() {
                i = i.saturating_add(1);
                continue;
            }
            let r = self.running.remove(i);
            let outcome = match r.done {
                Some(Ok(outcome)) => outcome,
                Some(Err(payload)) => Err(panicked(r.command, &panic_text(payload.as_ref()))),
                None => Err(CmdError::internal(format!(
                    "the worker of `{}` ended without an answer",
                    r.command
                ))),
            };
            ended.push(Ended {
                id: r.id,
                command: r.command,
                cancelled: r.ctx.cancelled(),
                outcome,
            });
        }
        ended
    }

    pub fn remember(&mut self, id: JobId, state: JobState) {
        if self.ended.len() >= KEEP_ENDED {
            self.ended.pop_front();
        }
        self.ended.push_back((id, state));
    }

    pub fn state(&self, id: JobId) -> Option<JobState> {
        if let Some(r) = self.running.iter().find(|r| r.id == id) {
            let (fraction, vi) = r.ctx.read_progress();
            return Some(JobState::Running {
                command: r.command.to_string(),
                fraction,
                vi,
                seconds: r.started.elapsed().as_secs_f32(),
            });
        }
        self.ended
            .iter()
            .rev()
            .find(|(i, _)| *i == id)
            .map(|(_, s)| s.clone())
    }

    /// The progress line of the job running on `doc`, if any.
    pub fn progress_of(&self, id: JobId) -> Option<(f32, String)> {
        self.running
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.ctx.read_progress())
    }
}

pub(crate) fn panicked(command: &str, text: &str) -> CmdError {
    CmdError::internal(format!(
        "`{command}` panicked ({text}); nothing was changed"
    ))
    .vi("Lỗi trong chương trình; ảnh được giữ nguyên")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Sender;

    /// A job whose work ends when `go` is sent something, answering `n`.
    fn waiting_job(n: u64) -> (Job<Value>, Sender<()>) {
        let (go, wait) = mpsc::channel::<()>();
        let job = Job {
            doc: None,
            label_vi: String::new(),
            work: Box::new(move |_cx: &JobCtx| {
                let _ = wait.recv();
                Ok(
                    Box::new(move |_s: &mut crate::session::Session<'_>| Ok(Value::from(n)))
                        as Apply<Value>,
                )
            }),
        };
        (job, go)
    }

    /// Poll until `count` jobs have been handed out (or a few seconds pass).
    fn take(jobs: &mut Jobs, count: usize) -> Vec<JobId> {
        let start = Instant::now();
        let mut out = Vec::new();
        while out.len() < count && start.elapsed() < std::time::Duration::from_secs(10) {
            out.extend(jobs.take_ended().into_iter().map(|e| e.id));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        out
    }

    #[test]
    fn jobs_held_in_turn_end_in_the_order_started() {
        let mut jobs = Jobs::default();
        let (first, go_first) = waiting_job(1);
        let (second, go_second) = waiting_job(2);
        let (free, go_free) = waiting_job(3);
        let a = jobs.spawn("file.open", first).expect("a thread");
        let b = jobs.spawn("file.open", second).expect("a thread");
        let c = jobs.spawn("other", free).expect("a thread");
        assert!(jobs.apply_in_turn(a) && jobs.apply_in_turn(b));
        // The second and the free job end first: only the free one is handed
        // out; the second waits for the first.
        go_second.send(()).expect("worker");
        go_free.send(()).expect("worker");
        assert_eq!(take(&mut jobs, 1), vec![c]);
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(jobs.take_ended().is_empty());
        go_first.send(()).expect("worker");
        assert_eq!(take(&mut jobs, 2), vec![a, b]);
        assert!(!jobs.busy());
    }
}
