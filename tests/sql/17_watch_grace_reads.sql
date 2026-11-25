-- Watch grace on the READ path: with Watch required and the stream not current for longer than watch_grace_ms, a
-- principal looked up before fails closed through rebac.readable_set, and one looked up since still reads.
select t.fixture_relations();
select t.beat();
set rebac.tenant = 'acme';
select rebac.watch_required(true);
update rebac.watch_state set current_at = now() - interval '1 hour' where scope = 'watch_applied';
select rebac.project('acme', 'old', array['c1'], 't', 'live', now() - interval '1 minute');
select rebac.project('acme', 'fresh', array['c2'], 't', 'live', clock_timestamp());
select t.raises($$select * from rebac.readable_set('old')$$, 'RB503', 'looked up before the stream dropped: closed');
select t.eq(t.set_of('fresh'), array['c2'], 'looked up since: still exact, still read');
-- Watch current again: everyone reads.
select rebac.watch_alive();
select t.eq(t.set_of('old'), array['c1'], 'Watch current: the old lookup reads');
-- watch = false: the grace does not apply (the sweep's freshness bound does).
update rebac.watch_state set current_at = now() - interval '1 hour' where scope = 'watch_applied';
select rebac.watch_required(false);
select t.eq(t.set_of('old'), array['c1'], 'Watch not required: no grace check');
