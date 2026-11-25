-- A SUBJECT SET GRANTED THROUGH A VIA RELATION (workspace#admin@group:ops#member): a member who leaves the set loses
-- the containers under the workspace at commit. Before 0.2.1 only grants on the projected type were mirrored, so the
-- membership's removal narrowed nothing here and the member kept them until a refresh.
select t.fixture_relations_with_sets();
select t.beat();
set rebac.tenant = 'acme';

select rebac.write('touch', 'container:acme/c9', 'workspace', 'workspace:acme/w1');
select rebac.write('touch', 'container:acme/c9-file', 'parent', 'container:acme/c9');
select rebac.write('touch', 'workspace:acme/w1', 'admin', 'group:acme/ops#member');
select t.ok(exists (select 1 from rebac.set_grant where container = 'workspace:w1' and subject_set = 'group:ops#member'),
            'a grant to a set through a via relation is mirrored, its object typed');
update rebac.outbox set synced_at = now(), zedtoken = 'z0' where synced_at is null;
select rebac.project('acme', 'vee', array['c9', 'c9-file', 'own'], 't');
select rebac.project('acme', 'wes', array['c9', 'c9-file'], 't');

-- The membership removed: in force at commit, the file under the container too.
select rebac.write('delete', 'group:acme/ops', 'member', 'user:acme/vee');
select t.eq(t.set_of('vee'), array['own'], 'leaving a set granted through a via relation narrows at commit');
select t.eq(t.set_of('wes'), array['c9', 'c9-file'], 'other members keep it');
-- Pending, a refresh (SpiceDB still answers the membership) cannot restore it.
select rebac.project('acme', 'vee', array['c9', 'c9-file', 'own'], 't');
select t.eq(t.set_of('vee'), array['own'], 'a pending removal of that membership is subtracted from a refresh');
update rebac.outbox set synced_at = now(), zedtoken = 'z1' where synced_at is null;

-- The via grant removed: every member loses it, and the mirror row goes.
select rebac.write('delete', 'workspace:acme/w1', 'admin', 'group:acme/ops#member');
select t.eq(t.set_of('wes'), '{}'::text[], 'the via grant to the set removed narrows its members at commit');
select t.ok(not exists (select 1 from rebac.set_grant where container = 'workspace:w1'), 'and leaves the mirror');

-- Delivered, a via grant to a set (touch or delete) names no member: each container under the workspace is queued for
-- a LookupSubjects, which does.
delete from rebac.refresh_resource;
select rebac.record_delivery(array(select max(seq) from rebac.outbox), 'z2', '{}', '{}');
select t.ok(exists (select 1 from rebac.refresh_resource where resource = 'container:acme/c9'),
            'a via grant to a set queues the containers it reaches');
