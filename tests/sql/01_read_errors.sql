-- The three answers: never asked (FN404), nothing (an empty set), unavailable (FN503). And no tenant is FN400.
select t.beat();

select t.raises($$ select * from fence.readable_set('alice') $$, 'FN400', 'a read with no tenant');

set fence.tenant = 'acme';
select t.raises($$ select * from fence.readable_set('alice') $$, 'FN404', 'a principal never asked');
select t.raises($$ select fence.readable_by('alice', 'x') $$, 'FN404', 'readable_by never asked');
select t.raises($$ select * from fence.readable_set('') $$, 'FN400', 'an empty principal');

-- Asked, and can read nothing: an empty set while the mark is fresh.
select fence.project('acme', 'alice', '{}', 'tok1');
select t.eq(t.set_of('alice'), '{}'::text[], 'a fresh empty mark reads as nothing');
-- A mark past its TTL is "never asked" again.
update fence.readable_empty set checked_at = now() - interval '2 minutes';
select t.raises($$ select * from fence.readable_set('alice') $$, 'FN404', 'an expired empty mark');

-- Projected.
select fence.project('acme', 'alice', array['c1', 'c2'], 'tok2');
select t.eq(t.set_of('alice'), array['c1', 'c2'], 'projected containers');
select t.ok(fence.readable_by('alice', 'c1'), 'readable_by true');
select t.ok(not fence.readable_by('alice', 'c9'), 'readable_by false');

-- Within narrows and never widens.
select t.eq((select array_agg(c order by c) from fence.readable_set('alice', array['c2', 'c9']) c),
            array['c2'], 'within intersects');

-- Unavailable is an error, never empty.
select fence.mark_unavailable('acme', 'bob', 'lookup_failed');
select t.raises($$ select * from fence.readable_set('bob') $$, 'FN503', 'an unavailable principal');

-- Stale: no heartbeat within the bound fails every read closed. Back-dated, not slept.
update fence.watch_state set heartbeat_at = clock_timestamp() - interval '6 seconds';
select t.raises($$ select * from fence.readable_set('alice') $$, 'FN503', 'stale past 5 s');
update fence.watch_state set heartbeat_at = clock_timestamp() - interval '4 seconds';
select t.eq(t.set_of('alice'), array['c1', 'c2'], 'fresh within 5 s');
delete from fence.watch_state;
select t.raises($$ select * from fence.readable_set('alice') $$, 'FN503', 'no heartbeat at all');
-- The bound can be turned off (a deployment with a static source).
update fence.config set value = '0' where key = 'max_staleness_ms';
select t.eq(t.set_of('alice'), array['c1', 'c2'], 'staleness off');
update fence.config set value = '5000' where key = 'max_staleness_ms';
select t.beat();

-- Retired reads nothing, and a projection cannot grow it back.
select fence.retire('acme', 'alice');
select t.eq(t.set_of('alice'), '{}'::text[], 'retired reads nothing');
select fence.project('acme', 'alice', array['c1'], 'tok3');
select t.eq(t.set_of('alice'), '{}'::text[], 'retired is not regrown by a refresh');
select t.eq((select count(*) from fence.readable where principal = 'alice')::int, 0, 'no rows for retired');

-- Over the cap: unavailable, never truncated.
update fence.config set value = '2' where key = 'max_containers_per_principal';
select fence.project('acme', 'carol', array['a', 'b', 'c'], 'tok4');
select t.raises($$ select * from fence.readable_set('carol') $$, 'FN503', 'over the cap');
select t.eq((select count(*) from fence.readable where principal = 'carol')::int, 0, 'over cap writes nothing');
