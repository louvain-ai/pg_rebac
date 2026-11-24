-- Wildcards on the projected type (container#member@user:*): a grant to everyone, refreshed for everyone, and its
-- removal narrows every holder of the container (as an arrow's or a set grant's does).
select t.fixture_relations_with_sets();
select t.beat();
set rebac.tenant = 'acme';

-- Written through rebac.write: a wildcard carries no tenant prefix, and is accepted on a tenant's resource.
select rebac.write('touch', 'container:acme/handbook', 'member', 'user:*');
select t.raises($$ select rebac.write('touch', 'container:globex/x', 'member', 'user:*') $$, 'RB422',
                'the resource still carries the tenant');
select t.eq((select count(*) from rebac.principals_of('acme', 'user:*')), 0::bigint, 'a wildcard names nobody');

-- Delivered: every principal the tenant knows is refreshed (LookupSubjects would name none of them).
select rebac.project('acme', 'ana', array['own'], 't', 'live', now());
select rebac.project('acme', 'bo', '{}', 't', 'live', now());
insert into rebac.subject values ('acme', 'cy', 'user:acme/slack_U1', true);
delete from rebac.refresh;
select rebac.record_delivery(array(select seq from rebac.outbox), 'z1', '{}', '{}');
select t.eq((select array_agg(principal order by principal) from rebac.refresh), array['ana', 'bo', 'cy'],
            'a wildcard grant refreshes everyone');
select t.ok(exists (select 1 from rebac.refresh_resource where resource = 'container:acme/handbook'), 'and its resource');

-- The refreshes project it (as SpiceDB would answer).
select rebac.project('acme', 'ana', array['own', 'handbook'], 't', 'live', now());
select rebac.project('acme', 'bo', array['handbook'], 't', 'live', now());

-- Removed: in force at commit for every holder.
begin;
select rebac.write('delete', 'container:acme/handbook', 'member', 'user:*');
commit;
select t.eq(t.set_of('ana'), array['own'], 'a wildcard removed narrows every holder');
select t.eq(t.set_of('bo'), '{}'::text[], 'everyone');
-- Pending, it holds the container out of every refresh.
select rebac.project('acme', 'bo', array['handbook'], 't', 'live', now());
select t.eq(t.set_of('bo'), '{}'::text[], 'a pending wildcard removal is subtracted from a refresh');

-- Removed outside rebac.write, seen on Watch: the same.
update rebac.outbox set synced_at = now(), zedtoken = 'z2' where synced_at is null;
select rebac.project('acme', 'bo', array['wiki'], 't', 'live', now());
select rebac.apply_watch(array['touch'], array['container:acme/wiki'], array['member'], array['user:*'], 'z3');
select t.ok(exists (select 1 from rebac.refresh where principal = 'bo'), 'an external wildcard grant refreshes everyone');
select rebac.apply_watch(array['delete'], array['container:acme/wiki'], array['member'], array['user:*'], 'z4');
select t.eq(t.set_of('bo'), '{}'::text[], 'an external wildcard removal narrows every holder');
