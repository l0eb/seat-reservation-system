-- Seat reservation service — initial schema.
-- Order matters: seats.reservation_id references reservations(id).

create extension if not exists pgcrypto;

create table shows (
    id              uuid primary key default gen_random_uuid(),
    name            text not null,
    price_paise     bigint not null check (price_paise >= 0),
    per_user_limit  int not null default 4 check (per_user_limit > 0),
    total_seats     int not null check (total_seats > 0),
    created_at      timestamptz not null default now()
);

create table reservations (
    id              uuid primary key default gen_random_uuid(),
    show_id         uuid not null references shows(id),
    user_id         text not null,
    seats           text[] not null,
    amount_paise    bigint not null check (amount_paise >= 0),
    status          text not null check (status in ('confirmed', 'cancelled')),
    idempotency_key text not null,
    request_hash    text not null,
    created_at      timestamptz not null default now(),
    unique (user_id, idempotency_key)
);

-- No separate "held" phase: the reserve flow claims a seat straight to
-- "confirmed" in one transaction, so only two statuses exist in practice.
-- The column still allows the shape GET /shows/{id} reports (available /
-- held / confirmed) for forward compatibility with a future hold model.
create table seats (
    show_id         uuid not null references shows(id),
    label           text not null,
    status          text not null default 'available'
                        check (status in ('available', 'held', 'confirmed')),
    reservation_id  uuid references reservations(id),
    primary key (show_id, label),
    check (
        (status = 'available' and reservation_id is null)
        or (status <> 'available' and reservation_id is not null)
    )
);

create index seats_show_status_idx on seats (show_id, status);

-- Per-user seat count per show, used to enforce per_user_limit under
-- concurrency via a single conditional UPDATE.
create table user_show_holds (
    show_id   uuid not null references shows(id),
    user_id   text not null,
    held      int not null default 0 check (held >= 0),
    primary key (show_id, user_id)
);
