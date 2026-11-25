-- The tenant every layout is built from: P principals, 100,000 containers, UUID-length ids (as the design's estimate).
--   * teams of 20, each granted 5 containers;        * departments of 500, each granted 100 containers;
--   * one all-company group granted 1,000 (1%);     * 20 direct shares per principal, spread at random;
--   * one principal in 10,000 is also in a "broad" group granted 20,000 containers (20%).
-- A typical reader reads about 1,125 containers, a broad one about 21,000. Run with -v principals=N.
create extension if not exists vector;
create schema if not exists bench;
create or replace function bench.pid(i int) returns text language sql immutable as $$ select md5('p' || i)::uuid::text $$;
create or replace function bench.cid(j int) returns text language sql immutable as $$ select md5('c' || j)::uuid::text $$;
create or replace function bench.team(t int) returns text language sql immutable as
  $$ select 'group:' || md5('team' || t)::uuid::text || '#member' $$;
create or replace function bench.dept(d int) returns text language sql immutable as
  $$ select 'group:' || md5('dept' || d)::uuid::text || '#member' $$;
-- Each grant node and the containers it is granted.
create or replace function bench.node_containers(n_principals int)
returns table (node text, container int) language sql immutable as $$
  select bench.team(t), (t * 5 + k) % 30000 from generate_series(0, (n_principals - 1) / 20) t, generate_series(0, 4) k
  union all
  select bench.dept(d), 30000 + (d * 100 + k) % 20000 from generate_series(0, (n_principals - 1) / 500) d, generate_series(0, 99) k
  union all
  select 'group:all#member', 50000 + k from generate_series(0, 999) k
  union all
  select 'group:broad#member', 51000 + k from generate_series(0, 19999) k
$$;
-- A principal's memberships, and its direct shares.
create or replace function bench.nodes_of(i int) returns setof text language sql immutable as $$
  select bench.team(i / 20) union all select bench.dept(i / 500) union all select 'group:all#member'
  union all select 'group:broad#member' where i % 10000 = 7
$$;
create or replace function bench.direct_of(i int) returns setof int language sql immutable as $$
  select distinct abs(hashint4(i * 31 + k)) % 100000 from generate_series(0, 19) k
$$;
