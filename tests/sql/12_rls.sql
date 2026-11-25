-- RLS: the fence is RESTRICTIVE, so no other policy widens it; and where a role that is not trusted can query, the
-- principal is bound by a trusted role (rebac.set_principal) rather than read from a setting anyone can change.
select t.fixture_relations();
select t.beat();
set rebac.tenant = 'acme';
select rebac.project('acme', 'lk', array['q1'], 't');
select rebac.project('acme', 'ceo', array['q1', 'q2', 'q3'], 't');

do $$ begin
  if not exists (select 1 from pg_roles where rolname = 'rebac_t_app') then create role rebac_t_app; end if;
  if not exists (select 1 from pg_roles where rolname = 'rebac_t_trusted') then create role rebac_t_trusted; end if;
  if not exists (select 1 from pg_roles where rolname = 'rebac_t_untrusted') then create role rebac_t_untrusted; end if;
end $$;
grant usage on schema t to rebac_t_app, rebac_t_trusted, rebac_t_untrusted;
grant execute on all functions in schema t to rebac_t_app, rebac_t_trusted, rebac_t_untrusted;

-- 1. An application's own permissive policy (here: everything) does not widen the fence.
create table doc (id int primary key, container text);
insert into doc values (1, 'q1'), (2, 'q2'), (3, 'q3');
alter table doc enable row level security;
create policy app_all on doc for select using (true);
select rebac.enable_rls('doc', 'container', force => true);
select t.ok((select not polpermissive from pg_policy where polrelid = 'doc'::regclass and polname = 'rebac_read'),
            'the fence is restrictive');
select t.ok(not exists (select 1 from pg_policy where polrelid = 'doc'::regclass and polname = 'rebac_base'),
            'with a permissive policy of its own, no base policy is added');
grant usage on schema rebac to rebac_t_app;
grant select on all tables in schema rebac to rebac_t_app;
grant execute on all functions in schema rebac to rebac_t_app;
grant select on doc to rebac_t_app;
set role rebac_t_app;
set rebac.principal = 'lk';
select t.eq((select array_agg(id order by id) from doc), array[1], 'a permissive using (true) does not widen it');
reset role;

-- 2. A table with no policy of its own gets a permissive base, or restrictive alone would return nothing.
create table note (id int primary key, container text);
insert into note values (1, 'q1'), (2, 'q2');
select rebac.enable_rls('note', 'container', force => true);
select t.ok(exists (select 1 from pg_policy where polrelid = 'note'::regclass and polname = 'rebac_base'),
            'a base policy is added');
grant select on note to rebac_t_app;
set role rebac_t_app;
set rebac.principal = 'lk';
select t.eq((select array_agg(id order by id) from note), array[1], 'restrictive with the base fences, not empties');
reset role;

-- 3. principal_source => 'session': an untrusted role cannot choose the principal.
create table secret (id int primary key, container text);
insert into secret values (1, 'q1'), (2, 'q2'), (3, 'q3');
select rebac.enable_rls('secret', 'container', force => true, principal_source => 'session');
grant usage on schema rebac to rebac_t_trusted, rebac_t_untrusted;
grant execute on function rebac.session_readable_set() to rebac_t_trusted, rebac_t_untrusted;
grant execute on function rebac.set_principal(text, text) to rebac_t_trusted;
grant select on secret to rebac_t_trusted, rebac_t_untrusted;
reset rebac.tenant;

set role rebac_t_untrusted;
select t.raises($$ select rebac.set_principal('acme', 'ceo') $$, '42501', 'untrusted cannot bind a principal');
set rebac.principal = 'ceo';
set rebac.tenant = 'acme';
select t.raises($$ select * from secret $$, 'RB400', 'the setting is ignored: nothing is bound');
select t.raises($$ select * from rebac.readable $$, '42501', 'nor can it read the projection');
select t.raises($$ select * from rebac.readable_set('ceo') $$, '42501', 'nor anyone''s set');
select t.raises($$ insert into rebac.session_principal values (pg_backend_pid(), pg_current_xact_id(), 'acme', 'ceo') $$,
                '42501', 'nor write the binding');
reset role;

-- The trusted role binds; the binding lasts for its transaction only.
set role rebac_t_trusted;
begin;
select rebac.set_principal('acme', 'lk');
select t.eq((select array_agg(id order by id) from secret), array[1], 'the bound principal fences the scan');
set local rebac.principal = 'ceo';
select t.eq((select array_agg(id order by id) from secret), array[1], 'changing the setting changes nothing');
set local rebac.tenant = 'globex';
select t.raises($$ select * from secret $$, 'RB422', 'changing the tenant after binding is refused');
commit;
select t.raises($$ select * from secret $$, 'RB400', 'the binding ended with its transaction');
-- Binding writes, so it fails in a read-only transaction (and on a read replica): an error, never a wider read.
begin read only;
select t.raises($$ select rebac.set_principal('acme', 'lk') $$, '25006', 'binding is refused read-only');
rollback;
reset role;
reset rebac.principal;

-- Who bypasses the fence: rebac.rls_bypasses names every role and view that reads a fenced table without it.
create table audit_doc (id int primary key, container text);
select rebac.enable_rls('audit_doc', 'container', force => true);
select t.ok(not exists (select 1 from rebac.rls_bypasses('audit_doc') where kind = 'role' and name = current_user
                          and not (select rolsuper or rolbypassrls from pg_roles where rolname = current_user)),
            'a FORCEd table: its owner is not listed (unless it is a superuser or BYPASSRLS anyway)');
create view audit_v as select * from audit_doc;
select t.ok(exists (select 1 from rebac.rls_bypasses('audit_doc') where kind = 'view' and name = 'audit_v')
            = (select rolsuper or rolbypassrls from pg_roles where rolname = current_user),
            'a view owned by a role that bypasses RLS is listed');
do $$ begin
  if current_setting('server_version_num')::int >= 150000 then
    execute 'alter view audit_v set (security_invoker = true)';
    perform t.ok(not exists (select 1 from rebac.rls_bypasses('audit_doc') where name = 'audit_v'),
                 'with security_invoker it is not');
  end if;
end $$;
create materialized view audit_m as select * from audit_doc;
select t.ok(exists (select 1 from rebac.rls_bypasses('audit_doc') where kind = 'materialized view' and name = 'audit_m'),
            'a materialized view always is: it holds rows read once, by its owner');
-- An unforced table's owner is listed.
create table audit_open (id int primary key, container text);
select t.ok(exists (select 1 from rebac.rls_bypasses('audit_open') where kind = 'role' and name = current_user),
            'the owner of a table without FORCE ROW LEVEL SECURITY is listed');
