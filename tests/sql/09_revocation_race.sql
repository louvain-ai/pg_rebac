-- THE RACE THAT G2 MUST CLOSE: a refresh that has written its row but not committed, against a revocation that
-- narrows EVERY holder of a container (an arrow tuple removed, a grant to a subject set withdrawn). The refresh runs
-- in session `a`, the revocation in session `b`; neither is ordered by time. The test waits until `b` is either done
-- or blocked on a lock (polled from pg_locks, no clock), then lets `a` commit. Before the tenant lock, `b` finished
-- without seeing `a`'s uncommitted row, and `a` then committed the container back: this test failed on that code.
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

-- 1. An arrow tuple removed: file-1 leaves space-9 while dee's refresh (which still sees it in SpiceDB) is in flight.
select rebac.write('touch', 'container:acme/file-1', 'parent', 'container:acme/space-9');
select dblink_exec('a', 'begin');
select * from dblink('a', $$ select rebac.lock_principal('acme', 'dee')::text $$) as x(v text);
select * from dblink('a', $$ select rebac.project('acme', 'dee', array['space-9', 'file-1'], 'tok')::text $$) as x(v text);
select dblink_send_query('b', $$ do $d$ begin perform set_config('rebac.tenant', 'acme', true);
  perform rebac.write('delete', 'container:acme/file-1', 'parent', 'container:acme/space-9'); end $d$ $$);
select t.eq(t.wait_blocked_or_done('b', (select pid from b_pid)), 'blocked', 'the arrow revocation waits for the refresh');
select dblink_exec('a', 'commit');
select * from dblink_get_result('b') as x(s text);
select * from dblink_get_result('b') as x(s text);   -- drained: the connection is free again
select t.eq(t.set_of('dee'), array['space-9'], 'the refresh that raced an arrow revocation does not keep the file');

-- 2. A grant to a subject set withdrawn while a member's refresh is in flight.
select rebac.write('touch', 'container:acme/eng-space', 'member', 'group:acme/eng#member');
select dblink_exec('a', 'begin');
select * from dblink('a', $$ select rebac.lock_principal('acme', 'eve')::text $$) as x(v text);
select * from dblink('a', $$ select rebac.project('acme', 'eve', array['eng-space', 'own'], 'tok')::text $$) as x(v text);
select dblink_send_query('b', $$ do $d$ begin perform set_config('rebac.tenant', 'acme', true);
  perform rebac.write('delete', 'container:acme/eng-space', 'member', 'group:acme/eng#member'); end $d$ $$);
select t.eq(t.wait_blocked_or_done('b', (select pid from b_pid)), 'blocked', 'the set-grant revocation waits for the refresh');
select dblink_exec('a', 'commit');
select * from dblink_get_result('b') as x(s text);
select * from dblink_get_result('b') as x(s text);   -- drained: the connection is free again
select t.eq(t.set_of('eve'), array['own'], 'the refresh that raced a set-grant revocation does not keep the container');

-- 3. The other order: the revocation holds the tenant first, so a refresh that starts after it waits, and then sees the
-- revocation pending (rebac.pending_revoked) and leaves the container out.
select rebac.write('touch', 'container:acme/file-2', 'parent', 'container:acme/space-9');
select dblink_exec('b', 'begin');
select dblink_exec('b', $$ set local rebac.tenant = 'acme' $$);
select * from dblink('b', $$ select rebac.write('delete', 'container:acme/file-2', 'parent', 'container:acme/space-9')::text $$) as x(v text);
select * from dblink('b', $$ select rebac.narrow_pending()::text $$) as x(v text);   -- holds the tenant exclusively
create temp table a_pid as select * from dblink('a', 'select pg_backend_pid()') as x(pid int);
select dblink_send_query('a', $$ begin; select rebac.lock_principal('acme', 'fay');
  select rebac.project('acme', 'fay', array['space-9', 'file-2'], 'tok'); commit $$);
select t.eq(t.wait_blocked_or_done('a', (select pid from a_pid)), 'blocked', 'a refresh waits for an arrow revocation in flight');
select dblink_exec('b', 'commit');
select t.drain('a');
select t.eq(t.set_of('fay'), array['space-9'], 'a refresh after the revocation subtracts it');

-- 4. A direct revocation waits for that principal only, never for the whole tenant: a refresh of someone else in
-- flight does not hold it.
select rebac.project('acme', 'gus', array['g1'], 'tok');
select dblink_exec('a', 'begin');
select * from dblink('a', $$ select rebac.lock_principal('acme', 'hal')::text $$) as x(v text);
select rebac.write('delete', 'container:acme/g1', 'member', 'user:acme/gus');
select t.eq(t.set_of('gus'), '{}'::text[], 'a direct revocation is not held by another principal''s refresh');
select dblink_exec('a', 'rollback');

-- 5. Outside READ COMMITTED the commit-time delete reads an old snapshot, and would miss a refresh that committed
-- after it: such a revocation is refused, never committed un-narrowed.
select rebac.project('acme', 'ivy', array['i1'], 'tok');
begin isolation level repeatable read;
select t.raises($$ select rebac.write('delete', 'container:acme/i1', 'member', 'user:acme/ivy') $$, 'RB422',
                'a view revocation under REPEATABLE READ is refused');
rollback;
begin isolation level serializable;
select t.raises($$ select rebac.write('delete', 'container:acme/i1', 'parent', 'container:acme/x') $$, 'RB422',
                'an arrow revocation under SERIALIZABLE is refused');
rollback;
begin isolation level repeatable read;
select rebac.write('touch', 'container:acme/i2', 'member', 'user:acme/ivy');
commit;
select t.ok(true, 'a grant under REPEATABLE READ is allowed (it narrows nothing)');

-- 6. The relay re-narrows its claimed deletes and then holds its transaction across SpiceDB writes. It takes no tenant
-- lock, so an arrow revocation is never held up by a relay waiting on SpiceDB.
select rebac.project('acme', 'jo', array['j1'], 'tok');
select rebac.write('delete', 'container:acme/j1', 'member', 'user:acme/jo');
select rebac.write('touch', 'container:acme/file-3', 'parent', 'container:acme/space-9');
select dblink_exec('a', 'begin');
select * from dblink('a', format('select rebac.narrow_seqs(array[%s]::bigint[], false)::text',
  (select max(seq) from rebac.outbox where resource = 'container:acme/j1'))) as x(v text);
select dblink_send_query('b', $$ do $d$ begin perform set_config('rebac.tenant', 'acme', true);
  perform rebac.write('delete', 'container:acme/file-3', 'parent', 'container:acme/space-9'); end $d$ $$);
select t.eq(t.wait_blocked_or_done('b', (select pid from b_pid)), 'done', 'a relay mid-delivery does not hold an arrow revocation');
select * from dblink_get_result('b') as x(s text);
select * from dblink_get_result('b') as x(s text);
select dblink_exec('a', 'commit');

select dblink_disconnect('a');
select dblink_disconnect('b');
