-- Bumped by every write to a seat, so the service's in-memory seat map can
-- apply updates that arrive out of order without resurrecting an old owner.
alter table seats add column version bigint not null default 0;
