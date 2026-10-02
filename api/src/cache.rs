use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use redis::aio::ConnectionManager;
use redis::FromRedisValue;
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::time::timeout;

use crate::metrics::Metrics;

/// Past this, the cache is slower than just asking Postgres.
const OP_TIMEOUT: Duration = Duration::from_millis(100);
/// After a failure, skip the cache this long so an outage costs one timeout,
/// not one per request. Must exceed every cached-detail TTL: invalidations
/// are skipped while open, so entries from before the trip must have expired
/// by the time the cache is used again.
pub const BREAKER_COOLDOWN: Duration = Duration::from_secs(5);

/// Read-through cache in front of Postgres (Dragonfly, Redis protocol).
/// Never authoritative: every failure degrades to a miss, so an unavailable
/// cache only costs latency. With no CACHE_URL it is disabled entirely.
#[derive(Clone)]
pub struct Cache {
    conn: Option<ConnectionManager>,
    metrics: Arc<Metrics>,
    started: Instant,
    /// Milliseconds since `started` before which the cache is skipped.
    open_until_ms: Arc<AtomicU64>,
}

impl Cache {
    pub async fn connect(url: Option<&str>, metrics: Arc<Metrics>) -> anyhow::Result<Self> {
        let conn = match url {
            Some(url) => Some(redis::Client::open(url)?.get_connection_manager().await?),
            None => None,
        };
        Ok(Self {
            conn,
            metrics,
            started: Instant::now(),
            open_until_ms: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn enabled(&self) -> bool {
        self.conn.is_some()
    }

    pub async fn get_json<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let mut cmd = redis::cmd("GET");
        cmd.arg(key);
        let raw = match self.run::<Option<String>>(key, cmd).await {
            Ok(Some(raw)) => raw,
            Ok(None) => {
                self.metrics.record_cache("miss");
                return None;
            }
            Err(result) => {
                self.metrics.record_cache(result);
                return None;
            }
        };
        match serde_json::from_str(&raw) {
            Ok(value) => {
                self.metrics.record_cache("hit");
                Some(value)
            }
            Err(err) => {
                tracing::warn!(key, error = %err, "cache entry failed to deserialize");
                self.metrics.record_cache("error");
                None
            }
        }
    }

    pub async fn set_json<T: Serialize>(&self, key: &str, value: &T, ttl: Duration) {
        let Ok(raw) = serde_json::to_string(value) else {
            return;
        };
        let mut cmd = redis::cmd("SET");
        cmd.arg(key).arg(raw).arg("PX").arg(ttl.as_millis() as u64);
        let _ = self.run::<()>(key, cmd).await;
    }

    pub async fn delete(&self, key: &str) {
        let mut cmd = redis::cmd("DEL");
        cmd.arg(key);
        let _ = self.run::<()>(key, cmd).await;
    }

    /// Runs one command with the timeout and breaker applied. `Err` carries
    /// the metrics result for a lookup that didn't reach the cache.
    async fn run<T: FromRedisValue>(&self, key: &str, cmd: redis::Cmd) -> Result<T, &'static str> {
        let Some(mut conn) = self.conn.clone() else {
            return Err("disabled");
        };
        if self.elapsed_ms() < self.open_until_ms.load(Relaxed) {
            return Err("bypassed");
        }
        let failure = match timeout(OP_TIMEOUT, cmd.query_async::<T>(&mut conn)).await {
            Ok(Ok(value)) => return Ok(value),
            Ok(Err(err)) => err.to_string(),
            Err(_) => "timed out".to_string(),
        };
        let until = self.elapsed_ms() + BREAKER_COOLDOWN.as_millis() as u64;
        if self.open_until_ms.swap(until, Relaxed) <= self.elapsed_ms() {
            tracing::warn!(key, error = %failure, cooldown_secs = BREAKER_COOLDOWN.as_secs(), "cache unavailable, bypassing");
        }
        Err("error")
    }

    fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}
