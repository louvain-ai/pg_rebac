-- THE NODE RACES: a grant node's refresh, and a member's refresh, each in flight against the revocation that narrows
-- them, in both orders (as 09 does for principals). Session `a` refreshes, session `b` revokes.
create extension if not exists dblink;
select t.fixture_relations_with_sets();
select t.beat();
set rebac.tenant = 'acme';

create or replace function t.wait_blocked_or_done(conn text, who int) returns text
language plpgsql as $$
declare i int := 0;
begin
  loop
    if dblink_is_busy(conn) = 0 then return 'done'; end if;
    if exists (select 1 from pg_locks l where l.pid = who and not l.granted) then return 'blocked'; end if;
    i := i + 1;
    if i > 100000 then raise exception 'FAIL % neither finished nor blocked', conn; end if;
    perform pg_sleep(0.001);
  end loop;
end $$;

create or replace function t.drain(conn text) returns void
language plpgsql as $$
begin
  -- one result per statement sent, then an empty one; the results of a multi-statement send differ in shape
  while dblink_is_busy(conn) = 1 loop perform pg_sleep(0.001); end loop;
  loop
    begin
      perform * from dblink_get_result(conn, false) as x(s text);
      exit when not found;
    exception when datatype_mismatch then null;
    end;
  end loop;
end $$;

select dblink_connect('a', t.conn());
select dblink_connect('b', t.conn());
create temp table b_pid as select * from dblink('b', 'select pg_backend_pid()') as x(pid int);
create temp table a_pid as select * from dblink('a', 'select pg_backend_pid()') as x(pid int);

select rebac.write('touch', 'container:acme/eng-space', 'member', 'group:acme/eng#member');
select rebac.write('touch', 'container:acme/file', 'parent', 'container:acme/eng-space');
update rebac.outbox set synced_at = now(), zedtoken = 'z0' where synced_at is null;
select rebac.project('acme', '#group:eng#member', array['eng-space', 'file'], 't');
select rebac.project('acme', 'ana', array['eng-space', 'file'], 't', 'live', now(), array['#group:eng#member']);
select t.eq(t.set_of('ana'), array['eng-space', 'file'], 'ana reads through the node');

-- 1. The node's refresh in flight (it still sees the grant in SpiceDB), then the grant to the set is withdrawn.
select dblink_exec('a', 'begin');
select * from dblink('a', $$ select rebac.lock_principal('acme', '#group:eng#member')::text $$) as x(v text);
select * from dblink('a', $$ select rebac.project('acme', '#group:eng#member', array['eng-space', 'file'], 'tok')::text $$) as x(v text);
select dblink_send_query('b', $$ do $d$ begin perform set_config('rebac.tenant', 'acme', true);
  perform rebac.write('delete', 'container:acme/eng-space', 'member', 'group:acme/eng#member'); end $d$ $$);
select t.eq(t.wait_blocked_or_done('b', (select pid from b_pid)), 'blocked', 'the set-grant revocation waits for the node''s refresh');
select dblink_exec('a', 'commit');
select * from dblink_get_result('b') as x(s text);
select * from dblink_get_result('b') as x(s text);
select t.eq(t.set_of('ana'), '{}'::text[], 'the node refresh that raced a set-grant revocation does not keep it');

-- 2. The other order: the revocation first, the node's refresh waits, then subtracts it as pending.
update rebac.outbox set synced_at = now(), zedtoken = 'z1' where synced_at is null;
select rebac.write('touch', 'container:acme/space-2', 'member', 'group:acme/eng#member');
update rebac.outbox set synced_at = now(), zedtoken = 'z2' where synced_at is null;
select rebac.project('acme', '#group:eng#member', array['space-2'], 't');
select dblink_exec('b', 'begin');
select dblink_exec('b', $$ set local rebac.tenant = 'acme' $$);
select * from dblink('b', $$ select rebac.write('delete', 'container:acme/space-2', 'member', 'group:acme/eng#member')::text $$) as x(v text);
select * from dblink('b', $$ select rebac.narrow_pending()::text $$) as x(v text);
select dblink_send_query('a', $$ begin; select rebac.lock_principal('acme', '#group:eng#member');
  select rebac.project('acme', '#group:eng#member', array['space-2'], 'tok'); commit $$);
select t.eq(t.wait_blocked_or_done('a', (select pid from a_pid)), 'blocked', 'a node refresh waits for a set-grant revocation in flight');
select dblink_exec('b', 'commit');
select t.drain('a');
select t.eq(t.set_of('ana'), '{}'::text[], 'a node refresh after the revocation subtracts it');

-- 3. A member's refresh in flight (SpiceDB still answers the membership), then the membership is removed.
update rebac.outbox set synced_at = now(), zedtoken = 'z3' where synced_at is null;
select rebac.write('touch', 'container:acme/space-3', 'member', 'group:acme/eng#member');
update rebac.outbox set synced_at = now(), zedtoken = 'z4' where synced_at is null;
select rebac.project('acme', '#group:eng#member', array['space-3'], 't');
select dblink_exec('a', 'begin');
select * from dblink('a', $$ select rebac.lock_principal('acme', 'ana')::text $$) as x(v text);
select * from dblink('a', $$ select rebac.project('acme', 'ana', '{space-3}', 'tok', 'live', now(), '{#group:eng#member}')::text $$) as x(v text);
select dblink_send_query('b', $$ do $d$ begin perform set_config('rebac.tenant', 'acme', true);
  perform rebac.write('delete', 'group:acme/eng', 'member', 'user:acme/ana'); end $d$ $$);
select t.eq(t.wait_blocked_or_done('b', (select pid from b_pid)), 'blocked', 'a membership revocation waits for the member''s refresh');
select dblink_exec('a', 'commit');
select * from dblink_get_result('b') as x(s text);
select * from dblink_get_result('b') as x(s text);
select t.eq(t.set_of('ana'), '{}'::text[], 'the member refresh that raced a membership revocation does not keep it');

-- 4. The other order: the membership revocation first; the member's refresh waits, then leaves the node out.
update rebac.outbox set synced_at = now(), zedtoken = 'z5' where synced_at is null;
select rebac.project('acme', 'bo', '{space-3}', 't', 'live', now(), '{#group:eng#member}');
select dblink_exec('b', 'begin');
select dblink_exec('b', $$ set local rebac.tenant = 'acme' $$);
select * from dblink('b', $$ select rebac.write('delete', 'group:acme/eng', 'member', 'user:acme/bo')::text $$) as x(v text);
select * from dblink('b', $$ select rebac.narrow_pending()::text $$) as x(v text);
select dblink_send_query('a', $$ begin; select rebac.lock_principal('acme', 'bo');
  select rebac.project('acme', 'bo', '{space-3}', 'tok', 'live', now(), '{#group:eng#member}'); commit $$);
select t.eq(t.wait_blocked_or_done('a', (select pid from a_pid)), 'blocked', 'a member refresh waits for a membership revocation in flight');
select dblink_exec('b', 'commit');
select t.drain('a');
select t.eq(t.set_of('bo'), '{}'::text[], 'a member refresh after the membership revocation leaves the node out');

-- 5. A member's refresh in flight writes, as her own row, a container of a set whose node is not projected yet
-- (so nothing is subtracted), and the membership is removed: the removal waits for her lock, and only then reads her
-- rows, so the row the refresh commits goes too.
update rebac.outbox set synced_at = now(), zedtoken = 'z6' where synced_at is null;
select rebac.write('touch', 'container:acme/ops-space', 'member', 'group:acme/ops#member');
update rebac.outbox set synced_at = now(), zedtoken = 'z7' where synced_at is null;
select dblink_exec('a', 'begin');
select * from dblink('a', $$ select rebac.lock_principal('acme', 'cy')::text $$) as x(v text);
select * from dblink('a', $$ select rebac.project('acme', 'cy', '{ops-space}', 'tok', 'live', now(), '{#group:ops#member}')::text $$) as x(v text);
select dblink_send_query('b', $$ do $d$ begin perform set_config('rebac.tenant', 'acme', true);
  perform rebac.write('delete', 'group:acme/ops', 'member', 'user:acme/cy'); end $d$ $$);
select t.eq(t.wait_blocked_or_done('b', (select pid from b_pid)), 'blocked', 'a membership revocation waits for the member''s refresh');
select dblink_exec('a', 'commit');
select * from dblink_get_result('b') as x(s text);
select * from dblink_get_result('b') as x(s text);
select t.eq(t.set_of('cy'), '{}'::text[], 'the own row a racing refresh committed goes with the membership');

select dblink_disconnect('a');
select dblink_disconnect('b');
