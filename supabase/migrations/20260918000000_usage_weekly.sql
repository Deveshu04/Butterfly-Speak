-- Weekly dictated-word count per signed-in user, mirrored from the relay.
-- The relay's Durable Object is the counter of record; this row exists so
-- the app (and later the landing page) can show "n / 2000 words this week".
create table if not exists public.usage_weekly (
  user_id    uuid not null references auth.users (id) on delete cascade,
  week_start date not null,
  words      integer not null default 0 check (words >= 0),
  updated_at timestamptz not null default now(),
  primary key (user_id, week_start)
);

alter table public.usage_weekly enable row level security;

-- A signed-in user may read only their own rows. No insert/update/delete
-- policy exists for any user role: only the service role (which bypasses
-- RLS) writes, from the relay.
create policy usage_weekly_read_own
  on public.usage_weekly
  for select
  to authenticated
  using (user_id = (select auth.uid()));

revoke all on public.usage_weekly from anon;
grant select on public.usage_weekly to authenticated;
