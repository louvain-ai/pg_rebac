-- WRITE AMPLIFICATION OF A REFRESH: a large tenant whose memberships change often. Each principal reads `width`
-- containers; each round re-projects `per_round` principals with `churn` of their containers swapped for others, as a
-- refresh after a membership change would. Reports, per round: time, WAL bytes, and rows inserted and deleted in
-- rebac.readable. Run in a fresh database with sql/rebac.sql installed:
--   psql -v principals=1000 -v width=5000 -v per_round=200 -v churn=0.01 -v rounds=5 -f churn.sql
\set ON_ERROR_STOP 1
update rebac.config set value = '0' where key in ('max_staleness_ms', 'max_projection_age_ms');
update rebac.config set value = '1000000' where key = 'max_containers_per_principal';
select set_config('bench.principals', :'principals', false), set_config('bench.width', :'width', false),
       set_config('bench.per_round', :'per_round', false), set_config('bench.churn', :'churn', false),
       set_config('bench.rounds', :'rounds', false);

-- The tenant: principal i reads a window of `width` containers out of 4 * width, so neighbours overlap.
do $$
declare n int := current_setting('bench.principals')::int; w int := current_setting('bench.width')::int;
begin
  for i in 1..n loop
    perform rebac.project('big', 'p' || i,
      array(select 'c' || ((i * 37 + k) % (4 * w)) from generate_series(1, w) k), 'tok');
  end loop;
end $$;
vacuum analyze rebac.readable;
checkpoint;

create temp table round_result (round int, ms numeric, wal_bytes numeric, ins bigint, del bigint);
do $$
declare
  n int := current_setting('bench.principals')::int; w int := current_setting('bench.width')::int;
  per int := current_setting('bench.per_round')::int; churn float := current_setting('bench.churn')::float;
  rounds int := current_setting('bench.rounds')::int;
  t0 timestamptz; l0 pg_lsn; i0 bigint; d0 bigint; i1 bigint; d1 bigint; p int; cur text[]; keep int;
begin
  for r in 1..rounds loop
    select n_tup_ins, n_tup_del into i0, d0 from pg_stat_xact_user_tables where relid = 'rebac.readable'::regclass;
    t0 := clock_timestamp(); l0 := pg_current_wal_insert_lsn();
    for j in 1..per loop
      p := 1 + ((r * per + j) * 7919) % n;
      select array_agg(container) into cur from rebac.readable where tenant = 'big' and principal = 'p' || p;
      keep := cardinality(cur) - greatest(1, (cardinality(cur) * churn)::int);
      -- drop `churn` of them, add as many new ones
      perform rebac.project('big', 'p' || p,
        cur[1:keep] || array(select 'n' || r || '-' || j || '-' || k from generate_series(1, cardinality(cur) - keep) k),
        'tok');
    end loop;
    select n_tup_ins, n_tup_del into i1, d1 from pg_stat_xact_user_tables where relid = 'rebac.readable'::regclass;
    insert into round_result values (r, extract(epoch from clock_timestamp() - t0) * 1000,
      pg_wal_lsn_diff(pg_current_wal_insert_lsn(), l0), i1 - i0, d1 - d0);
  end loop;
end $$;
select round, round(ms) as ms, pg_size_pretty(wal_bytes) as wal, ins as rows_inserted, del as rows_deleted
from round_result order by round;
select round(avg(ms)) as avg_ms, pg_size_pretty(avg(wal_bytes)) as avg_wal, round(avg(ins)) as avg_ins,
       round(avg(del)) as avg_del from round_result;
