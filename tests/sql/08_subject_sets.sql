-- Grants to a subject set (container#member@group:eng#member) and memberships of it (group:eng#member@user:ana).
select t.fixture_relations_with_sets();
select t.beat();
set rebac.tenant = 'acme';

select rebac.write('touch', 'container:acme/eng-space', 'member', 'group:acme/eng#member');
select rebac.write('touch', 'container:acme/file', 'parent', 'container:acme/eng-space');
select t.ok(exists (select 1 from rebac.set_grant where container = 'eng-space' and subject_set = 'group:eng#member'),
            'a set grant is mirrored');
select rebac.project('acme', 'ana', array['eng-space', 'file', 'mine'], 't');
select rebac.project('acme', 'bo', array['eng-space', 'file'], 't');

-- A member leaves the set: in force at commit, with the files under what the set was granted.
select t.raises($$ select rebac.write('delete', 'group:acme/eng#member', 'member', 'user:acme/ana') $$, 'RB422',
                'a resource never carries #relation');
select rebac.write('delete', 'group:acme/eng', 'member', 'user:acme/ana');
select t.eq(t.set_of('ana'), array['mine'], 'membership removed: the set''s containers go, children too');
select t.eq(t.set_of('bo'), array['eng-space', 'file'], 'other members keep it');

-- Pending, it cannot be regrown by a refresh.
select rebac.project('acme', 'ana', array['eng-space', 'file', 'mine'], 't');
select t.eq(t.set_of('ana'), array['mine'], 'a pending membership removal is subtracted');

-- The grant to the set is withdrawn: everyone holding the container loses it (their own grants come back on refresh).
select rebac.write('delete', 'container:acme/eng-space', 'member', 'group:acme/eng#member');
select t.eq(t.set_of('bo'), '{}'::text[], 'a set grant removed narrows every holder');
select t.ok(not exists (select 1 from rebac.set_grant where container = 'eng-space'), 'set grant removed');

-- Delivery routes: a membership refreshes its member; a set grant queues its container for LookupSubjects.
update rebac.outbox set synced_at = null;
select rebac.record_delivery(array(select seq from rebac.outbox order by seq), 'zed', '{}', '{}');
select t.ok(exists (select 1 from rebac.refresh where principal = 'ana'), 'membership refreshes the member');
select t.ok(exists (select 1 from rebac.refresh_resource where resource = 'container:acme/eng-space'), 'set grant queued');
