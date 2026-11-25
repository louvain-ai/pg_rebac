-- WATCH GRACE, per principal (rebac.watch_ok): with Watch required, a set is read while the later of its own lookup and
-- the last time the caught-up stream was current is within watch_grace_ms. A dropped stream fails closed only the
-- principals not looked up since, and only after the grace; the gap still fails everyone looked up before it.
create or replace function t.ok_at(looked_up interval) returns boolean
language plpgsql as $$
begin
  perform rebac.watch_ok('acme', case when looked_up is null then null else now() - looked_up end);
  return true;
exception when sqlstate 'RB503' then return false;
end $$;

-- No Watch row: nothing to check.
select t.ok(t.ok_at('1 hour'), 'no Watch state, no Watch check');

-- Watch on (a live daemon said so), never current: only a fresh lookup reads.
select rebac.watch_required(true);
select t.ok(t.ok_at('1 second'), 'a lookup within the grace is exact on its own');
select t.ok(not t.ok_at('10 seconds'), 'an older lookup with Watch never current fails closed');
select t.ok(not t.ok_at(null), 'a lookup of unknown age fails closed');

-- Caught up now: every set reads, however old its lookup (G7's freshness bound is separate).
select rebac.watch_alive();
select t.ok(t.ok_at('10 minutes'), 'Watch current: an old lookup reads');

-- The stream dropped 10 s ago: sets looked up since still read; the rest fail closed past the 5 s grace.
update rebac.watch_state set current_at = now() - interval '10 seconds' where scope = 'watch_applied';
select t.ok(t.ok_at('2 seconds'), 'looked up after the drop: still exact');
select t.ok(not t.ok_at('20 seconds'), 'looked up before the drop, past the grace: RB503');
select t.raises($$select rebac.watch_ok('acme', now() - interval '20 seconds')$$, 'RB503', 'raised as RB503');

-- Within the grace after a drop, everyone still reads.
update rebac.watch_state set current_at = now() - interval '3 seconds' where scope = 'watch_applied';
select t.ok(t.ok_at('20 seconds'), 'within the grace of the drop');

-- watch_alive never moves current_at backwards, nor into the future.
select rebac.watch_alive(now() - interval '1 hour');
select t.ok((select current_at > now() - interval '4 seconds' from rebac.watch_state where scope = 'watch_applied'),
            'an older catch-up time does not move current_at back');
select rebac.watch_alive(now() + interval '1 hour');
select t.ok((select current_at <= now() from rebac.watch_state where scope = 'watch_applied'), 'nor into the future');

-- The grace off, or Watch not required: no grace check.
update rebac.watch_state set current_at = now() - interval '1 hour' where scope = 'watch_applied';
update rebac.config set value = '0' where key = 'watch_grace_ms';
select t.ok(t.ok_at('20 seconds'), 'watch_grace_ms 0 turns the check off');
update rebac.config set value = '5000' where key = 'watch_grace_ms';
select rebac.watch_required(false);
select t.ok(t.ok_at('20 seconds'), 'watch = false: the sweep bound (G7) applies instead');

-- THE GAP fails every set looked up before it, required or not, until re-projected.
update rebac.watch_state set gap_at = now() - interval '1 second' where scope = 'watch_applied';
select t.ok(not t.ok_at('5 seconds'), 'looked up before the gap');
select t.ok(t.ok_at('0 seconds'), 'looked up after the gap');

-- Through the read path (rebac.checked calls watch_ok with the oldest lookup behind the set).
select t.beat();
set rebac.tenant = 'acme';
select rebac.watch_required(true);
update rebac.watch_state set current_at = now() - interval '1 hour' where scope = 'watch_applied';
select rebac.project('acme', 'old', array['c1'], 't', 'live', now() - interval '1 minute');
select t.raises($$select * from rebac.readable_set('old')$$, 'RB503', 'a stale-Watch principal fails closed through readable_set');
select rebac.project('acme', 'new', array['c1'], 't', 'live', now());
select t.eq(t.set_of('new'), array['c1'], 'a principal looked up since still reads');
-- A grant node looked up before the drop fails its members closed, though their own lookup is recent.
select rebac.project('acme', '#group:eng#member', array['c2'], 't', 'live', now() - interval '1 minute');
select rebac.project('acme', 'mem', '{}', 't', 'live', now(), array['#group:eng#member']);
select t.raises($$select * from rebac.readable_set('mem')$$, 'RB503', 'the oldest lookup behind the set decides, a node''s included');
