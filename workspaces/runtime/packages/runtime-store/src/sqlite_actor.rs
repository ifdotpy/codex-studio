//! Bounded admission to the single asynchronous Canvas writer actor.

use std::path::Path;
use std::sync::Arc;

use crate::{RuntimeLock, StoreError, StoreResult};
use rusqlite::OpenFlags;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;

type ActorResponseCompletion<R, E> =
    Box<dyn FnOnce(&Result<R, E>, ActorResponseDisposition) + Send>;

pub(crate) struct ActorResponse<R, E> {
    result: Option<Result<R, E>>,
    completion: Option<ActorResponseCompletion<R, E>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActorResponseDisposition {
    Delivered,
    Abandoned,
}

impl<R, E> ActorResponse<R, E> {
    pub(crate) fn resolve_with(mut self, on_resolve: impl FnOnce(&Result<R, E>)) -> Result<R, E> {
        // `result` is populated at construction and `resolve_with` consumes `self`.
        let result = self
            .result
            .take()
            .expect("actor response is resolved exactly once");
        let on_resolve_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            on_resolve(&result);
        }));
        let completion_result = self.completion.take().map(|completion| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                completion(&result, ActorResponseDisposition::Delivered);
            }))
        });
        if let Err(panic) = on_resolve_result {
            std::panic::resume_unwind(panic);
        }
        if let Some(Err(panic)) = completion_result {
            std::panic::resume_unwind(panic);
        }
        result
    }
}

impl<R, E> Drop for ActorResponse<R, E> {
    fn drop(&mut self) {
        if let (Some(result), Some(completion)) = (&self.result, self.completion.take())
            && let Err(_panic_payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    completion(result, ActorResponseDisposition::Abandoned);
                }))
        {
            // A dropped response cannot report observer panics to its caller.
        }
    }
}

/// Maximum admitted Canvas writer jobs, including queued and running jobs.
/// The unit is jobs; saturation waits cancellably before actor submission.
pub(crate) const MAX_PENDING_WRITES: usize = 16;

pub(crate) struct SqliteActor {
    connection: tokio_rusqlite::Connection,
    admission: Arc<Semaphore>,
    _runtime_lock: Arc<RuntimeLock>,
}

impl SqliteActor {
    pub(crate) async fn open(path: &Path, runtime_lock: Arc<RuntimeLock>) -> StoreResult<Self> {
        let connection =
            tokio_rusqlite::Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
                .await
                .map_err(StoreError::from)?;
        Ok(Self {
            connection,
            admission: Arc::new(Semaphore::new(MAX_PENDING_WRITES)),
            _runtime_lock: runtime_lock,
        })
    }

    /// Submit one synchronous job. The owned permit and runtime lease move into
    /// the actor callback, so cancellation after submission cannot release
    /// admission or the state-directory lease while that callback can still run.
    pub(crate) async fn call<R, E>(
        &self,
        operation: impl FnOnce(&mut rusqlite::Connection) -> Result<R, E> + Send + 'static,
    ) -> Result<R, tokio_rusqlite::Error<E>>
    where
        R: Send + 'static,
        E: Send + 'static,
    {
        let permit = self
            .acquire()
            .await
            .map_err(|_| tokio_rusqlite::Error::ConnectionClosed)?;
        self.call_admitted(permit, operation).await
    }

    pub(crate) async fn acquire(
        &self,
    ) -> Result<OwnedSemaphorePermit, tokio_rusqlite::Error<rusqlite::Error>> {
        Arc::clone(&self.admission)
            .acquire_owned()
            .await
            .map_err(|_| tokio_rusqlite::Error::ConnectionClosed)
    }

    pub(crate) async fn call_admitted<R, E>(
        &self,
        permit: OwnedSemaphorePermit,
        operation: impl FnOnce(&mut rusqlite::Connection) -> Result<R, E> + Send + 'static,
    ) -> Result<R, tokio_rusqlite::Error<E>>
    where
        R: Send + 'static,
        E: Send + 'static,
    {
        let response = self
            .call_admitted_with_completion(permit, operation, |_, _| {})
            .await?;
        response
            .resolve_with(|_| {})
            .map_err(tokio_rusqlite::Error::Error)
    }

    /// Deliver an owned response and run one synchronous completion callback
    /// either when the response is resolved or when the submitted response is
    /// dropped. The callback runs on the thread that drops or resolves the
    /// response, so it must be short and nonblocking.
    pub(crate) async fn call_admitted_with_completion<R, E>(
        &self,
        permit: OwnedSemaphorePermit,
        operation: impl FnOnce(&mut rusqlite::Connection) -> Result<R, E> + Send + 'static,
        completion: impl FnOnce(&Result<R, E>, ActorResponseDisposition) + Send + 'static,
    ) -> Result<ActorResponse<R, E>, tokio_rusqlite::Error<E>>
    where
        R: Send + 'static,
        E: Send + 'static,
    {
        let runtime_lock = Arc::clone(&self._runtime_lock);
        self.connection
            .call_raw(move |connection| {
                let _permit = permit;
                let _runtime_lock = runtime_lock;
                ActorResponse {
                    result: Some(operation(connection)),
                    completion: Some(Box::new(completion)),
                }
            })
            .await
            .map_err(|_| tokio_rusqlite::Error::ConnectionClosed)
    }

    pub(crate) fn available_admissions(&self) -> usize {
        self.admission.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use std::{future::Future, future::poll_fn, task::Poll};

    #[test]
    fn abandoned_response_receives_operation_error() {
        let observed = Arc::new(AtomicBool::new(false));
        let observed_callback = Arc::clone(&observed);
        let response = ActorResponse::<(), rusqlite::Error> {
            result: Some(Err(rusqlite::Error::InvalidQuery)),
            completion: Some(Box::new(move |result, disposition| {
                observed_callback.store(
                    result.is_err() && disposition == ActorResponseDisposition::Abandoned,
                    Ordering::Release,
                );
            })),
        };
        drop(response);
        assert!(observed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn cancellation_while_saturated_does_not_submit_sql() {
        let directory = tempfile::tempdir().expect("temporary actor directory");
        let database_path = directory.path().join("canvas.sqlite3");
        rusqlite::Connection::open(&database_path)
            .expect("create actor database")
            .close()
            .expect("close fixture database");
        let runtime_lock = Arc::new(RuntimeLock::acquire(directory.path()).expect("runtime lock"));
        let actor = SqliteActor::open(&database_path, runtime_lock)
            .await
            .expect("open SQLite actor");
        let mut permits = Vec::with_capacity(MAX_PENDING_WRITES);
        for _ in 0..MAX_PENDING_WRITES {
            permits.push(actor.acquire().await.expect("fill actor admission"));
        }
        let executed = Arc::new(AtomicBool::new(false));
        let executed_callback = Arc::clone(&executed);
        let mut blocked = Box::pin(actor.call(move |_| {
            executed_callback.store(true, Ordering::Release);
            Ok::<(), rusqlite::Error>(())
        }));
        let first_poll = poll_fn(|context| Poll::Ready(blocked.as_mut().poll(context))).await;
        assert!(
            first_poll.is_pending(),
            "saturated admission waits before enqueue"
        );
        drop(blocked);
        drop(permits);
        tokio::task::yield_now().await;
        assert!(!executed.load(Ordering::Acquire));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn canceled_response_keeps_admission_until_queued_job_finishes() {
        let directory = tempfile::tempdir().expect("temporary actor directory");
        let database_path = directory.path().join("canvas.sqlite3");
        rusqlite::Connection::open(&database_path)
            .expect("create actor database")
            .close()
            .expect("close fixture database");
        let runtime_lock = Arc::new(RuntimeLock::acquire(directory.path()).expect("runtime lock"));
        let actor = Arc::new(
            SqliteActor::open(&database_path, runtime_lock)
                .await
                .expect("open SQLite actor"),
        );
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let first_entered = Arc::clone(&entered);
        let first_release = Arc::clone(&release);
        let first_actor = Arc::clone(&actor);
        let first = tokio::spawn(async move {
            first_actor
                .call(move |_| {
                    first_entered.wait();
                    first_release.wait();
                    Ok::<(), rusqlite::Error>(())
                })
                .await
        });
        entered.wait();

        let second_ran = Arc::new(AtomicBool::new(false));
        let second_ran_callback = Arc::clone(&second_ran);
        // The actor drops the permit when the job returns, then drops the
        // undeliverable response, so admission can return before the callback.
        let (completion_sender, completion_receiver) = tokio::sync::oneshot::channel();
        let permit = actor.acquire().await.expect("second actor admission");
        let second = actor.call_admitted_with_completion(
            permit,
            move |_| {
                second_ran_callback.store(true, Ordering::Release);
                Ok::<(), rusqlite::Error>(())
            },
            move |result, disposition| {
                let _ignored = completion_sender.send((result.is_ok(), disposition));
            },
        );
        let mut second = Box::pin(second);
        let first_poll = poll_fn(|context| Poll::Ready(second.as_mut().poll(context))).await;
        assert!(first_poll.is_pending(), "the actor response remains queued");
        tokio::time::timeout(Duration::from_secs(2), async {
            while actor.available_admissions() != MAX_PENDING_WRITES - 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both actor callbacks acquired admission");
        drop(second);
        assert_eq!(actor.available_admissions(), MAX_PENDING_WRITES - 2);
        release.wait();
        first
            .await
            .expect("first actor task")
            .expect("first actor call");

        tokio::time::timeout(Duration::from_secs(2), async {
            while actor.available_admissions() != MAX_PENDING_WRITES {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both callbacks released admission");
        assert!(second_ran.load(Ordering::Acquire));
        let completion = tokio::time::timeout(Duration::from_secs(2), completion_receiver)
            .await
            .expect("abandoned response completion deadline")
            .expect("abandoned response completion runs once");
        assert_eq!(completion, (true, ActorResponseDisposition::Abandoned));
    }
}
