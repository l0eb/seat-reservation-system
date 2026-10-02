use std::env;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub jwt_secret: String,
    pub port: u16,
    pub db_pool_max_connections: u32,
    pub db_acquire_timeout_secs: u64,
    pub reserve_semaphore_permits: usize,
    pub auth_token_route_enabled: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            database_url: env::var("DATABASE_URL").context("DATABASE_URL must be set")?,
            jwt_secret: env::var("JWT_SECRET").context("JWT_SECRET must be set")?,
            port: parse_env("PORT", 8080)?,
            db_pool_max_connections: parse_env("DB_POOL_MAX_CONNECTIONS", 30)?,
            db_acquire_timeout_secs: parse_env("DB_ACQUIRE_TIMEOUT_SECS", 30)?,
            reserve_semaphore_permits: parse_env("RESERVE_SEMAPHORE_PERMITS", 60)?,
            auth_token_route_enabled: env::var("AUTH_TOKEN_ROUTE_ENABLED")
                .map(|v| v == "true")
                .unwrap_or(false),
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
