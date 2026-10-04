use std::env;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub cache_url: Option<String>,
    pub jwt_secret: String,
    pub port: u16,
    pub db_pool_max_connections: u32,
    pub db_acquire_timeout_secs: u64,
    pub reserve_semaphore_permits: usize,
    pub reserve_queue_timeout_secs: u64,
    pub db_statement_timeout_secs: u64,
    pub auth_token_route_enabled: bool,
    /// This replica's name in metrics, e.g. "api-1".
    pub replica_id: String,
    /// Base URLs of the other replicas, whose counters /metrics adds in.
    pub peers: Vec<String>,
    /// On SIGTERM, fail /readyz this long before closing connections, so a
    /// load balancer stops routing here first. 0 when there is none.
    pub shutdown_drain_secs: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            database_url: env::var("DATABASE_URL").context("DATABASE_URL must be set")?,
            cache_url: env::var("CACHE_URL").ok().filter(|v| !v.trim().is_empty()),
            jwt_secret: env::var("JWT_SECRET").context("JWT_SECRET must be set")?,
            port: parse_env("PORT", 8080)?,
            db_pool_max_connections: parse_env("DB_POOL_MAX_CONNECTIONS", 30)?,
            db_acquire_timeout_secs: parse_env("DB_ACQUIRE_TIMEOUT_SECS", 5)?,
            reserve_semaphore_permits: parse_env("RESERVE_SEMAPHORE_PERMITS", 24)?,
            reserve_queue_timeout_secs: parse_env("RESERVE_QUEUE_TIMEOUT_SECS", 15)?,
            db_statement_timeout_secs: parse_env("DB_STATEMENT_TIMEOUT_SECS", 5)?,
            auth_token_route_enabled: env::var("AUTH_TOKEN_ROUTE_ENABLED")
                .map(|v| v == "true")
                .unwrap_or(false),
            replica_id: env::var("REPLICA_ID")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "api".into()),
            peers: env::var("PEERS")
                .unwrap_or_default()
                .split(',')
                .map(|p| p.trim().trim_end_matches('/').to_string())
                .filter(|p| !p.is_empty())
                .collect(),
            shutdown_drain_secs: parse_env("SHUTDOWN_DRAIN_SECS", 0)?,
        })
    }
}

fn parse_env<T>(key: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env::var(key) {
        Ok(raw) => raw
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("invalid value for {key}: {e}")),
        Err(_) => Ok(default),
    }
}
