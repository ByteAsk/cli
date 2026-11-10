//! Background runs and their completion notice.
//!
//! A run starts as a task. The tool call waits for it up to `wait_s`; if it is
//! not done by then the call returns and the run continues. When a run that
//! went to the background finishes, its result is put in front of the model on
//! its next request (a world-state section of this extension, shown once), so
//! the model never has to poll. `action=wait` blocks until a run finishes, for
//! when there is nothing else to do; `action=status` shows what is running.

use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use codex_extension_api::PreviousWorldStateSection;
use codex_extension_api::RenderedWorldStateFragment;
use codex_extension_api::WorldStateSectionContribution;
use serde_json::Value;
use tokio::sync::watch;

const WORLD_STATE_ID: &str = "cpp_test_select_runs";
const START_MARKER: &str = "<cpp_test_select_result>";
const END_MARKER: &str = "</cpp_test_select_result>";

struct Job {
    id: u32,
    label: String,
    started: Instant,
    progress: Arc<Mutex<String>>,
    result: watch::Receiver<Option<String>>,
    cancel: watch::Sender<bool>,
    /// The call that started it returned before it finished.
    background: bool,
    /// The model has seen the result (returned by a call).
    fetched: bool,
    /// Completion order of background runs, for the notice.
    notice_serial: Option<u64>,
}

#[derive(Default)]
struct Inner {
    next_id: u32,
    serial: u64,
    jobs: Vec<Job>,
}

/// The runs of one thread.
#[derive(Default)]
pub(crate) struct Jobs {
    inner: Mutex<Inner>,
}

/// What a caller learns when it starts or waits for a run.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Waited {
    Done(String),
    Running {
        id: u32,
        label: String,
        secs: u64,
        progress: String,
    },
}

impl Jobs {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Has this thread started any run?
    pub(crate) fn has_runs(&self) -> bool {
        !self.lock().jobs.is_empty()
    }

    /// The run in progress, if any: `(id, label, seconds, progress)`.
    pub(crate) fn running(&self) -> Option<(u32, String, u64, String)> {
        let inner = self.lock();
        inner
            .jobs
            .iter()
            .find(|j| j.result.borrow().is_none())
            .map(|j| {
                (
                    j.id,
                    j.label.clone(),
                    j.started.elapsed().as_secs(),
                    progress_of(j),
                )
            })
    }

    /// Start `make(cancel, progress)` as a run labelled `label`.
    pub(crate) fn start<F, Fut>(self: &Arc<Self>, label: String, make: F) -> u32
    where
        F: FnOnce(watch::Receiver<bool>, Arc<Mutex<String>>) -> Fut,
        Fut: Future<Output = String> + Send + 'static,
    {
        let (result_tx, result_rx) = watch::channel(None);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let progress = Arc::new(Mutex::new("starting".to_string()));
        let fut = make(cancel_rx, progress.clone());
        let id = {
            let mut inner = self.lock();
            inner.next_id += 1;
            let id = inner.next_id;
            inner.jobs.push(Job {
                id,
                label,
                started: Instant::now(),
                progress,
                result: result_rx,
                cancel: cancel_tx,
                background: false,
                fetched: false,
                notice_serial: None,
            });
            id
        };
        let jobs = Arc::clone(self);
        // The run keeps the sandbox of the call that started it.
        let fut = crate::sandbox::inherit(fut);
        tokio::spawn(async move {
            let text = fut.await;
            let mut inner = jobs.lock();
            inner.serial += 1;
            let serial = inner.serial;
            if let Some(job) = inner.jobs.iter_mut().find(|j| j.id == id) {
                if job.background {
                    job.notice_serial = Some(serial);
                }
            }
            // Published under the lock, so `wait` sees either a finished run
            // or one whose completion will find `background` already set.
            let _ = result_tx.send(Some(text));
            drop(inner);
        });
        id
    }

    /// Wait up to `wait` for run `id` (the newest when `None`). A result
    /// returned here counts as seen; a run still going is left in the
    /// background and announced when it ends.
    pub(crate) async fn wait(&self, id: Option<u32>, wait: Duration) -> Option<Waited> {
        let (id, mut rx) = {
            let inner = self.lock();
            let job = match id {
                Some(id) => inner.jobs.iter().find(|j| j.id == id)?,
                None => inner.jobs.last()?,
            };
            (job.id, job.result.clone())
        };
        let done = tokio::time::timeout(wait, rx.wait_for(Option::is_some))
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(|v| v.clone());
        let mut inner = self.lock();
        let job = inner.jobs.iter_mut().find(|j| j.id == id)?;
        let done = done.or_else(|| job.result.borrow().clone());
        Some(match done {
            Some(text) => {
                job.fetched = true;
                Waited::Done(text)
            }
            None => {
                job.background = true;
                Waited::Running {
                    id,
                    label: job.label.clone(),
                    secs: job.started.elapsed().as_secs(),
                    progress: progress_of(job),
                }
            }
        })
    }

    /// Ask run `id` (the running one when `None`) to stop.
    pub(crate) fn cancel(&self, id: Option<u32>) -> Option<u32> {
        let inner = self.lock();
        let job = match id {
            Some(id) => inner.jobs.iter().find(|j| j.id == id)?,
            None => inner.jobs.iter().find(|j| j.result.borrow().is_none())?,
        };
        let _ = job.cancel.send(true);
        Some(job.id)
    }

    /// One line per run, newest first (at most 5).
    pub(crate) fn summary(&self) -> Vec<String> {
        let inner = self.lock();
        inner
            .jobs
            .iter()
            .rev()
            .take(5)
            .map(|j| {
                let state = if j.result.borrow().is_some() {
                    "finished".to_string()
                } else {
                    format!(
                        "running for {}s ({})",
                        j.started.elapsed().as_secs(),
                        progress_of(j)
                    )
                };
                format!("run {}: {} — {state}", j.id, j.label)
            })
            .collect()
    }

    /// Finished background runs the model has not seen: `(serial, text)`.
    fn unseen(&self) -> (u64, Vec<(u64, u32, String)>) {
        let inner = self.lock();
        let items = inner
            .jobs
            .iter()
            .filter(|j| j.background && !j.fetched)
            .filter_map(|j| {
                let serial = j.notice_serial?;
                let text = j.result.borrow().clone()?;
                Some((serial, j.id, text))
            })
            .collect();
        (inner.serial, items)
    }

    /// The world-state section announcing finished background runs: shown once
    /// per run, on the first request after it finished.
    pub(crate) fn world_state_section(&self) -> WorldStateSectionContribution {
        let (serial, items) = self.unseen();
        WorldStateSectionContribution::new(WORLD_STATE_ID, move |previous| {
            let seen = match previous {
                PreviousWorldStateSection::Known(value) => value.as_u64().unwrap_or(0),
                PreviousWorldStateSection::Absent => 0,
                // Resumed without saved state: do not repeat old notices.
                PreviousWorldStateSection::Unknown => serial,
            };
            let fresh: Vec<String> = items
                .iter()
                .filter(|(s, _, _)| *s > seen)
                .map(|(_, id, text)| {
                    format!("Background run {id} of cpp_test_select finished:\n{text}")
                })
                .collect();
            let fragment = (!fresh.is_empty()).then(|| {
                RenderedWorldStateFragment::new(
                    "developer",
                    (START_MARKER, END_MARKER),
                    codex_protocol::models::input_text(fresh.join("\n\n")),
                )
            });
            (Some(Value::from(serial)), fragment)
        })
    }
}

fn progress_of(job: &Job) -> String {
    job.progress
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn a_quick_run_returns_directly_and_is_never_announced() {
        let jobs = Jobs::new();
        jobs.start("quick".to_string(), |_, _| async {
            "all passed".to_string()
        });
        assert_eq!(
            jobs.wait(None, Duration::from_secs(5)).await,
            Some(Waited::Done("all passed".to_string()))
        );
        let section = jobs.world_state_section();
        assert_eq!(section.render_diff(PreviousWorldStateSection::Absent), None);
    }

    #[tokio::test]
    async fn a_long_run_goes_to_the_background_and_is_announced_once() {
        let jobs = Jobs::new();
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        jobs.start("long".to_string(), move |_, progress| async move {
            *progress.lock().unwrap() = "building core".to_string();
            let _ = go_rx.await;
            "2 FAILED".to_string()
        });
        let waited = jobs.wait(None, Duration::from_millis(50)).await.unwrap();
        match waited {
            Waited::Running { id, progress, .. } => {
                assert_eq!(id, 1);
                assert_eq!(progress, "building core");
            }
            other => panic!("{other:?}"),
        }
        assert!(jobs.running().is_some());
        // Nothing to announce yet.
        let before = jobs.world_state_section();
        let snapshot = before.snapshot().clone();
        assert_eq!(before.render_diff(PreviousWorldStateSection::Absent), None);
        go_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while jobs.running().is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let after = jobs.world_state_section();
        let shown = after
            .render_diff(PreviousWorldStateSection::Known(&snapshot))
            .expect("announced");
        assert!(
            shown
                .body()
                .contains("Background run 1 of cpp_test_select finished:\n2 FAILED")
        );
        // The next request (previous = this snapshot) does not repeat it.
        let again = jobs.world_state_section();
        assert_eq!(
            again.render_diff(PreviousWorldStateSection::Known(after.snapshot())),
            None
        );
        // Nor does a resumed thread with unknown state.
        assert_eq!(again.render_diff(PreviousWorldStateSection::Unknown), None);
    }

    #[tokio::test]
    async fn a_run_fetched_with_wait_is_not_announced_again() {
        let jobs = Jobs::new();
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        jobs.start("long".to_string(), move |_, _| async move {
            let _ = go_rx.await;
            "done".to_string()
        });
        assert!(matches!(
            jobs.wait(None, Duration::from_millis(20)).await,
            Some(Waited::Running { .. })
        ));
        let snapshot = jobs.world_state_section().snapshot().clone();
        go_tx.send(()).unwrap();
        assert_eq!(
            jobs.wait(Some(1), Duration::from_secs(5)).await,
            Some(Waited::Done("done".to_string()))
        );
        let section = jobs.world_state_section();
        assert_eq!(
            section.render_diff(PreviousWorldStateSection::Known(&snapshot)),
            None
        );
    }

    #[tokio::test]
    async fn cancel_reaches_the_run() {
        let jobs = Jobs::new();
        jobs.start("cancellable".to_string(), |mut cancel, _| async move {
            let _ = cancel.wait_for(|c| *c).await;
            "cancelled".to_string()
        });
        assert_eq!(jobs.cancel(None), Some(1));
        assert_eq!(
            jobs.wait(Some(1), Duration::from_secs(5)).await,
            Some(Waited::Done("cancelled".to_string()))
        );
        assert_eq!(jobs.cancel(None), None);
        assert_eq!(
            jobs.summary(),
            vec!["run 1: cancellable — finished".to_string()]
        );
    }
}
