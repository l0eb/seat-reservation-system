//! Every buyer wants the same seat at the same instant. Exactly one may win.

use std::collections::HashSet;

use anyhow::Result;
use futures::future::join_all;
use uuid::Uuid;

use super::Ctx;
use crate::api::ShowSpec;
use crate::report::{Tally, Totals};

const NAME: &str = "hot-seat";

pub async fn run(ctx: &mut Ctx, buyers: usize) -> Result<Totals> {
    println!("\n== hot seat: {buyers} buyers send for seat A1 at the same instant, then all retry");
    let api = ctx.api.clone();
    let spec = ShowSpec {
        name: ctx.show_name(NAME),
        rows: 1,
        seats_per_row: 10,
        per_user_limit: 4,
    };
    let show = api.new_show(&ctx.admin, &spec).await?;
    ctx.shows.push((NAME, show.clone()));
    let tokens = api.mint_users(&ctx.users(NAME), buyers).await?;
    let keys: Vec<String> = (0..buyers).map(|_| Uuid::new_v4().to_string()).collect();
    let seat = vec!["A1".to_string()];

    let wave = || {
        join_all(
            tokens
                .iter()
                .zip(&keys)
                .map(|(token, key)| api.reserve(token, &show, &seat, key)),
        )
    };
    let first = wave().await;
    // The same requests again, as if every client retried after a timeout.
    let retry = wave().await;

    let mut first_tally = Tally::default();
    let mut retry_tally = Tally::default();
    first.iter().for_each(|r| first_tally.add(r));
    retry.iter().for_each(|r| retry_tally.add(r));
    first_tally.print("first wave");
    retry_tally.print("retry wave");

    let winners: Vec<usize> = (0..buyers).filter(|&i| first[i].is(201)).collect();
    let r = &mut ctx.report;
    r.check(
        NAME,
        "exactly one buyer gets the seat",
        winners.len() == 1,
        format!("{} x 201", winners.len()),
    );
    r.check(
        NAME,
        "every other buyer gets 409 seat_taken",
        first_tally.count("409 seat_taken") == buyers as u64 - 1,
        format!("{} of {}", first_tally.count("409 seat_taken"), buyers - 1),
    );
    let replayed = winners.first().is_some_and(|&w| {
        retry[w].is(201) && retry[w].reservation_id() == first[w].reservation_id()
    });
    r.check(
        NAME,
        "the winner's retry returns the same reservation",
        replayed,
        "",
    );
    r.check(
        NAME,
        "losers' retries are still 409 seat_taken",
        retry_tally.count("409 seat_taken") == buyers as u64 - 1,
        format!("{} of {}", retry_tally.count("409 seat_taken"), buyers - 1),
    );
    let failures = first_tally.failures() + retry_tally.failures();
    r.check(
        NAME,
        "no 5xx or lost responses",
        failures == 0,
        format!("{failures}"),
    );

    let state = api.show_settled(&show).await?;
    r.check(
        NAME,
        "A1 confirmed, the other 9 seats untouched",
        state.confirmed == 1 && state.confirmed_seats.contains("A1") && state.counts_add_up(),
        format!("confirmed {} of {}", state.confirmed, state.total_seats),
    );

    let ids: HashSet<&str> = first
        .iter()
        .chain(&retry)
        .filter(|r| r.is(201))
        .filter_map(|r| r.reservation_id())
        .collect();
    let ok = first_tally.count("201") + retry_tally.count("201");
    let mut totals = Totals {
        created: ids.len() as u64,
        replayed: ok - ids.len() as u64,
        ..Totals::default()
    };
    let mut both = first_tally;
    for (label, n) in retry_tally.counts {
        *both.counts.entry(label).or_default() += n;
    }
    totals = totals.with_declines(&both);
    Ok(totals)
}
