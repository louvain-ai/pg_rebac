-- The three answers: never asked (RB404), nothing (an empty set), unavailable (RB503). And no tenant is RB400.
select t.beat();

select t.raises($$ select * from rebac.readable_set('alice') $$, 'RB400', 'a read with no tenant');

set rebac.tenant = 'acme';
select t.raises($$ select * from rebac.readable_set('alice') $$, 'RB404', 'a principal never asked');
select t.raises($$ select rebac.readable_by('alice', 'x') $$, 'RB404', 'readable_by never asked');
select t.raises($$ select * from rebac.readable_set('') $$, 'RB400', 'an empty principal');

-- Asked, and can read nothing: an empty set while the mark is fresh.
select rebac.project('acme', 'alice', '{}', 'tok1');
select t.eq(t.set_of('alice'), '{}'::text[], 'a fresh empty mark reads as nothing');
-- A mark past its TTL is "never asked" again.
update rebac.readable_empty set checked_at = now() - interval '2 minutes';
select t.raises($$ select * from rebac.readable_set('alice') $$, 'RB404', 'an expired empty mark');

-- Projected.
select rebac.project('acme', 'alice', array['c1', 'c2'], 'tok2');
select t.eq(t.set_of('alice'), array['c1', 'c2'], 'projected containers');
select t.ok(rebac.readable_by('alice', 'c1'), 'readable_by true');
select t.ok(not rebac.readable_by('alice', 'c9'), 'readable_by false');

-- Within narrows and never widens.
select t.eq((select array_agg(c order by c) from rebac.readable_set('alice', array['c2', 'c9']) c),
            array['c2'], 'within intersects');

-- Unavailable is an error, never empty.
select rebac.mark_unavailable('acme', 'bob', 'lookup_failed');
select t.raises($$ select * from rebac.readable_set('bob') $$, 'RB503', 'an unavailable principal');

-- Stale: no heartbeat within the bound fails every read closed. Back-dated, not slept.
update rebac.watch_state set heartbeat_at = clock_timestamp() - interval '6 seconds';
select t.raises($$ select * from rebac.readable_set('alice') $$, 'RB503', 'stale past 5 s');
update rebac.watch_state set heartbeat_at = clock_timestamp() - interval '4 seconds';
select t.eq(t.set_of('alice'), array['c1', 'c2'], 'fresh within 5 s');
delete from rebac.watch_state;
select t.raises($$ select * from rebac.readable_set('alice') $$, 'RB503', 'no heartbeat at all');
-- The bound can be turned off (a deployment with a static source).
update rebac.config set value = '0' where key = 'max_staleness_ms';
select t.eq(t.set_of('alice'), array['c1', 'c2'], 'staleness off');
update rebac.config set value = '5000' where key = 'max_staleness_ms';
select t.beat();

-- Retired reads nothing, and a projection cannot grow it back.
select rebac.retire('acme', 'alice');
select t.eq(t.set_of('alice'), '{}'::text[], 'retired reads nothing');
select rebac.project('acme', 'alice', array['c1'], 'tok3');
select t.eq(t.set_of('alice'), '{}'::text[], 'retired is not regrown by a refresh');
select t.eq((select count(*) from rebac.readable where principal = 'alice')::int, 0, 'no rows for retired');

-- Over the cap: unavailable, never truncated.
update rebac.config set value = '2' where key = 'max_containers_per_principal';
select rebac.project('acme', 'carol', array['a', 'b', 'c'], 'tok4');
select t.raises($$ select * from rebac.readable_set('carol') $$, 'RB503', 'over the cap');
select t.eq((select count(*) from rebac.readable where principal = 'carol')::int, 0, 'over cap writes nothing');
