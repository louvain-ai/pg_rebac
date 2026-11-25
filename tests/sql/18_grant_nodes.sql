-- GRANT NODES (node_grants = on, the default): a grant to a subject set or a wildcard is stored once, under its node,
-- and every member reads it through rebac.holders_of. Every removal is in force at commit, as with per-principal rows.
select t.fixture_relations_with_sets();
insert into rebac.view_relation (resource_type, relation, kind) values ('workspace', 'admin', 'via')
  on conflict (resource_type, relation) do nothing;
select t.beat();
set rebac.tenant = 'acme';
select t.eq(rebac.setting('node_grants'), 'on', 'grant nodes are on by default');

-- A grant to a set, delivered: its NODE is refreshed, nobody else, and no LookupSubjects is queued.
select rebac.write('touch', 'container:acme/eng-space', 'member', 'group:acme/eng#member');
select rebac.write('touch', 'container:acme/file', 'parent', 'container:acme/eng-space');
select t.eq((select array_agg(x) from rebac.principals_of('acme', 'group:acme/eng#member') x),
            array['#group:eng#member'], 'a subject set stands for its node');
delete from rebac.refresh;
select rebac.record_delivery(array(select seq from rebac.outbox order by seq), 'z0', '{}', '{}');
select t.ok(exists (select 1 from rebac.refresh where principal = '#group:eng#member'), 'a set grant refreshes its node');
select t.ok(not exists (select 1 from rebac.refresh_resource where resource = 'container:acme/eng-space'),
            'and queues no LookupSubjects for its container');

-- The node is projected once; members record it and keep only what it does not hold.
select rebac.project('acme', '#group:eng#member', array['eng-space', 'file'], 't');
select rebac.project('acme', 'ana', array['eng-space', 'file', 'mine'], 't', 'live', now(), array['#group:eng#member']);
select rebac.project('acme', 'bo', array['eng-space', 'file'], 't', 'live', now(), array['#group:eng#member']);
select t.eq((select array_agg(container order by container) from rebac.readable where principal = 'ana'), array['mine'],
            'a member stores only its own grants');
select t.eq((select count(*) from rebac.readable where principal = 'bo')::int, 0, 'bo stores nothing of his own');
select t.eq(t.set_of('ana'), array['eng-space', 'file', 'mine'], 'ana reads the node''s containers and her own');
select t.eq(t.set_of('bo'), array['eng-space', 'file'], 'bo reads through the node alone (no RB404)');
select t.eq(rebac.breadth('ana'), 3, 'breadth sums the holders'' rows');
select t.ok(rebac.readable_by('bo', 'file'), 'readable_by reads through the node');
select t.eq((select array_agg(c order by c) from rebac.readable_set('ana', array['file', 'nope']) c), array['file'],
            'the narrowed set reads through the node');

-- Nobody reads as a node.
select t.raises($$ select * from rebac.readable_set('#group:eng#member') $$, 'RB400', 'a node name is refused');

-- FRESHNESS per node: the set is as old as its oldest lookup.
update rebac.principal_state set looked_up_at = now() - interval '2 hours' where principal = '#group:eng#member';
select t.raises($$ select * from rebac.readable_set('bo') $$, 'RB503', 'a stale node fails its members closed');
select t.eq((select array_agg(x) from rebac.stale_nodes('acme', 'bo') x), array['#group:eng#member'],
            'POST /lookup knows to re-project it');
update rebac.principal_state set looked_up_at = null where principal = '#group:eng#member';
select t.raises($$ select * from rebac.readable_set('bo') $$, 'RB503', 'a node of unknown age fails closed');
update rebac.principal_state set looked_up_at = now() where principal = '#group:eng#member';
insert into rebac.watch_state (scope, gap_at) values ('watch_applied', clock_timestamp())
  on conflict (scope) do update set gap_at = clock_timestamp();
update rebac.principal_state set looked_up_at = now() - interval '1 minute' where principal = '#group:eng#member';
select t.raises($$ select * from rebac.readable_set('ana') $$, 'RB503', 'a node looked up before the Watch gap fails closed');
update rebac.watch_state set gap_at = null where scope = 'watch_applied';
update rebac.principal_state set looked_up_at = now() where principal = '#group:eng#member';
-- Over its cap: unavailable, never truncated, and its members fail closed.
update rebac.config set value = '1' where key = 'max_containers_per_node';
select rebac.project('acme', '#group:eng#member', array['eng-space', 'file'], 't');
select t.raises($$ select * from rebac.readable_set('bo') $$, 'RB503', 'a node over its cap fails its members closed');
update rebac.config set value = '1000000' where key = 'max_containers_per_node';
select rebac.project('acme', '#group:eng#member', array['eng-space', 'file'], 't');
select t.eq(t.set_of('bo'), array['eng-space', 'file'], 'back once re-projected');

-- The version moves when the node changes.
create temp table v0 as select rebac.version('bo') as v;
select rebac.project('acme', '#group:eng#member', array['eng-space', 'file', 'wiki'], 't');
select t.ok(rebac.version('bo') <> (select v from v0), 'a member''s version changes with its node');
select rebac.project('acme', '#group:eng#member', array['eng-space', 'file'], 't');
select t.ok(exists (select 1 from rebac.refresh_resource where resource = 'container:acme/wiki'),
            'a node that shrank asks who still views what it lost');

-- A MEMBERSHIP removed: in force at commit, for that member only, and her version changes (a sum of the holders'
-- counters would not: hers goes up by one as the node's drops out).
create temp table v1 as select rebac.version('ana') as v;
select rebac.write('delete', 'group:acme/eng', 'member', 'user:acme/ana');
select t.ok(rebac.version('ana') <> (select v from v1), 'leaving a set changes the version');
select t.eq(t.set_of('ana'), array['mine'], 'membership removed: the node''s containers are gone for her');
select t.eq(t.set_of('bo'), array['eng-space', 'file'], 'other members keep them');
select t.ok(not exists (select 1 from rebac.node_member where principal = 'ana'), 'her membership row is gone');
-- Pending, a refresh (SpiceDB still answers the membership) cannot restore it.
select t.eq((select array_agg(x) from rebac.pending_left_nodes('acme', 'ana') x), array['#group:eng#member'],
            'the pending membership revocation is known');
select rebac.project('acme', 'ana', array['eng-space', 'file', 'mine'], 't', 'live', now(), array['#group:eng#member']);
select t.eq(t.set_of('ana'), array['mine'], 'a pending membership removal is not regrown');
update rebac.outbox set synced_at = now(), zedtoken = 'z1' where synced_at is null;

-- Rows from before the node (0.2's, or projected before the node was): a membership removal takes them too.
select rebac.project('acme', 'old', array['eng-space', 'file', 'own'], 't');   -- no memberships recorded
insert into rebac.node_member values ('acme', 'old', '#group:eng#member');
select rebac.write('delete', 'group:acme/eng', 'member', 'user:acme/old');
select t.eq(t.set_of('old'), array['own'], 'a member''s own copies of the node''s containers go with the membership');
update rebac.outbox set synced_at = now(), zedtoken = 'z2' where synced_at is null;

-- A GRANT TO THE SET removed: in force at commit for the node (and so for every member), children too.
select rebac.write('delete', 'container:acme/eng-space', 'member', 'group:acme/eng#member');
select t.eq(t.set_of('bo'), '{}'::text[], 'a set grant removed narrows its node at commit');
select t.eq((select count(*) from rebac.readable where principal = '#group:eng#member')::int, 0, 'no node rows left');
-- Pending, the node's own refresh cannot regrow it.
select rebac.project('acme', '#group:eng#member', array['eng-space', 'file'], 't');
select t.eq((select count(*) from rebac.readable where principal = '#group:eng#member')::int, 0,
            'a pending set-grant removal is subtracted from the node''s refresh');
update rebac.outbox set synced_at = now(), zedtoken = 'z3' where synced_at is null;
delete from rebac.refresh_resource;
select rebac.record_delivery(array(select max(seq) from rebac.outbox), 'z3', '{}', '{}');
select t.ok(exists (select 1 from rebac.refresh_resource where resource = 'container:acme/eng-space'),
            'its removal asks who still views the container (direct holders come back)');

-- An ARROW removed narrows every node holding the child.
select rebac.write('touch', 'container:acme/space-2', 'member', 'group:acme/eng#member');
select rebac.write('touch', 'container:acme/f2', 'parent', 'container:acme/space-2');
update rebac.outbox set synced_at = now(), zedtoken = 'z4' where synced_at is null;
select rebac.project('acme', '#group:eng#member', array['space-2', 'f2'], 't');
select t.eq(t.set_of('bo'), array['f2', 'space-2'], 'bo reads the new space through the node');
select rebac.write('delete', 'container:acme/f2', 'parent', 'container:acme/space-2');
select t.eq(t.set_of('bo'), array['space-2'], 'an arrow removed narrows the node''s rows at commit');
update rebac.outbox set synced_at = now(), zedtoken = 'z5' where synced_at is null;

-- A set granted through a VIA relation (workspace#admin@group:ops#member): its removal narrows the node too.
select rebac.write('touch', 'container:acme/c9', 'workspace', 'workspace:acme/w1');
select rebac.write('touch', 'workspace:acme/w1', 'admin', 'group:acme/ops#member');
update rebac.outbox set synced_at = now(), zedtoken = 'z6' where synced_at is null;
select rebac.project('acme', '#group:ops#member', array['c9'], 't');
select rebac.project('acme', 'cy', '{}', 't', 'live', now(), array['#group:ops#member']);
select t.eq(t.set_of('cy'), array['c9'], 'cy reads c9 as an ops member');
select t.ok(exists (select 1 from rebac.set_grant where container = 'workspace:w1' and subject_set = 'group:ops#member'),
            'a via grant to a set is mirrored, its object typed');
-- A member's own copy of it (projected before the node held it, or by 0.2) goes with the membership: the walk goes up
-- the member's rows, one hop through the arrow, to the mirrored via grant.
select rebac.project('acme', 'vee', array['c9'], 't');
select rebac.write('delete', 'group:acme/ops', 'member', 'user:acme/vee');
select t.eq(t.set_of('vee'), '{}'::text[], 'leaving a set granted through a via relation narrows at commit (0.2 did not)');
update rebac.outbox set synced_at = now(), zedtoken = 'z6b' where synced_at is null;
select rebac.write('delete', 'workspace:acme/w1', 'admin', 'group:acme/ops#member');
select t.eq(t.set_of('cy'), '{}'::text[], 'a via grant to a set removed narrows its node at commit');
select t.ok(not exists (select 1 from rebac.set_grant where container = 'workspace:w1'), 'and leaves the mirror');
update rebac.outbox set synced_at = now(), zedtoken = 'z7' where synced_at is null;

-- A WILDCARD: one node, every principal with a subject of its type reads through it.
select rebac.write('touch', 'container:acme/handbook', 'member', 'user:*');
select t.ok(exists (select 1 from rebac.set_grant where container = 'handbook' and subject_set = 'user:*'),
            'a wildcard grant is mirrored');
delete from rebac.refresh;
select rebac.record_delivery(array(select max(seq) from rebac.outbox), 'z8', '{}', '{}');
select t.eq((select array_agg(principal) from rebac.refresh), array['#user:*'],
            'a wildcard grant refreshes its node, not everyone');
-- The node's lookup runs as a stand-in subject: a grant made to the stand-in itself is dropped by the mirror.
select rebac.project('acme', '#user:*', array['handbook', 'granted-to-the-stand-in'], 't');
select t.eq((select array_agg(container) from rebac.readable where principal = '#user:*'), array['handbook'],
            'a wildcard node holds only what is granted to the wildcard');
select t.eq(t.set_of('bo'), array['handbook', 'space-2'], 'bo reads the wildcard''s container');
select t.eq(t.set_of('cy'), array['handbook'], 'so does cy, who reads nothing else');
-- Only a principal with a subject of the wildcard's type stands for it.
update rebac.config set value = 'false' where key = 'implicit_subject';
insert into rebac.subject values ('acme', 'svc', 'service:acme/s1', true);
select rebac.project('acme', 'svc', array['s-own'], 't', 'live', now(), '{}');
select t.eq(t.set_of('svc'), array['s-own'], 'a principal with no user subject does not read a user:* grant');
update rebac.config set value = 'true' where key = 'implicit_subject';
-- Removed: in force at commit for everyone, through the one node row.
select rebac.write('delete', 'container:acme/handbook', 'member', 'user:*');
select t.eq(t.set_of('cy'), '{}'::text[], 'a wildcard removed is gone for everyone at commit');
select t.ok(not exists (select 1 from rebac.set_grant where subject_set = 'user:*'), 'and from the mirror');
update rebac.outbox set synced_at = now(), zedtoken = 'z9' where synced_at is null;

-- Seen on WATCH (made outside rebac.write): the same narrowing.
select rebac.write('touch', 'container:acme/wiki', 'member', 'group:acme/eng#member');
update rebac.outbox set synced_at = now(), zedtoken = 'z10' where synced_at is null;
select rebac.project('acme', '#group:eng#member', array['space-2', 'wiki'], 't');
select rebac.project('acme', 'dan', '{}', 't', 'live', now(), array['#group:eng#member']);
select rebac.apply_watch(array['delete'], array['group:acme/eng'], array['member'], array['user:acme/dan'], 'z11');
select t.eq(t.set_of('dan'), '{}'::text[], 'a membership removed on Watch is in force');
select rebac.apply_watch(array['delete'], array['container:acme/wiki'], array['member'], array['group:acme/eng#member'], 'z12');
select t.eq(t.set_of('bo'), array['space-2'], 'a set grant removed on Watch narrows the node');
select t.ok(exists (select 1 from rebac.refresh where principal = '#group:eng#member' and zedtoken = ''),
            'and the node is re-projected fully consistent');

-- A resource's viewers include subject sets: their nodes are refreshed (LookupSubjects names them).
delete from rebac.refresh;
insert into rebac.refresh_resource (tenant, resource, generation) values ('acme', 'container:acme/space-2', 1)
  on conflict (tenant, resource) do update set generation = 1;
select rebac.complete_refresh_resource('acme', 'container:acme/space-2', 1,
  array['user:acme/bo', 'group:acme/eng#member'], 'z13');
select t.eq((select array_agg(principal order by principal collate "C") from rebac.refresh), array['#group:eng#member', 'bo'],
            'a resource refresh re-projects the nodes that view it');

-- The sweep re-projects every node the mirror names.
delete from rebac.refresh; delete from rebac.sweep_claim;
select * from rebac.sweep(1000, 0, false);
select t.ok(exists (select 1 from rebac.refresh where principal = '#group:eng#member'), 'the sweep re-projects nodes');

-- Retired: the membership rows go, and the principal reads nothing.
select rebac.retire('acme', 'bo');
select t.ok(not exists (select 1 from rebac.node_member where principal = 'bo'), 'a retired principal has no memberships');
select t.eq(t.set_of('bo'), '{}'::text[], 'and reads nothing');

-- THE PLAN: still one table, one index probe per candidate with the holders as an array.
create table doc (id int primary key, container text not null);
insert into doc select g, 'c' || (g % 500) from generate_series(1, 20000) g;
create index on doc (container);
select rebac.project('acme', '#group:big#member', array(select 'c' || g from generate_series(1, 300) g), 't');
select rebac.project('acme', 'eli', array['c1', 'zz'], 't', 'live', now(), array['#group:big#member']);
analyze;
create temp table plan_text (line text);
do $$
declare r record;
begin
  for r in execute 'explain (costs off) select count(*) from doc where container in (select * from rebac.readable_set(''eli''))' loop
    insert into plan_text values (r."QUERY PLAN");
  end loop;
end $$;
select t.ok(not exists (select 1 from plan_text where line like '%Function Scan%'), 'still inlined');
select t.ok(not exists (select 1 from plan_text where line like '%Append%'), 'no union: one scan of rebac.readable');
select t.ok(exists (select 1 from plan_text where line like '%ANY%'), 'the holders are an array');
select t.eq((select count(*) from doc where container in (select * from rebac.readable_set('eli')))::int, 12000,
            'fenced count through a node');

-- The kill switch: off, nobody reads through a node (narrower, never wider), and grants route as in 0.2.
update rebac.config set value = 'off' where key = 'node_grants';
select t.eq(t.set_of('eli'), array['zz'], 'with nodes off a member reads only its own rows (c1 was subtracted: the node held it)');
update rebac.config set value = 'on' where key = 'node_grants';
