-- POST /shows takes an Idempotency-Key, scoped to the admin who sent it,
-- so a retried create returns the original show instead of a duplicate.
-- Shows created before this have no key; unique ignores nulls.
alter table shows
    add column created_by      text,
    add column idempotency_key text,
    add column request_hash    text,
    add unique (created_by, idempotency_key);
