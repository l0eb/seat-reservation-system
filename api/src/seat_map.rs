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
//! With several replicas, each one's map must also learn what the others
//! did, through Postgres LISTEN/NOTIFY (see `listen`). How each change is
//! announced follows the same rule:
//!
//! - Cancels and new shows notify inside their transaction, so Postgres
//!   delivers the notice exactly when they commit. A lost cancel notice
//!   would leave a free seat looking taken, so it must not be lost.
//! - Bookings are announced after commit, batched by a background task.
//!   Notifying inside the transaction serialised every booking's commit on
//!   Postgres's global notify lock (p99 80ms -> 730ms in the burst). A lost
//!   booking notice only leaves a seat looking free, which Postgres
//!   declines.
//!
//! A cancel on another replica can still leave a seat looking taken here
//! until its notice arrives, a few milliseconds; that is the one window
//! where this map can wrongly decline. Every `AUDIT_EVERY`, each replica
//! also compares its per-show counts with Postgres and reloads any show
//! that stays different, which heals a booking notice lost to a crash.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::postgres::PgListener;
use sqlx::{PgConnection, PgPool};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::error::Conflict;

/// The Postgres channel replicas use to tell each other about changes.
pub const CHANNEL: &str = "seat_map";
/// Postgres rejects payloads of 8000 bytes or more; past this, a notice
/// asks replicas to reload the show instead of listing every seat.
const MAX_PAYLOAD: usize = 7500;
/// How often a replica checks its map against Postgres.
const AUDIT_EVERY: Duration = Duration::from_secs(15);

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
    /// Bookings waiting to be announced to the other replicas. Unset in
    /// tests, where announcing does nothing.
    outbox: OnceLock<mpsc::UnboundedSender<Notice>>,
}

#[derive(sqlx::FromRow)]
struct SeatRow {
    show_id: Uuid,
    label: String,
    version: i64,
    owner: Option<String>,
}

/// What one replica tells the others, as a `pg_notify` payload.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Notice {
    /// Seats changed owner: `owner` for a booking, None for a cancel.
    Seats {
        show: Uuid,
        owner: Option<String>,
        seats: Vec<(String, i64)>,
    },
    /// Load this show from the database: it is new, or the change was too
    /// big to list.
    Show { show: Uuid },
    /// Several bookings announced together.
    Batch { notices: Vec<Notice> },
}

impl SeatMap {
    /// Every show's seats, from the database.
    pub async fn load(pool: &PgPool) -> anyhow::Result<Self> {
        let map = Self::default();
        map.reload(pool, None).await?;
        Ok(map)
    }

    /// Merge the database's view of one show (or all of them) into the map.
    /// Per seat the higher version wins, so a reload never undoes a newer
    /// change the map already holds.
    async fn reload(&self, pool: &PgPool, show: Option<Uuid>) -> anyhow::Result<()> {
        let rows: Vec<SeatRow> = sqlx::query_as(
            "select s.show_id, s.label, s.version, r.user_id as owner
             from seats s left join reservations r on r.id = s.reservation_id
             where $1::uuid is null or s.show_id = $1",
        )
        .bind(show)
        .fetch_all(pool)
        .await?;
        let users: Vec<String> = sqlx::query_scalar(
            "select distinct user_id from reservations where $1::uuid is null or show_id = $1",
        )
        .bind(show)
        .fetch_all(pool)
        .await?;

        // Users first, as with a booking: a seat must never show this user
        // as its owner while their retries could still be declined.
        self.users_with_reservations
            .write()
            .unwrap()
            .extend(users.into_iter().map(Arc::from));
        let mut shows = self.shows.write().unwrap();
        for row in rows {
            let seats = shows.entry(row.show_id).or_default();
            let fresh = Entry {
                owner: row.owner.map(Arc::from),
                version: row.version,
            };
            match seats.get_mut(&row.label) {
                Some(entry) if entry.version >= fresh.version => {}
                Some(entry) => *entry = fresh,
                None => {
                    seats.insert(row.label, fresh);
                }
            }
        }
        Ok(())
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

    /// Apply what another replica (or this one) committed.
    async fn handle(&self, pool: &PgPool, payload: &str) -> anyhow::Result<()> {
        let notices = match serde_json::from_str::<Notice>(payload)? {
            Notice::Batch { notices } => notices,
            notice => vec![notice],
        };
        for notice in notices {
            self.handle_one(pool, notice).await?;
        }
        Ok(())
    }

    async fn handle_one(&self, pool: &PgPool, notice: Notice) -> anyhow::Result<()> {
        match notice {
            Notice::Seats { show, owner, seats } => {
                if let Some(owner) = &owner {
                    self.add_user(owner);
                }
                let owner: Option<Arc<str>> = owner.map(Arc::from);
                let changes = seats
                    .into_iter()
                    .map(|(label, version)| SeatChange {
                        label,
                        owner: owner.clone(),
                        version,
                    })
                    .collect();
                self.apply(show, changes);
            }
            Notice::Show { show } => self.reload(pool, Some(show)).await?,
            Notice::Batch { notices } => {
                anyhow::bail!("nested batch of {} notices", notices.len())
            }
        }
        Ok(())
    }

    /// Tell the other replicas about a booking that has committed. Fire and
    /// forget: if the notice is lost, they only see the seats as free.
    pub fn announce_booking(&self, show: Uuid, owner: &str, seats: &[(String, i64)]) {
        if let Some(outbox) = self.outbox.get() {
            let _ = outbox.send(Notice::Seats {
                show,
                owner: Some(owner.to_string()),
                seats: seats.to_vec(),
            });
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

/// Load the map and keep it in step with every replica's changes. LISTEN
/// starts before the snapshot is read, so nothing committed in between is
/// missed; notices are handled one at a time, in commit order, so a show is
/// loaded before any later change to it is applied. If the listening
/// connection drops, notices may have been lost: reload everything.
pub async fn listen(pool: PgPool) -> anyhow::Result<Arc<SeatMap>> {
    let mut listener = PgListener::connect_with(&pool).await?;
    listener.listen(CHANNEL).await?;
    let map = Arc::new(SeatMap::load(&pool).await?);

    let (outbox, queued) = mpsc::unbounded_channel();
    let _ = map.outbox.set(outbox);
    tokio::spawn(send_bookings(pool.clone(), queued));
    tokio::spawn(audit(map.clone(), pool.clone()));

    let shared = map.clone();
    tokio::spawn(async move {
        let map = shared;
        loop {
            match listener.try_recv().await {
                Ok(Some(notice)) => {
                    if let Err(err) = map.handle(&pool, notice.payload()).await {
                        tracing::warn!(error = %err, "seat map notice failed, reloading");
                        reload_until_done(&map, &pool).await;
                    }
                }
                // The connection dropped and has been re-established.
                Ok(None) => {
                    tracing::warn!("seat map listener reconnected, reloading");
                    reload_until_done(&map, &pool).await;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "seat map listener failed, retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });
    Ok(map)
}

/// Reload any show whose taken count differs from Postgres's confirmed
/// count while the show is quiet: the same in Postgres on two checks a
/// second apart. Mid-burst the two differ for a moment as a matter of
/// course; a lost notice keeps them apart after the show settles. Reloads
/// only move seats to newer versions, so a needless one is harmless.
async fn audit(map: Arc<SeatMap>, pool: PgPool) {
    let confirmed = || async {
        sqlx::query_as::<_, (Uuid, i64)>(
            "select show_id, count(*) filter (where status = 'confirmed') from seats group by show_id",
        )
        .fetch_all(&pool)
        .await
        .map(|rows| rows.into_iter().collect::<HashMap<Uuid, i64>>())
    };
    loop {
        tokio::time::sleep(AUDIT_EVERY).await;
        let Ok(first) = confirmed().await else {
            continue;
        };
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Ok(second) = confirmed().await else {
            continue;
        };
        let ours: HashMap<Uuid, i64> = map.taken_counts().into_iter().collect();
        for (show, count) in &second {
            let quiet = first.get(show) == Some(count);
            if quiet && ours.get(show).is_some_and(|taken| taken != count) {
                tracing::warn!(%show, "seat map differs from the database, reloading the show");
                if let Err(err) = map.reload(&pool, Some(*show)).await {
                    tracing::warn!(error = %err, "seat map reload failed");
                }
            }
        }
    }
}

/// Announce queued bookings, as few notices as fit: whatever queued while
/// the last one was being sent goes out together.
async fn send_bookings(pool: PgPool, mut queued: mpsc::UnboundedReceiver<Notice>) {
    while let Some(first) = queued.recv().await {
        let mut notices = vec![first];
        while let Ok(next) = queued.try_recv() {
            notices.push(next);
        }
        for payload in pack(notices) {
            let sent = sqlx::query("select pg_notify($1, $2)")
                .bind(CHANNEL)
                .bind(&payload)
                .execute(&pool)
                .await;
            if let Err(err) = sent {
                // Harmless: the other replicas see these seats as free.
                tracing::warn!(error = %err, "booking notice not sent");
            }
        }
    }
}

/// Group notices into payloads under Postgres's size limit. A notice too
/// big on its own becomes "reload this show".
fn pack(notices: Vec<Notice>) -> Vec<String> {
    let json = |n: &Notice| serde_json::to_string(n).expect("serializable");
    let mut payloads = Vec::new();
    let mut batch: Vec<Notice> = Vec::new();
    let mut size = 0;
    for notice in notices {
        let mut notice_size = json(&notice).len();
        let notice = if notice_size > MAX_PAYLOAD - 64 {
            let show = match notice {
                Notice::Seats { show, .. } | Notice::Show { show } => show,
                Notice::Batch { .. } => continue,
            };
            let reload = Notice::Show { show };
            notice_size = json(&reload).len();
            reload
        } else {
            notice
        };
        if !batch.is_empty() && size + notice_size + 1 > MAX_PAYLOAD - 64 {
            payloads.push(json(&Notice::Batch {
                notices: std::mem::take(&mut batch),
            }));
            size = 0;
        }
        size += notice_size + 1;
        batch.push(notice);
    }
    if !batch.is_empty() {
        payloads.push(json(&Notice::Batch { notices: batch }));
    }
    payloads
}

async fn reload_until_done(map: &SeatMap, pool: &PgPool) {
    while let Err(err) = map.reload(pool, None).await {
        tracing::warn!(error = %err, "seat map reload failed, retrying");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Tell every replica about seat changes. Call inside the transaction that
/// made them: Postgres delivers the notice only if it commits.
pub async fn notify_seats(
    conn: &mut PgConnection,
    show: Uuid,
    owner: Option<&str>,
    seats: &[(String, i64)],
) -> sqlx::Result<()> {
    let notice = Notice::Seats {
        show,
        owner: owner.map(str::to_string),
        seats: seats.to_vec(),
    };
    let payload = serde_json::to_string(&notice).expect("serializable");
    if payload.len() > MAX_PAYLOAD {
        return notify_show(conn, show).await;
    }
    send(conn, &payload).await
}

/// Tell every replica to load a show from the database. Same rule: inside
/// the transaction that created or changed it.
pub async fn notify_show(conn: &mut PgConnection, show: Uuid) -> sqlx::Result<()> {
    let payload = serde_json::to_string(&Notice::Show { show }).expect("serializable");
    send(conn, &payload).await
}

async fn send(conn: &mut PgConnection, payload: &str) -> sqlx::Result<()> {
    sqlx::query("select pg_notify($1, $2)")
        .bind(CHANNEL)
        .bind(payload)
        .execute(conn)
        .await?;
    Ok(())
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

    /// A pool that never connects: seat notices don't touch the database.
    fn no_db() -> PgPool {
        PgPool::connect_lazy("postgres://unused@localhost/unused").unwrap()
    }

    fn notice(show: Uuid, owner: Option<&str>, seats: &[(&str, i64)]) -> String {
        serde_json::to_string(&Notice::Seats {
            show,
            owner: owner.map(str::to_string),
            seats: seats.iter().map(|(l, v)| (l.to_string(), *v)).collect(),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn a_booking_on_another_replica_is_declined_here() {
        let show = Uuid::new_v4();
        let map = map_with(show, &["A1"]);
        map.handle(&no_db(), &notice(show, Some("bob"), &[("A1", 1)]))
            .await
            .unwrap();
        assert_eq!(
            map.check(show, "alice", &seats(&["A1"]), 4),
            Gate::Decline(Conflict::SeatTaken)
        );
        // bob may be retrying that booking here: it must reach Postgres.
        assert_eq!(map.check(show, "bob", &seats(&["A1"]), 4), Gate::Database);
    }

    #[tokio::test]
    async fn a_cancel_on_another_replica_frees_the_seat_here() {
        let show = Uuid::new_v4();
        let map = map_with(show, &["A1"]);
        map.handle(&no_db(), &notice(show, Some("bob"), &[("A1", 1)]))
            .await
            .unwrap();
        map.handle(&no_db(), &notice(show, None, &[("A1", 2)]))
            .await
            .unwrap();
        assert_eq!(map.check(show, "alice", &seats(&["A1"]), 4), Gate::Database);
    }

    #[tokio::test]
    async fn an_old_notice_cannot_retake_a_freed_seat() {
        let show = Uuid::new_v4();
        let map = map_with(show, &["A1"]);
        map.handle(&no_db(), &notice(show, None, &[("A1", 2)]))
            .await
            .unwrap();
        map.handle(&no_db(), &notice(show, Some("bob"), &[("A1", 1)]))
            .await
            .unwrap();
        assert_eq!(map.taken_counts(), vec![(show, 0)]);
    }

    #[tokio::test]
    async fn garbage_notices_are_errors_not_panics() {
        let map = SeatMap::default();
        assert!(map.handle(&no_db(), "not json").await.is_err());
    }

    #[tokio::test]
    async fn a_batch_applies_every_booking_in_it() {
        let show = Uuid::new_v4();
        let map = map_with(show, &["A1", "A2"]);
        let notices = vec![
            Notice::Seats {
                show,
                owner: Some("bob".into()),
                seats: vec![("A1".into(), 1)],
            },
            Notice::Seats {
                show,
                owner: Some("eve".into()),
                seats: vec![("A2".into(), 1)],
            },
        ];
        for payload in pack(notices) {
            map.handle(&no_db(), &payload).await.unwrap();
        }
        assert_eq!(map.taken_counts(), vec![(show, 2)]);
    }

    #[test]
    fn packed_payloads_stay_under_the_limit() {
        let show = Uuid::new_v4();
        let notices: Vec<Notice> = (0..2000)
            .map(|i| Notice::Seats {
                show,
                owner: Some(format!("user-{i}")),
                seats: vec![(format!("A{i}"), 1)],
            })
            .collect();
        let payloads = pack(notices);
        assert!(payloads.len() > 1);
        assert!(payloads.iter().all(|p| p.len() < MAX_PAYLOAD));
        let total: usize = payloads
            .iter()
            .map(|p| match serde_json::from_str::<Notice>(p).unwrap() {
                Notice::Batch { notices } => notices.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(total, 2000);
    }

    #[test]
    fn an_oversized_booking_becomes_a_show_reload() {
        let show = Uuid::new_v4();
        let seats = (0..2000).map(|i| (format!("SEAT-{i}"), 1)).collect();
        let payloads = pack(vec![Notice::Seats {
            show,
            owner: Some("bob".into()),
            seats,
        }]);
        assert_eq!(payloads.len(), 1);
        assert!(payloads[0].contains("\"kind\":\"show\""));
    }
}
