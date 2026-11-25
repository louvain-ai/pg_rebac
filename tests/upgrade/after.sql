-- Re-run twice: the same reads, before any refresh.
select t.beat();
set rebac.tenant = 'acme';
select t.eq(rebac.setting('node_grants'), 'on', 'node grants are on after the upgrade');
select t.eq(t.set_of(b.p), b.s, 'the same set after the upgrade: ' || b.p) from t.before b;
select t.ok((select last_at < now() - interval '1 day' from rebac.sweep_claim) is not false,
            'the upgrade forces a sweep, so every principal is re-projected with its memberships');
-- Revocations are in force at commit on 0.2's rows.
select rebac.write('delete', 'group:acme/eng', 'member', 'user:acme/ana');
select t.eq(t.set_of('ana'), array['handbook', 'mine'], 'a membership removed narrows 0.2''s rows at commit');
select rebac.write('delete', 'container:acme/handbook', 'member', 'user:*');
select t.eq(t.set_of('cy'), '{}'::text[], 'a wildcard removed narrows 0.2''s rows at commit');
select rebac.write('delete', 'container:acme/file', 'parent', 'container:acme/eng-space');
select t.eq(t.set_of('bo'), array['eng-space'], 'an arrow removed narrows 0.2''s rows at commit');
select rebac.write('delete', 'container:acme/eng-space', 'member', 'group:acme/eng#member');
select t.eq(t.set_of('bo'), '{}'::text[], 'a set grant removed narrows 0.2''s rows at commit');
-- A 0.2 daemon's six-argument project still resolves.
update rebac.outbox set synced_at = now(), zedtoken = 'z1' where synced_at is null;
select rebac.project('acme', 'cy', array['x'], 'z1', 'live', now());
select t.eq(t.set_of('cy'), array['x'], 'project with 0.2''s arguments');
-- 0.2 did not mirror a via grant to a set (nor a wildcard grant): the install asks rebacd to re-read every tenant's
-- relationships once. Removing the grant itself narrows every holder of what it reaches without the mirror.
select t.ok(exists (select 1 from rebac.config where key = 'remirror_pending'), 'the upgrade asks rebacd to re-mirror');
begin;
select rebac.write('delete', 'workspace:acme/w1', 'admin', 'group:acme/ops#member');
select rebac.narrow_pending();   -- what the commit would do, so the next check can roll it back
select t.eq(t.set_of('dee'), '{}'::text[], 'a via grant to a set removed narrows 0.2''s rows at commit');
rollback;
-- Once re-mirrored (the row rebacd's re-read writes), leaving the set narrows 0.2's rows at commit too.
insert into rebac.set_grant values ('acme', 'workspace:w1', 'admin', 'group:ops#member') on conflict do nothing;
select rebac.write('delete', 'group:acme/ops', 'member', 'user:acme/dee');
select t.eq(t.set_of('dee'), '{}'::text[], 'leaving a set granted through a via relation narrows 0.2''s rows at commit');
