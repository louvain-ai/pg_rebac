-- Narrowing waits for a principal's lock (a lookup in flight), bounded: past the bound the COMMIT fails, so a
-- revocation is never committed un-narrowed.
create extension if not exists dblink;
select t.fixture_relations();
select t.beat();
set rebac.tenant = 'acme';
select rebac.project('acme', 'lk', array['q1'], 't');
update rebac.config set value = '300' where key = 'narrow_lock_timeout_ms';
select dblink_connect('other', t.conn());
select dblink_exec('other', 'begin');
select * from dblink('other', $$ select rebac.lock_principal('acme', 'lk')::text $$) as x(v text);
select t.raises($$ select rebac.write('delete', 'container:acme/q1', 'member', 'user:acme/lk'); set constraints all immediate $$,
                '55P03', 'a held principal lock fails the narrowing');
select dblink_exec('other', 'rollback');
select t.eq(t.set_of('lk'), array['q1'], 'a failed narrowing removed nothing');
select t.eq((select count(*) from rebac.outbox where resource = 'container:acme/q1')::int, 0, 'and the write did not commit');
select dblink_disconnect('other');

-- RLS: the generated policy fences a table, and refuses an owner without force.
do $$ begin if not exists (select 1 from pg_roles where rolname = 'rebac_test_app') then create role rebac_test_app; end if; end $$;
create table passage (id int primary key, container text);
insert into passage values (1, 'q1'), (2, 'q2');
select t.raises($$ select rebac.enable_rls('passage', 'container') $$, 'RB422', 'owner refused without force');
select rebac.enable_rls('passage', 'container', force => true);
grant usage on schema rebac, t to rebac_test_app;
grant select on all tables in schema rebac to rebac_test_app;
grant execute on all functions in schema rebac to rebac_test_app;
grant select on passage to rebac_test_app;
set role rebac_test_app;
set rebac.principal = 'lk';
select t.eq((select array_agg(id) from passage), array[1], 'rls fences the scan');
set rebac.principal = 'nobody';
select t.raises($$ select * from passage $$, 'RB404', 'rls keeps RB404');
reset role;
