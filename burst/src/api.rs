//! A thin client for the service. Every call returns what the burst report
//! needs, including requests that never got a response.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use futures::stream::{self, StreamExt, TryStreamExt};
use serde_json::{json, Value};

use crate::metrics::Metrics;

const MINT_CONCURRENCY: usize = 256;

#[derive(Clone)]
pub struct Api {
    client: reqwest::Client,
    base: String,
}

/// One request's outcome. `status` is None when no response arrived
/// (timeout, refused connection): the request may or may not have applied.
pub struct Reply {
    pub status: Option<u16>,
    pub body: Value,
    pub latency: Duration,
}

impl Reply {
    /// "201", "409 seat_taken", "503 overloaded", "no response".
    pub fn label(&self) -> String {
        match (self.status, self.body["error"].as_str()) {
            (None, _) => "no response".into(),
            (Some(status), Some(error)) => format!("{status} {error}"),
            (Some(status), None) => status.to_string(),
        }
    }

    pub fn is(&self, status: u16) -> bool {
        self.status == Some(status)
    }

    pub fn is_conflict(&self, reason: &str) -> bool {
        self.is(409) && self.body["error"] == reason
    }

    /// The reservation in a reserve or cancel response.
    pub fn reservation_id(&self) -> Option<&str> {
        self.body["reservation_id"].as_str()
    }

    /// The show in a create-show response.
    pub fn show_id(&self) -> Option<&str> {
        self.body["id"].as_str()
    }
}

impl Api {
    pub fn new(base: &str, timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .pool_max_idle_per_host(4096)
            .build()?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_string(),
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Reply {
        let started = Instant::now();
        match request.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let body = response.json().await.unwrap_or(Value::Null);
                Reply {
                    status: Some(status),
                    body,
                    latency: started.elapsed(),
                }
            }
            Err(_) => Reply {
                status: None,
                body: Value::Null,
                latency: started.elapsed(),
            },
        }
    }

    pub async fn ready(&self) -> Result<()> {
        let reply = self.send(self.client.get(self.url("/readyz"))).await;
        if !reply.is(200) {
            bail!(
                "{} is not ready: GET /readyz -> {}",
                self.base,
                reply.label()
            );
        }
        Ok(())
    }

    /// Needs the service's test-token route (AUTH_TOKEN_ROUTE_ENABLED=true).
    pub async fn mint(&self, user_id: &str, admin: bool) -> Result<String> {
        let mut body = json!({ "user_id": user_id });
        if admin {
            body["role"] = json!("admin");
        }
        let reply = self
            .send(self.client.post(self.url("/auth/token")).json(&body))
            .await;
        match reply.body["token"].as_str() {
            Some(token) if reply.is(200) => Ok(token.to_string()),
            _ => bail!(
                "POST /auth/token -> {} (is AUTH_TOKEN_ROUTE_ENABLED=true?)",
                reply.label()
            ),
        }
    }

    /// Tokens for users `{prefix}-0` .. `{prefix}-{n-1}`, in order.
    pub async fn mint_users(&self, prefix: &str, n: usize) -> Result<Vec<String>> {
        stream::iter(0..n)
            .map(|i| async move { self.mint(&format!("{prefix}-{i}"), false).await })
            .buffered(MINT_CONCURRENCY)
            .try_collect()
            .await
    }

    pub async fn create_show(&self, admin: &str, spec: &ShowSpec, key: &str) -> Reply {
        // The brief's shape: every seat listed out.
        let body = json!({
            "name": spec.name,
            "seats": spec.labels(),
            "price_paise": 100,
            "per_user_limit": spec.per_user_limit,
        });
        let request = self
            .client
            .post(self.url("/shows"))
            .bearer_auth(admin)
            .header("idempotency-key", key)
            .json(&body);
        self.send(request).await
    }

    /// Creates the show and returns its id.
    pub async fn new_show(&self, admin: &str, spec: &ShowSpec) -> Result<String> {
        let reply = self
            .create_show(admin, spec, &uuid::Uuid::new_v4().to_string())
            .await;
        match reply.show_id() {
            Some(id) if reply.is(201) => Ok(id.to_string()),
            _ => bail!("POST /shows -> {}", reply.label()),
        }
    }

    pub async fn reserve(&self, token: &str, show: &str, seats: &[String], key: &str) -> Reply {
        let request = self
            .client
            .post(self.url(&format!("/shows/{show}/reserve")))
            .bearer_auth(token)
            .json(&json!({ "seats": seats, "idempotency_key": key }));
        self.send(request).await
    }

    pub async fn cancel(&self, token: &str, reservation: &str) -> Reply {
        let request = self
            .client
            .post(self.url(&format!("/reservations/{reservation}/cancel")))
            .bearer_auth(token);
        self.send(request).await
    }

    pub async fn show(&self, show: &str) -> Result<ShowState> {
        let reply = self
            .send(self.client.get(self.url(&format!("/shows/{show}"))))
            .await;
        if !reply.is(200) {
            bail!("GET /shows/{show} -> {}", reply.label());
        }
        ShowState::from_json(&reply.body).context("unexpected GET /shows/{id} body")
    }

    pub async fn metrics(&self) -> Result<Metrics> {
        let text = self
            .client
            .get(self.url("/metrics"))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(Metrics::parse(&text))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

pub struct ShowSpec {
    pub name: String,
    pub rows: u32,
    pub seats_per_row: u32,
    pub per_user_limit: u32,
}

impl ShowSpec {
    /// A1..A{seats_per_row}, B1.., one letter per row.
    pub fn labels(&self) -> Vec<String> {
        (0..self.rows)
            .flat_map(|r| {
                let row = (b'A' + r as u8) as char;
                (1..=self.seats_per_row).map(move |n| format!("{row}{n}"))
            })
            .collect()
    }
}

/// GET /shows/{id}, reduced to what the checks compare.
pub struct ShowState {
    pub total_seats: i64,
    pub available: i64,
    pub held: i64,
    pub confirmed: i64,
    /// Labels whose status is "confirmed".
    pub confirmed_seats: std::collections::BTreeSet<String>,
}

impl ShowState {
    fn from_json(body: &Value) -> Option<Self> {
        let counts = &body["counts"];
        let confirmed_seats = body["seats"]
            .as_array()?
            .iter()
            .filter(|seat| seat["status"] == "confirmed")
            .filter_map(|seat| seat["label"].as_str().map(String::from))
            .collect();
        Some(Self {
            total_seats: body["total_seats"].as_i64()?,
            available: counts["available"].as_i64()?,
            held: counts["held"].as_i64()?,
            confirmed: counts["confirmed"].as_i64()?,
            confirmed_seats,
        })
    }

    pub fn counts_add_up(&self) -> bool {
        self.available + self.held + self.confirmed == self.total_seats
    }
}
