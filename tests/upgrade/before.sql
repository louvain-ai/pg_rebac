-- 0.2.0: a group grant, a wildcard grant and a direct grant, expanded per principal.
select t.fixture_relations_with_sets();
select t.beat();
set rebac.tenant = 'acme';
select rebac.write('touch', 'container:acme/eng-space', 'member', 'group:acme/eng#member');
select rebac.write('touch', 'container:acme/file', 'parent', 'container:acme/eng-space');
select rebac.write('touch', 'container:acme/handbook', 'member', 'user:*');
select rebac.write('touch', 'container:acme/mine', 'member', 'user:acme/ana');
select rebac.write('touch', 'group:acme/eng', 'member', 'user:acme/ana');
select rebac.write('touch', 'group:acme/eng', 'member', 'user:acme/bo');
update rebac.outbox set synced_at = now(), zedtoken = 'z0';
select rebac.project('acme', 'ana', array['eng-space', 'file', 'handbook', 'mine'], 'z0', 'live', now());
select rebac.project('acme', 'bo', array['eng-space', 'file', 'handbook'], 'z0', 'live', now());
select rebac.project('acme', 'cy', array['handbook'], 'z0', 'live', now());
create table t.before as select p, t.set_of(p) as s from unnest(array['ana', 'bo', 'cy']) p;
-- A subject set granted through a via relation, expanded per member as 0.2 did.
insert into rebac.view_relation (resource_type, relation, kind, through) values ('workspace', 'admin', 'via', 'workspace')
  on conflict (resource_type, relation) do update set through = 'workspace';
select rebac.write('touch', 'container:acme/w-doc', 'workspace', 'workspace:acme/w1');
select rebac.write('touch', 'workspace:acme/w1', 'admin', 'group:acme/ops#member');
select rebac.write('touch', 'group:acme/ops', 'member', 'user:acme/dee');
update rebac.outbox set synced_at = now(), zedtoken = 'z0' where synced_at is null;
select rebac.project('acme', 'dee', array['w-doc'], 'z0', 'live', now());
insert into t.before values ('dee', array['w-doc']);
