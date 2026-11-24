-- Test helpers: assertions that raise, so psql's ON_ERROR_STOP stops at the first failure.
create schema if not exists t;

create or replace function t.eq(actual anyelement, expected anyelement, what text) returns void
language plpgsql as $$
begin
  if actual is distinct from expected then
    raise exception 'FAIL %: expected %, got %', what, expected, actual;
  end if;
end $$;

create or replace function t.ok(cond boolean, what text) returns void
language plpgsql as $$
begin
  if cond is not true then raise exception 'FAIL %', what; end if;
end $$;

-- Run `stmt` and require it to fail with `state`.
create or replace function t.raises(stmt text, state text, what text) returns void
language plpgsql as $$
declare got text;
begin
  begin
    execute stmt;
  exception when others then
    got := sqlstate;
  end;
  if got is distinct from state then
    raise exception 'FAIL %: expected SQLSTATE %, got %', what, state, coalesce(got, 'no error');
  end if;
end $$;

create or replace function t.set_of(p text) returns text[]
language sql as $$ select coalesce(array_agg(c order by c), '{}') from rebac.readable_set(p) c $$;

-- The schema shape pg_rebac supports, as the daemon would derive it from:
--   definition workspace { relation admin: user }
--   definition container { relation workspace: workspace  relation parent: container
--     relation member: user  relation editor: user
--     permission view = member + editor + workspace->admin + parent->view }
create or replace function t.fixture_relations() returns void
language sql as $$
  delete from rebac.view_relation;
  insert into rebac.view_relation (resource_type, relation, kind, target_type, through) values
    ('container', 'member', 'direct', null, null),
    ('container', 'editor', 'direct', null, null),
    ('container', 'parent', 'arrow', 'container', null),
    ('container', 'workspace', 'arrow', 'workspace', null),
    ('workspace', 'admin', 'via', null, 'workspace');
$$;

-- A live heartbeat, so the staleness bound does not get in the way of tests about something else.
create or replace function t.beat() returns void
language sql as $$
  insert into rebac.watch_state (scope, heartbeat_at) values ('*', clock_timestamp())
  on conflict (scope) do update set heartbeat_at = clock_timestamp()
$$;

-- The same shape with subject sets: container#member also accepts group#member.
create or replace function t.fixture_relations_with_sets() returns void
language sql as $$
  select t.fixture_relations();
  insert into rebac.view_relation (resource_type, relation, kind) values ('group', 'member', 'set');
$$;

-- A dblink connection string back to this database as this role. A role that is not a superuser must authenticate
-- with a password (dblink refuses otherwise), so in owner mode run.sh supplies one as t.dblink_password, and the
-- connection goes to the address this session came in on, never loopback, which a container may trust without one.
create or replace function t.conn() returns text
language sql as $$
  select 'dbname=' || current_database() || ' user=' || current_user
         || case when coalesce(current_setting('t.dblink_password', true), '') = '' then ''
                 else ' host=' || host(inet_server_addr()) || ' port=' || inet_server_port()
                      || ' password=' || current_setting('t.dblink_password') end
$$;
