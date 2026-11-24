-- WATCH, APPLIED: a change made in SpiceDB without rebac.write (another service, the zed CLI) is applied from the
-- Watch stream: revocations narrowed at once under the commit's lock order, structure mirrored, everyone it touches
-- refreshed fully consistent, and the daemon's own relayed writes recognised and skipped.
create extension if not exists dblink;
select t.fixture_relations_with_sets();
select t.beat();
set rebac.tenant = 'acme';

create or replace function t.watch(op text, res text, rel text, sub text, tok text) returns integer
language sql as $$ select narrowed from rebac.apply_watch(array[op], array[res], array[rel], array[sub], tok) $$;

-- 1. A direct relation deleted outside rebac.write: narrowed now, refreshed fully consistent, position recorded.
select rebac.project('acme', 'ana', array['space-1', 'space-2'], 't', 'live', now());
select t.eq(t.watch('delete', 'container:acme/space-1', 'member', 'user:acme/ana', 'z1'), 1, 'one row narrowed');
select t.eq(t.set_of('ana'), array['space-2'], 'an external direct revocation is in force');
select t.eq((select zedtoken from rebac.refresh where principal = 'ana'), '', 'refreshed fully consistent');
select t.eq((select priority from rebac.refresh where principal = 'ana'), 0::smallint, 'at revocation priority');
select t.eq((select zedtoken from rebac.watch_state where scope = 'watch_applied'), 'z1', 'position recorded');
select t.eq((select external_writes from rebac.watch_state where scope = 'watch_applied'), 1::bigint, 'counted');
select t.ok((select origin = 'watch' and synced_at is not null and requeued_at is not null from rebac.outbox
             where resource = 'container:acme/space-1'), 'recorded as applied, never relayed, never re-sent');
select t.eq((select count(*) from rebac.claim_relay(100, 100, 100)), 0::bigint, 'the relay has nothing to send');

-- A later enqueue at a token cannot make that refresh less than fully consistent.
select rebac.enqueue_refresh('acme', 'ana', 1::smallint, 'older');
select t.eq((select zedtoken from rebac.refresh where principal = 'ana'), '', 'fully consistent sticks');

-- 2. OURS: a relayed write echoed on Watch at the token it was delivered at is skipped; so is one still in flight.
select rebac.project('acme', 'bo', array['space-3', 'space-4'], 't', 'live', now());
select rebac.write('touch', 'container:acme/space-5', 'member', 'user:acme/bo');
select t.eq(t.watch('touch', 'container:acme/space-5', 'member', 'user:acme/bo', 'z2'), 0, 'in flight: ours');
select rebac.record_delivery(array(select seq from rebac.outbox where origin = 'write'), 'z3', '{}', '{}');
select t.eq((select count(*) from rebac.apply_watch(array['touch'], array['container:acme/space-5'], array['member'],
             array['user:acme/bo'], 'z3') where external = 0), 1::bigint, 'delivered at this token: ours');
-- The same relationship deleted later by someone else is not ours, though rebac.write touched it.
select t.eq(t.watch('delete', 'container:acme/space-3', 'member', 'user:acme/bo', 'z4'), 1, 'external, narrowed');
select t.eq(t.set_of('bo'), array['space-4'], 'bo lost space-3');
-- Replayed (a reconnect from an older position): already applied, nothing changes.
select t.eq(t.watch('delete', 'container:acme/space-3', 'member', 'user:acme/bo', 'z4'), 0, 'a replay is a no-op');

-- 3. STRUCTURE IS MIRRORED, so a later revocation finds it: an external arrow touch, then an external admin delete.
select t.watch('touch', 'container:acme/w-doc', 'workspace', 'workspace:acme/w1', 'z5');
select t.ok(exists (select 1 from rebac.edge where child = 'w-doc' and parent = 'workspace:w1'), 'external edge mirrored');
select t.ok(exists (select 1 from rebac.refresh_resource where resource = 'container:acme/w-doc'),
            'an external grant is a refresh, not a write');
select rebac.project('acme', 'adm', array['w-doc', 'own'], 't', 'live', now());
select t.eq(t.watch('delete', 'workspace:acme/w1', 'admin', 'user:acme/adm', 'z6'), 1, 'via narrowed');
select t.eq(t.set_of('adm'), array['own'], 'a workspace admin removed outside rebac.write loses its containers');

-- An external grant to a subject set is mirrored, and an external membership removal then narrows through it.
select t.watch('touch', 'container:acme/eng', 'member', 'group:acme/eng#member', 'z7');
select t.ok(exists (select 1 from rebac.set_grant where container = 'eng' and subject_set = 'group:eng#member'),
            'external set grant mirrored');
select rebac.project('acme', 'dee', array['eng', 'own'], 't', 'live', now());
select rebac.project('acme', 'cy', array['eng'], 't', 'live', now());
select t.watch('delete', 'group:acme/eng', 'member', 'user:acme/dee', 'z8');
select t.eq(t.set_of('dee'), array['own'], 'an external membership removal narrows the member');
select t.eq(t.set_of('cy'), array['eng'], 'and nobody else');

-- 4. An arrow and a set grant removed outside rebac.write narrow every holder, and every holder is refreshed.
select rebac.write('touch', 'container:acme/file-1', 'parent', 'container:acme/space-9');
select rebac.project('acme', 'eve', array['space-9', 'file-1'], 't', 'live', now());
select rebac.project('acme', 'fay', array['file-1'], 't', 'live', now());
delete from rebac.refresh;
select t.watch('delete', 'container:acme/file-1', 'parent', 'container:acme/space-9', 'z9');
select t.eq(t.set_of('eve'), array['space-9'], 'detached file narrowed for a space reader');
select t.eq(t.set_of('fay'), '{}'::text[], 'and for everyone');
select t.eq((select count(*) from rebac.refresh where principal in ('eve', 'fay') and zedtoken = ''), 2::bigint,
            'every unnamed holder is refreshed fully consistent');
select t.ok(not exists (select 1 from rebac.edge where child = 'file-1'), 'the edge is removed');
select t.watch('delete', 'container:acme/eng', 'member', 'group:acme/eng#member', 'z10');
select t.eq(t.set_of('cy'), '{}'::text[], 'an external set-grant removal narrows every holder');

-- 5. Tenancy and scope: another tenant's ids map to nobody; relations outside the closure are ignored.
select rebac.project('acme', 'gus', array['space-1'], 't', 'live', now());
select t.eq(t.watch('delete', 'container:acme/space-1', 'member', 'user:globex/gus', 'z11'), 0, 'a foreign subject');
select t.eq(t.set_of('gus'), array['space-1'], 'is nobody here');
select t.eq((select external from rebac.apply_watch(array['delete'], array['container:acme/space-1'],
             array['owner'], array['user:acme/gus'], 'z12')), 0, 'a relation that does not grant view is not counted');

-- 6. apply => false (a shadow daemon) counts and changes nothing.
select rebac.project('acme', 'hal', array['space-1'], 't', 'live', now());
select t.eq((select external from rebac.apply_watch(array['delete'], array['container:acme/space-1'], array['member'],
             array['user:acme/hal'], 'z13', false)), 1, 'counted');
select t.eq(t.set_of('hal'), array['space-1'], 'not applied');
select t.eq((select zedtoken from rebac.watch_state where scope = 'watch_applied'), 'z12', 'the live position did not move');
select t.eq((select zedtoken from rebac.watch_state where scope = 'watch_shadow'), 'z13', 'shadow keeps its own');

-- 7. THE GAP: Watch restarted from head. Every set looked up before it fails closed until it is re-projected.
select rebac.project('acme', 'ivy', array['space-1'], 't', 'live', now() - interval '1 second');
insert into rebac.sweep_claim (id, last_at) values (true, now()) on conflict (id) do update set last_at = now();
select rebac.enqueue_refresh('acme', 'ivy', 2::smallint, 'old');
select rebac.watch_restarted('head');
select t.eq((select zedtoken from rebac.refresh where principal = 'ivy'), '', 'a refresh queued before the gap runs fully consistent');
select t.raises($$ select * from rebac.readable_set('ivy') $$, 'RB503', 'looked up before the gap: closed');
select t.ok((select last_at < now() - interval '1 day' from rebac.sweep_claim), 'the sweep is forced');
select rebac.project('acme', 'ivy', array['space-1'], 't', 'live', clock_timestamp());
select t.eq(t.set_of('ivy'), array['space-1'], 're-projected after it: open');

-- 8. THE LOCK ORDER: an external arrow removal waits for a refresh of the tenant in flight, then narrows its row.
select rebac.write('touch', 'container:acme/file-2', 'parent', 'container:acme/space-9');
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
select dblink_connect('a', t.conn());
select dblink_connect('b', t.conn());
create temp table b_pid as select * from dblink('b', 'select pg_backend_pid()') as x(pid int);
select dblink_exec('a', 'begin');
select * from dblink('a', $$ select rebac.lock_principal('acme', 'jo')::text $$) as x(v text);
select * from dblink('a', $$ select rebac.project('acme', 'jo', array['space-9', 'file-2'], 'tok')::text $$) as x(v text);
select dblink_send_query('b', $$ select narrowed from rebac.apply_watch(array['delete'], array['container:acme/file-2'],
  array['parent'], array['container:acme/space-9'], 'z14') $$);
select t.eq(t.wait_blocked_or_done('b', (select pid from b_pid)), 'blocked', 'the Watch narrowing waits for the refresh');
select dblink_exec('a', 'commit');
select * from dblink_get_result('b') as x(s text);
select * from dblink_get_result('b') as x(s text);
select t.eq(t.set_of('jo'), array['space-9'], 'the refresh that raced an external arrow revocation does not keep it');

-- A lock it cannot take in time fails the whole message: nothing narrowed, the position not advanced.
update rebac.config set value = '50' where key = 'narrow_lock_timeout_ms';
select rebac.write('touch', 'container:acme/file-3', 'parent', 'container:acme/space-9');
select rebac.project('acme', 'kai', array['file-3'], 't', 'live', now());
select dblink_exec('a', 'begin');
select * from dblink('a', $$ select rebac.lock_principal('acme', 'lu')::text $$) as x(v text);
select t.raises($$ select rebac.apply_watch(array['delete'], array['container:acme/file-3'], array['parent'],
                  array['container:acme/space-9'], 'z15') $$, '55P03', 'a held tenant fails the message');
select dblink_exec('a', 'rollback');
select t.eq(t.set_of('kai'), array['file-3'], 'nothing half-applied');
select t.eq((select zedtoken from rebac.watch_state where scope = 'watch_applied'), 'z14', 'the position stays');
