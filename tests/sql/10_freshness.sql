-- Liveness and FRESHNESS are two bounds. The heartbeat says the daemon is up; it says nothing about how current a set
-- is. A principal's set is read only while its last SpiceDB lookup began within max_projection_age_ms, and a set of
-- unknown age is never read.
select t.fixture_relations();
select t.beat();
set rebac.tenant = 'acme';
select t.eq(rebac.setting('max_projection_age_ms'), '3600000', 'freshness is on by default, at one hour');

-- Fresh: projected now (the default `looked_up_at`).
select rebac.project('acme', 'ana', array['c1'], 'tok');
select t.eq(t.set_of('ana'), array['c1'], 'a set looked up now is read');

-- Old: the lookup behind it began two hours ago. The daemon is live, and the read still fails closed.
select rebac.project('acme', 'old', array['c1'], 'tok', 'live', now() - interval '2 hours');
select t.raises($$ select * from rebac.readable_set('old') $$, 'RB503', 'a set older than the bound is not read');
select t.raises($$ select rebac.readable_by('old', 'c1') $$, 'RB503', 'nor checked one by one');
select t.raises($$ select rebac.breadth('old') $$, 'RB503', 'nor measured');

-- Unknown: a state row written without a lookup time (an upgrade, or a writer other than rebac.project).
insert into rebac.principal_state (tenant, principal, state, n_containers, version) values ('acme', 'mir', 'projected', 1, 1);
insert into rebac.readable values ('acme', 'mir', 'c1');
select t.raises($$ select * from rebac.readable_set('mir') $$, 'RB503', 'a set of unknown age is not read');

-- Narrowing only removes, and never makes a set look fresher.
select rebac.write('delete', 'container:acme/c1', 'member', 'user:acme/old');
select t.raises($$ select * from rebac.readable_set('old') $$, 'RB503', 'narrowed, still old');

-- A refresh re-stamps it.
select rebac.project('acme', 'old', array['c2'], 'tok', 'live', clock_timestamp());
select t.eq(t.set_of('old'), array['c2'], 're-projected, read again');

-- A retired principal reads nothing, whatever its age.
select rebac.retire('acme', 'mir');
select t.eq(t.set_of('mir'), '{}'::text[], 'retired reads nothing, not RB503');

-- The two bounds are independent: a dead daemon fails a fresh set closed; the freshness bound off reads an old one.
update rebac.watch_state set heartbeat_at = now() - interval '1 minute';
select t.raises($$ select * from rebac.readable_set('ana') $$, 'RB503', 'liveness still fails closed on its own');
select t.beat();
update rebac.config set value = '0' where key = 'max_projection_age_ms';
select rebac.project('acme', 'old', array['c3'], 'tok', 'live', now() - interval '2 hours');
select t.eq(t.set_of('old'), array['c3'], 'freshness off: the age is not checked');
update rebac.config set value = '3600000' where key = 'max_projection_age_ms';

-- Reported: per tenant (the oldest lookup is when the whole tenant was last re-read) and in status().
select t.eq((select unfresh from rebac.freshness() where tenant = 'acme'), 1::bigint, 'freshness() counts the old set');
select t.ok((select oldest_lookup_at < now() - interval '1 hour' from rebac.freshness() where tenant = 'acme'),
            'freshness() reports the oldest lookup');
select t.eq((select unfresh_principals from rebac.status()), 1::bigint, 'status() counts it too');
select rebac.write('touch', 'container:acme/c9', 'member', 'user:acme/ana');
select t.ok((select relay_lag_ms >= 0 from rebac.status()), 'status() reports relay lag');
