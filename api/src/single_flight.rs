//! Merges concurrent identical work: while one call for a key is running,
//! later calls for the same key wait for its result instead of repeating
//! it. Used so a burst of cache misses for one show costs one database read
//! per replica, not one per request.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use futures::future::{BoxFuture, FutureExt, Shared};

pub struct SingleFlight<K, V: Clone> {
    inflight: Arc<Mutex<HashMap<K, Shared<BoxFuture<'static, V>>>>>,
}

impl<K, V> SingleFlight<K, V>
where
    K: Eq + Hash + Clone + Send + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self {
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The result of `work` for `key`, shared with every concurrent caller
    /// for the same key. `work` runs only if no call for `key` is running.
    pub async fn run<F>(&self, key: K, work: impl FnOnce() -> F) -> V
    where
        F: Future<Output = V> + Send + 'static,
    {
        let shared = {
            let mut inflight = self.inflight.lock().unwrap();
            match inflight.get(&key) {
                Some(running) => running.clone(),
                None => {
                    // The work removes its own entry when it finishes, so the
                    // entry goes even if the caller that started it is
                    // cancelled and another waiter finishes the work.
                    let map = self.inflight.clone();
                    let k = key.clone();
                    let fut = work();
                    let running = async move {
                        let value = fut.await;
                        map.lock().unwrap().remove(&k);
                        value
                    }
                    .boxed()
                    .shared();
                    inflight.insert(key, running.clone());
                    running
                }
            }
        };
        shared.await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn concurrent_calls_share_one_run() {
        let flight = Arc::new(SingleFlight::<u32, usize>::new());
        let runs = Arc::new(AtomicUsize::new(0));
        let calls = (0..50).map(|_| {
            let (flight, runs) = (flight.clone(), runs.clone());
            tokio::spawn(async move {
                flight
                    .run(7, || async move {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        runs.fetch_add(1, Ordering::SeqCst) + 1
                    })
                    .await
            })
        });
        let results = futures::future::join_all(calls).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(results.into_iter().all(|r| r.unwrap() == 1));
    }

    #[tokio::test]
    async fn later_calls_run_again_and_keys_are_separate() {
        let flight = SingleFlight::<u32, u32>::new();
        assert_eq!(flight.run(1, || async { 10 }).await, 10);
        assert_eq!(flight.run(1, || async { 11 }).await, 11);
        assert_eq!(flight.run(2, || async { 20 }).await, 20);
    }

    #[tokio::test]
    async fn a_cancelled_first_caller_does_not_wedge_the_key() {
        let flight = Arc::new(SingleFlight::<u32, u32>::new());
        let first = {
            let flight = flight.clone();
            tokio::spawn(async move {
                flight
                    .run(1, || async {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        1
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        first.abort();
        // A waiter that arrives later drives the same work to completion...
        assert_eq!(flight.run(1, || async { 2 }).await, 1);
        // ...and the entry is gone afterwards, so new work runs.
        assert_eq!(flight.run(1, || async { 3 }).await, 3);
    }
}
