-- A sign-in that is started but never finished leaves a row in
-- auth.flow_state, which can hold a short-lived token from Google.
-- Every hour, clear the rows that are more than a day old, so none
-- lasts much longer than a day.
create extension if not exists pg_cron with schema pg_catalog;

select cron.schedule(
  'clear-unfinished-sign-ins',
  '17 * * * *',
  $$delete from auth.flow_state where created_at < now() - interval '1 day'$$
);
