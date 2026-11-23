-- Narrowing waits for a principal's lock (a lookup in flight), bounded: past the bound the COMMIT fails, so a
-- revocation is never committed un-narrowed.
create extension if not exists dblink;
select t.fixture_relations();
select t.beat();
set fence.tenant = 'acme';
select fence.project('acme', 'lk', array['q1'], 't');
update fence.config set value = '300' where key = 'narrow_lock_timeout_ms';
select dblink_connect('other', 'dbname=' || current_database() || ' user=' || current_user);
select dblink_exec('other', 'begin');
select * from dblink('other', $$ select fence.lock_principal('acme', 'lk')::text $$) as x(v text);
select t.raises($$ select fence.write('delete', 'container:acme/q1', 'member', 'user:acme/lk'); set constraints all immediate $$,
                '55P03', 'a held principal lock fails the narrowing');
select dblink_exec('other', 'rollback');
select t.eq(t.set_of('lk'), array['q1'], 'a failed narrowing removed nothing');
select t.eq((select count(*) from fence.outbox where resource = 'container:acme/q1')::int, 0, 'and the write did not commit');
select dblink_disconnect('other');

-- RLS: the generated policy fences a table, and refuses an owner without force.
do $$ begin if not exists (select 1 from pg_roles where rolname = 'fence_test_app') then create role fence_test_app; end if; end $$;
create table passage (id int primary key, container text);
insert into passage values (1, 'q1'), (2, 'q2');
select t.raises($$ select fence.enable_rls('passage', 'container') $$, 'FN422', 'owner refused without force');
select fence.enable_rls('passage', 'container', force => true);
grant usage on schema fence, t to fence_test_app;
grant select on all tables in schema fence to fence_test_app;
grant execute on all functions in schema fence to fence_test_app;
grant select on passage to fence_test_app;
set role fence_test_app;
set fence.principal = 'lk';
select t.eq((select array_agg(id) from passage), array[1], 'rls fences the scan');
set fence.principal = 'nobody';
select t.raises($$ select * from passage $$, 'FN404', 'rls keeps FN404');
reset role;
