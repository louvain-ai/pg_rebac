-- A refresh can never restore a container whose revocation is pending.
select t.fixture_relations();
select t.beat();
set fence.tenant = 'acme';
select fence.write('touch', 'container:acme/f1', 'parent', 'container:acme/s1');
select fence.write('touch', 'container:acme/w1c', 'workspace', 'workspace:acme/w1');
select fence.write('delete', 'container:acme/s1', 'member', 'user:acme/dee');
select fence.write('delete', 'workspace:acme/w1', 'admin', 'user:acme/dee');
-- The daemon's lookup, which still sees the grants (not yet delivered), returns them all.
select fence.project('acme', 'dee', array['s1', 'f1', 'w1c', 'keep'], 'tok');
select t.eq(t.set_of('dee'), array['keep'], 'pending revocations subtracted, with children and via');
-- Once delivered, they no longer subtract.
update fence.outbox set synced_at = now();
select fence.project('acme', 'dee', array['s1', 'keep'], 'tok');
select t.eq(t.set_of('dee'), array['keep', 's1'], 'delivered revocations no longer subtract');

-- Shadow writes never touch the live projection.
select fence.project('acme', 'dee', array['shadow-only'], 'tok', 'shadow');
select t.eq(t.set_of('dee'), array['keep', 's1'], 'shadow leaves live');
select t.eq((select array_agg(container) from fence.readable_shadow where principal = 'dee'), array['shadow-only'], 'shadow written');

-- A narrow hook runs inside the narrowing transaction.
create table hook_log (tenant text, containers text[]);
create function hook_fn(t text, c text[]) returns void language sql as $$ insert into hook_log values (t, c) $$;
insert into fence.narrow_hook values ('hook_fn(text, text[])');
select fence.write('delete', 'container:acme/keep', 'member', 'user:acme/dee');
select t.eq((select containers from hook_log), array['keep'], 'hook called with the lost containers');
