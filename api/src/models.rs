use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Show {
    pub id: Uuid,
    pub name: String,
    pub price_paise: i64,
    pub per_user_limit: i32,
    pub total_seats: i32,
    pub created_at: DateTime<Utc>,
}

pub const SHOW_COLUMNS: &str = "id, name, price_paise, per_user_limit, total_seats, created_at";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum SeatStatus {
    Available,
    Held,
    Confirmed,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Seat {
    pub label: String,
    pub status: SeatStatus,
}

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Reservation {
    #[serde(rename = "reservation_id")]
    pub id: Uuid,
    pub show_id: Uuid,
    pub user_id: String,
    pub seats: Vec<String>,
    pub amount_paise: i64,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

pub const RESERVATION_COLUMNS: &str =
    "id, show_id, user_id, seats, amount_paise, status, created_at";
