//! Clients that retry. The same key must always mean the same reservation
//! (or show); the same key with a different request must be refused.

use std::collections::HashSet;

use anyhow::Result;
use futures::future::join_all;
use uuid::Uuid;

use super::{row_a, Ctx};
use crate::api::ShowSpec;
use crate::report::{Tally, Totals};

const NAME: &str = "idempotency";

pub async fn run(ctx: &mut Ctx, users: usize, copies: usize) -> Result<Totals> {
    println!("\n== idempotency: {users} users each send one booking {copies}x in parallel, then reuse the key wrongly, then retry");
    let api = ctx.api.clone();
    let spec = ShowSpec {
        name: ctx.show_name(NAME),
        rows: 1,
        seats_per_row: users as u32,
        per_user_limit: 4,
    };
    let show = api.new_show(&ctx.admin, &spec).await?;
    ctx.shows.push((NAME, show.clone()));
    let tokens = api.mint_users(&ctx.users(NAME), users).await?;
    let keys: Vec<String> = (0..users).map(|_| Uuid::new_v4().to_string()).collect();
    let seats = row_a(users);
    let own = |i: usize| std::slice::from_ref(&seats[i]);
    let other = |i: usize| std::slice::from_ref(&seats[(i + 1) % users]);
    let admin = ctx.admin.clone();
    let create_name = ctx.show_name("idempotent create");

    // Every copy of every user's request, all in flight together.
    let parallel = join_all((0..users).flat_map(|i| {
        let (api, show, tokens, keys, seat) = (&api, &show, &tokens, &keys, own(i));
        (0..copies)
            .map(move |_| async move { (i, api.reserve(&tokens[i], show, seat, &keys[i]).await) })
    }))
    .await;
    let mismatch =
        join_all((0..users).map(|i| api.reserve(&tokens[i], &show, other(i), &keys[i]))).await;
    let again =
        join_all((0..users).map(|i| api.reserve(&tokens[i], &show, own(i), &keys[i]))).await;

    let mut t_parallel = Tally::default();
    let mut t_mismatch = Tally::default();
    let mut t_again = Tally::default();
    parallel.iter().for_each(|(_, r)| t_parallel.add(r));
    mismatch.iter().for_each(|r| t_mismatch.add(r));
    again.iter().for_each(|r| t_again.add(r));
    t_parallel.print("same key in parallel");
    t_mismatch.print("same key, different seat");
    t_again.print("original request again");

    let mut ids_per_user: Vec<HashSet<&str>> = vec![HashSet::new(); users];
    for (i, reply) in &parallel {
        if let Some(id) = reply.reservation_id() {
            ids_per_user[*i].insert(id);
        }
    }
    let one_each = ids_per_user.iter().all(|ids| ids.len() == 1);
    let r = &mut ctx.report;
    r.check(
        NAME,
        "parallel copies all succeed",
        t_parallel.count("201") == (users * copies) as u64,
        format!("{} of {}", t_parallel.count("201"), users * copies),
    );
    r.check(
        NAME,
        "parallel copies share one reservation per user",
        one_each,
        "",
    );
    r.check(
        NAME,
        "same key, different seat -> 409 idempotency_mismatch",
        t_mismatch.count("409 idempotency_mismatch") == users as u64,
        format!(
            "{} of {users}",
            t_mismatch.count("409 idempotency_mismatch")
        ),
    );
    let same_again = (0..users).all(|i| {
        again[i].is(201) && ids_per_user[i].contains(again[i].reservation_id().unwrap_or(""))
    });
    r.check(
        NAME,
        "a later retry still returns the original",
        same_again,
        "",
    );
    let failures = t_parallel.failures() + t_mismatch.failures() + t_again.failures();
    r.check(
        NAME,
        "no 5xx or lost responses",
        failures == 0,
        format!("{failures}"),
    );

    let state = api.show_settled(&show).await?;
    let expected: std::collections::BTreeSet<String> = seats.iter().cloned().collect();
    r.check(
        NAME,
        "each user holds exactly their one seat",
        state.confirmed_seats == expected && state.counts_add_up(),
        format!("confirmed {} of {}", state.confirmed, state.total_seats),
    );

    // Show creation takes a key too: a retried create must not duplicate.
    let key = Uuid::new_v4().to_string();
    let spec = ShowSpec {
        name: create_name,
        rows: 1,
        seats_per_row: 5,
        per_user_limit: 4,
    };
    let creates = join_all((0..copies).map(|_| api.create_show(&admin, &spec, &key))).await;
    let show_ids: HashSet<&str> = creates
        .iter()
        .filter(|r| r.is(201))
        .filter_map(|r| r.show_id())
        .collect();
    r.check(
        NAME,
        "parallel creates with one key make one show",
        creates.iter().all(|c| c.is(201)) && show_ids.len() == 1,
        format!("{} distinct", show_ids.len()),
    );
    let created_show = show_ids.iter().next().map(|id| id.to_string());
    let changed = ShowSpec {
        seats_per_row: 6,
        ..spec
    };
    let reused = api.create_show(&admin, &changed, &key).await;
    r.check(
        NAME,
        "create with a reused key, different body -> 409",
        reused.is_conflict("idempotency_mismatch"),
        reused.label(),
    );

    if let Some(id) = created_show {
        ctx.shows.push((NAME, id));
    }
    let ok = t_parallel.count("201") + t_again.count("201");
    let mut all = t_parallel;
    for (label, n) in t_mismatch.counts.into_iter().chain(t_again.counts) {
        *all.counts.entry(label).or_default() += n;
    }
    Ok(Totals {
        created: users as u64,
        replayed: ok.saturating_sub(users as u64),
        ..Totals::default()
    }
    .with_declines(&all))
}
