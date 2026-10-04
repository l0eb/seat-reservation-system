//! In-memory copy of who holds each seat, used only to decline requests that
//! can't succeed without going to Postgres. Most of a burst asks for seats
//! that are already gone; answering those here leaves the database to the
//! requests that might win.
//!
//! Postgres still decides every booking. This map may lag it, but must never
//! say a seat is taken when the database says it's free, since that would
//! turn away a buyer for a seat they could have had. Two rules keep it so:
//!
//! - Every seat write in SQL bumps `seats.version`, and the map applies an
//!   update only if it is newer than what it holds. Updates that arrive out
//!   of order can't put back an old owner.
//! - Bookings update the map after they commit, cancels before. Any lag then
//!   leaves a seat looking free, which costs a database check, never a
//!   wrong decline.
//!
//! Correct only while the service runs as a single instance.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use sqlx::PgPool;
use uuid::Uuid;

use crate::error::Conflict;

#[derive(Clone)]
struct Entry {
    owner: Option<Arc<str>>,
    version: i64,
}

/// A change to one seat, as Postgres reported it.
pub struct SeatChange {
    pub label: String,
    pub owner: Option<Arc<str>>,
    pub version: i64,
}

/// What the map can say about a reserve request without the database.
#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    /// Labels that don't exist on this show.
    UnknownSeats(Vec<String>),
    /// Certain to lose; no need to ask Postgres.
    Decline(Conflict),
    /// Might succeed, or is a possible retry: Postgres decides.
    Database,
}

#[derive(Default)]
pub struct SeatMap {
    shows: RwLock<HashMap<Uuid, HashMap<String, Entry>>>,
    /// Users with at least one reservation (rows are never deleted). Only
    /// these can be retrying an earlier request, so only their requests
    /// need the idempotency lookup before a decline.
    users_with_reservations: RwLock<HashSet<Arc<str>>>,
}

#[derive(sqlx::FromRow)]
struct SeatRow {
    show_id: Uuid,
    label: String,
    version: i64,
    owner: Option<String>,
}

impl SeatMap {
    pub async fn load(pool: &PgPool) -> anyhow::Result<Self> {
        let rows: Vec<SeatRow> = sqlx::query_as(
            "select s.show_id, s.label, s.version, r.user_id as owner
             from seats s left join reservations r on r.id = s.reservation_id",
        )
        .fetch_all(pool)
        .await?;
        let users: Vec<String> = sqlx::query_scalar("select distinct user_id from reservations")
            .fetch_all(pool)
            .await?;

        let map = Self::default();
        {
            let mut shows = map.shows.write().unwrap();
            for row in rows {
                shows.entry(row.show_id).or_default().insert(
                    row.label,
                    Entry {
                        owner: row.owner.map(Arc::from),
                        version: row.version,
                    },
                );
            }
        }
        map.users_with_reservations
            .write()
            .unwrap()
            .extend(users.into_iter().map(Arc::from));
        Ok(map)
    }

    /// Call after the show's transaction commits.
    pub fn add_show(&self, show_id: Uuid, labels: &[String]) {
        let seats = labels
            .iter()
            .map(|label| {
                let entry = Entry {
                    owner: None,
                    version: 0,
                };
                (label.clone(), entry)
            })
            .collect();
        self.shows.write().unwrap().insert(show_id, seats);
    }

    pub fn check(
        &self,
        show_id: Uuid,
        user_id: &str,
        seats: &[String],
        per_user_limit: i32,
    ) -> Gate {
        let shows = self.shows.read().unwrap();
        // Not loaded yet (created a moment ago): let Postgres answer.
        let Some(show) = shows.get(&show_id) else {
            return Gate::Database;
        };
        let unknown: Vec<String> = seats
            .iter()
            .filter(|s| !show.contains_key(*s))
            .cloned()
            .collect();
        if !unknown.is_empty() {
            return Gate::UnknownSeats(unknown);
        }
        if self
            .users_with_reservations
            .read()
            .unwrap()
            .contains(user_id)
        {
            return Gate::Database;
        }
        if seats.len() > per_user_limit as usize {
            return Gate::Decline(Conflict::PerUserLimit);
        }
        let taken = seats.iter().any(|s| {
            show[s]
                .owner
                .as_deref()
                .is_some_and(|owner| owner != user_id)
        });
        if taken {
            Gate::Decline(Conflict::SeatTaken)
        } else {
            Gate::Database
        }
    }

    /// Bookings call this after commit, cancels before.
    pub fn apply(&self, show_id: Uuid, changes: Vec<SeatChange>) {
        let mut shows = self.shows.write().unwrap();
        let Some(show) = shows.get_mut(&show_id) else {
            return;
        };
        for change in changes {
            if let Some(entry) = show.get_mut(&change.label) {
                if change.version > entry.version {
                    *entry = Entry {
                        owner: change.owner,
                        version: change.version,
                    };
                }
            }
        }
    }

    /// Call after the user's first reservation commits, before `apply`.
    pub fn add_user(&self, user_id: &str) {
        let mut users = self.users_with_reservations.write().unwrap();
        if !users.contains(user_id) {
            users.insert(Arc::from(user_id));
        }
    }

    /// Seats the map believes are taken, per show. At rest this must equal
    /// the confirmed count in Postgres.
    pub fn taken_counts(&self) -> Vec<(Uuid, i64)> {
        self.shows
            .read()
            .unwrap()
            .iter()
            .map(|(id, seats)| {
                (
                    *id,
                    seats.values().filter(|e| e.owner.is_some()).count() as i64,
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_with(show: Uuid, labels: &[&str]) -> SeatMap {
        let map = SeatMap::default();
        let labels: Vec<String> = labels.iter().map(|s| s.to_string()).collect();
        map.add_show(show, &labels);
        map
    }

    fn change(label: &str, owner: Option<&str>, version: i64) -> SeatChange {
        SeatChange {
            label: label.into(),
            owner: owner.map(Arc::from),
            version,
        }
    }

    fn seats(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn free_seats_go_to_the_database() {
        let show = Uuid::new_v4();
        let map = map_with(show, &["A1", "A2"]);
        assert_eq!(map.check(show, "u", &seats(&["A1"]), 4), Gate::Database);
    }

    #[test]
    fn seats_held_by_others_are_declined_without_the_database() {
        let show = Uuid::new_v4();
        let map = map_with(show, &["A1", "A2"]);
        map.apply(show, vec![change("A1", Some("bob"), 1)]);
        assert_eq!(
            map.check(show, "alice", &seats(&["A1", "A2"]), 4),
            Gate::Decline(Conflict::SeatTaken)
        );
    }

    #[test]
    fn users_with_reservations_always_reach_the_database() {
        // They may be retrying: Postgres must get the chance to replay.
        let show = Uuid::new_v4();
        let map = map_with(show, &["A1"]);
        map.apply(show, vec![change("A1", Some("bob"), 1)]);
        map.add_user("alice");
        assert_eq!(map.check(show, "alice", &seats(&["A1"]), 4), Gate::Database);
    }

    #[test]
    fn unknown_labels_are_reported() {
        let show = Uuid::new_v4();
        let map = map_with(show, &["A1"]);
        assert_eq!(
            map.check(show, "u", &seats(&["A1", "Z9"]), 4),
            Gate::UnknownSeats(vec!["Z9".into()])
        );
    }

    #[test]
    fn unknown_shows_go_to_the_database() {
        let map = SeatMap::default();
        assert_eq!(
            map.check(Uuid::new_v4(), "u", &seats(&["A1"]), 4),
            Gate::Database
        );
    }

    #[test]
    fn a_late_booking_update_cannot_undo_a_cancel() {
        // Booking committed at v1, cancel at v2, but the booking's update
        // reaches the map last: the seat must stay free.
        let show = Uuid::new_v4();
        let map = map_with(show, &["A1"]);
        map.apply(show, vec![change("A1", None, 2)]);
        map.apply(show, vec![change("A1", Some("bob"), 1)]);
        assert_eq!(map.check(show, "alice", &seats(&["A1"]), 4), Gate::Database);
        assert_eq!(map.taken_counts(), vec![(show, 0)]);
    }
}
