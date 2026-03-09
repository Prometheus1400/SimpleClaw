use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tracing::{Instrument, info_span};

pub(crate) type BoxFutureUnit = Pin<Box<dyn Future<Output = ()> + Send>>;
pub(crate) type SessionHandler<T> = Arc<dyn Fn(T) -> BoxFutureUnit + Send + Sync>;

#[derive(Clone)]
pub(crate) struct SessionWorkerCoordinator<T> {
    workers: Arc<AsyncMutex<HashMap<String, SessionWorker<T>>>>,
    next_worker_id: Arc<AtomicU64>,
    idle_timeout: Duration,
}

#[derive(Clone)]
struct SessionWorker<T> {
    id: u64,
    tx: mpsc::UnboundedSender<T>,
}

impl<T> SessionWorkerCoordinator<T>
where
    T: Send + 'static,
{
    pub(crate) fn new(idle_timeout: Duration) -> Self {
        Self {
            workers: Arc::new(AsyncMutex::new(HashMap::new())),
            next_worker_id: Arc::new(AtomicU64::new(1)),
            idle_timeout,
        }
    }

    pub(crate) async fn dispatch(
        &self,
        key: String,
        message: T,
        handler: SessionHandler<T>,
    ) -> bool {
        let mut pending = Some(message);

        for attempt in 0..=1 {
            let (worker_id, tx) = self.worker_sender_for_key(&key, Arc::clone(&handler)).await;
            let payload = pending
                .take()
                .expect("pending message is always present before send attempt");

            match tx.send(payload) {
                Ok(()) => return true,
                Err(err) => {
                    pending = Some(err.0);
                    self.remove_worker_if_matches(&key, worker_id).await;

                    if attempt == 1 {
                        tracing::error!(
                            session_key = %key,
                            "dropping inbound message after worker enqueue retries were exhausted"
                        );
                        return false;
                    }
                }
            }
        }

        false
    }

    async fn worker_sender_for_key(
        &self,
        key: &str,
        handler: SessionHandler<T>,
    ) -> (u64, mpsc::UnboundedSender<T>) {
        let mut workers = self.workers.lock().await;
        if let Some(existing) = workers.get(key) {
            return (existing.id, existing.tx.clone());
        }

        let worker_id = self.next_worker_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        workers.insert(
            key.to_owned(),
            SessionWorker {
                id: worker_id,
                tx: tx.clone(),
            },
        );
        drop(workers);

        self.spawn_worker(key.to_owned(), worker_id, rx, handler);
        (worker_id, tx)
    }

    fn spawn_worker(
        &self,
        key: String,
        worker_id: u64,
        mut rx: mpsc::UnboundedReceiver<T>,
        handler: SessionHandler<T>,
    ) {
        let workers = Arc::clone(&self.workers);
        let idle_timeout = self.idle_timeout;
        let worker_key = key.clone();
        tokio::spawn(async move {
            loop {
                let next = tokio::time::timeout(idle_timeout, rx.recv()).await;
                let Some(message) = (match next {
                    Ok(Some(message)) => Some(message),
                    Ok(None) => None,
                    Err(_) => {
                        tracing::debug!(status = "idled_out", session_id = %key, "session worker");
                        None
                    }
                }) else {
                    break;
                };

                handler(message).await;
            }

            let mut workers = workers.lock().await;
            if workers.get(&key).is_some_and(|entry| entry.id == worker_id) {
                workers.remove(&key);
            }
        }
        .instrument(info_span!("session.worker", session_id = %worker_key)));
    }

    async fn remove_worker_if_matches(&self, key: &str, expected_worker_id: u64) {
        let mut workers = self.workers.lock().await;
        if workers
            .get(key)
            .is_some_and(|entry| entry.id == expected_worker_id)
        {
            workers.remove(key);
        }
    }

    #[cfg(test)]
    async fn worker_count(&self) -> usize {
        self.workers.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::sync::{Barrier, mpsc, oneshot};
    use tokio::time::{Duration, sleep, timeout};

    use super::{AsyncMutex, SessionHandler, SessionWorkerCoordinator};

    #[derive(Debug)]
    struct TestMessage {
        id: usize,
    }

    fn boxed_handler<F, Fut>(f: F) -> SessionHandler<TestMessage>
    where
        F: Fn(TestMessage) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Arc::new(move |message| Box::pin(f(message)))
    }

    #[tokio::test]
    async fn session_workers_serialize_messages_for_same_key() {
        let coordinator = SessionWorkerCoordinator::new(Duration::from_secs(60));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let (first_started_tx, first_started_rx) = oneshot::channel::<()>();
        let (second_started_tx, second_started_rx) = oneshot::channel::<()>();
        let (release_first_tx, release_first_rx) = oneshot::channel::<()>();
        let release_first_rx = Arc::new(AsyncMutex::new(Some(release_first_rx)));
        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<usize>();
        let first_started_tx = Arc::new(AsyncMutex::new(Some(first_started_tx)));
        let second_started_tx = Arc::new(AsyncMutex::new(Some(second_started_tx)));

        let handler = boxed_handler({
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            let release_first_rx = Arc::clone(&release_first_rx);
            let done_tx = done_tx.clone();
            let first_started_tx = Arc::clone(&first_started_tx);
            let second_started_tx = Arc::clone(&second_started_tx);
            move |message: TestMessage| {
                let active = Arc::clone(&active);
                let max_active = Arc::clone(&max_active);
                let release_first_rx = Arc::clone(&release_first_rx);
                let done_tx = done_tx.clone();
                let first_started_tx = Arc::clone(&first_started_tx);
                let second_started_tx = Arc::clone(&second_started_tx);
                async move {
                    let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                    loop {
                        let prev = max_active.load(Ordering::SeqCst);
                        if now_active <= prev {
                            break;
                        }
                        if max_active
                            .compare_exchange(prev, now_active, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                        {
                            break;
                        }
                    }

                    if message.id == 1 {
                        if let Some(tx) = first_started_tx.lock().await.take() {
                            let _ = tx.send(());
                        }
                        if let Some(rx) = release_first_rx.lock().await.take() {
                            let _ = rx.await;
                        }
                    }

                    if message.id == 2
                        && let Some(tx) = second_started_tx.lock().await.take()
                    {
                        let _ = tx.send(());
                    }

                    active.fetch_sub(1, Ordering::SeqCst);
                    let _ = done_tx.send(message.id);
                }
            }
        });

        coordinator
            .dispatch(
                "session-a".to_owned(),
                TestMessage { id: 1 },
                Arc::clone(&handler),
            )
            .await;
        coordinator
            .dispatch(
                "session-a".to_owned(),
                TestMessage { id: 2 },
                Arc::clone(&handler),
            )
            .await;

        first_started_rx
            .await
            .expect("first message should begin processing");
        assert!(
            timeout(Duration::from_millis(100), second_started_rx)
                .await
                .is_err(),
            "second message should not start before first is released"
        );

        let _ = release_first_tx.send(());
        let _ = timeout(Duration::from_secs(1), done_rx.recv())
            .await
            .expect("first completion should arrive")
            .expect("first completion value should exist");
        let _ = timeout(Duration::from_secs(1), done_rx.recv())
            .await
            .expect("second completion should arrive")
            .expect("second completion value should exist");

        assert_eq!(max_active.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn session_workers_run_different_keys_concurrently() {
        let coordinator = SessionWorkerCoordinator::new(Duration::from_secs(60));
        let barrier = Arc::new(Barrier::new(3));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));

        let handler = boxed_handler({
            let barrier = Arc::clone(&barrier);
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            move |_message: TestMessage| {
                let barrier = Arc::clone(&barrier);
                let active = Arc::clone(&active);
                let max_active = Arc::clone(&max_active);
                async move {
                    let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                    loop {
                        let prev = max_active.load(Ordering::SeqCst);
                        if now_active <= prev {
                            break;
                        }
                        if max_active
                            .compare_exchange(prev, now_active, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                        {
                            break;
                        }
                    }
                    barrier.wait().await;
                    active.fetch_sub(1, Ordering::SeqCst);
                }
            }
        });

        coordinator
            .dispatch(
                "session-a".to_owned(),
                TestMessage { id: 1 },
                Arc::clone(&handler),
            )
            .await;
        coordinator
            .dispatch(
                "session-b".to_owned(),
                TestMessage { id: 2 },
                Arc::clone(&handler),
            )
            .await;

        timeout(Duration::from_secs(1), barrier.wait())
            .await
            .expect("both session workers should run concurrently");
        assert!(
            max_active.load(Ordering::SeqCst) >= 2,
            "expected at least two concurrent handlers"
        );
    }

    #[tokio::test]
    async fn session_worker_expires_when_idle_and_respawns() {
        let coordinator = SessionWorkerCoordinator::new(Duration::from_millis(50));
        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<usize>();
        let handler = boxed_handler(move |message: TestMessage| {
            let done_tx = done_tx.clone();
            async move {
                let _ = done_tx.send(message.id);
            }
        });

        coordinator
            .dispatch(
                "session-a".to_owned(),
                TestMessage { id: 1 },
                Arc::clone(&handler),
            )
            .await;
        let first = timeout(Duration::from_secs(1), done_rx.recv())
            .await
            .expect("first completion should arrive")
            .expect("first completion payload should exist");
        assert_eq!(first, 1);

        for _ in 0..20 {
            if coordinator.worker_count().await == 0 {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(coordinator.worker_count().await, 0);

        coordinator
            .dispatch(
                "session-a".to_owned(),
                TestMessage { id: 2 },
                Arc::clone(&handler),
            )
            .await;
        let second = timeout(Duration::from_secs(1), done_rx.recv())
            .await
            .expect("second completion should arrive")
            .expect("second completion payload should exist");
        assert_eq!(second, 2);
    }
}
