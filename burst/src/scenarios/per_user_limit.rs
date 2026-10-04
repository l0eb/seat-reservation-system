//! Each user fires more single-seat bookings at once than the show allows.
//! However they interleave, each user must end with exactly the limit.

use anyhow::Result;
use futures::future::join_all;
use uuid::Uuid;

use super::Ctx;
use crate::api::ShowSpec;
use crate::report::{Tally, Totals};

const NAME: &str = "per-user-limit";
const LIMIT: usize = 4;
const ATTEMPTS: usize = 10;

pub async fn run(ctx: &mut Ctx, users: usize) -> Result<Totals> {
    println!("\n== per-user limit: {users} users each send {ATTEMPTS} bookings at once on a limit-{LIMIT} show");
    let api = ctx.api.clone();
    let spec = ShowSpec {
        name: ctx.show_name(NAME),
        rows: 1,
        seats_per_row: (users * ATTEMPTS) as u32,
        per_user_limit: LIMIT as u32,
    };
    let show = api.new_show(&ctx.admin, &spec).await?;
    ctx.shows.push((NAME, show.clone()));
    let tokens = api.mint_users(&ctx.users(NAME), users).await?;

    // User i asks for its own seats A{10i+1}..A{10i+10}, so only the limit
    // (never another buyer) can turn a request down.
    let replies = join_all((0..users).flat_map(|i| {
        let (api, show, token) = (&api, &show, &tokens[i]);
        (0..ATTEMPTS).map(move |a| async move {
            let seat = vec![format!("A{}", i * ATTEMPTS + a + 1)];
            (
                i,
                api.reserve(token, show, &seat, &Uuid::new_v4().to_string())
                    .await,
            )
        })
    }))
    .await;

    let mut tally = Tally::default();
    let mut won = vec![0usize; users];
    for (i, reply) in &replies {
        tally.add(reply);
        if reply.is(201) {
            won[*i] += 1;
        }
    }
    tally.print("all requests");

    let exact = won.iter().filter(|&&n| n == LIMIT).count();
    let over = won.iter().filter(|&&n| n > LIMIT).count();
    let r = &mut ctx.report;
    r.check(
        NAME,
        "no user gets more than the limit",
        over == 0,
        format!("{over} users over"),
    );
    r.check(
        NAME,
        "every user gets exactly the limit",
        exact == users,
        format!("{exact} of {users}"),
    );
    r.check(
        NAME,
        "the rest are 409 per_user_limit",
        tally.count("409 per_user_limit") == (users * (ATTEMPTS - LIMIT)) as u64,
        format!("{}", tally.count("409 per_user_limit")),
    );
    r.check(
        NAME,
        "no 5xx or lost responses",
        tally.failures() == 0,
        format!("{}", tally.failures()),
    );
    let state = api.show_settled(&show).await?;
    r.check(
        NAME,
        "confirmed seats == users x limit",
        state.confirmed == (users * LIMIT) as i64 && state.counts_add_up(),
        format!("confirmed {} of {}", state.confirmed, state.total_seats),
    );

    Ok(Totals {
        created: tally.count("201"),
        ..Totals::default()
    }
    .with_declines(&tally))
}
