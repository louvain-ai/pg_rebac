-- The set is inlined into the calling query: a semi-join on rebac.readable's key, its checks run once.
select t.beat();
create table doc (id int primary key, container text not null);
insert into doc select g, 'c' || (g % 500) from generate_series(1, 20000) g;
create index on doc (container);
select rebac.project('acme', 'alice', array(select 'c' || g from generate_series(1, 5) g), 't');
insert into rebac.readable select 'acme', 'p' || (g % 300), 'c' || (g % 500) from generate_series(1, 50000) g
  on conflict do nothing;
analyze;
set rebac.tenant = 'acme';
create temp table plan_text (line text);
do $$
declare r record;
begin
  for r in execute 'explain (costs off) select count(*) from doc where container in (select * from rebac.readable_set(''alice''))' loop
    insert into plan_text values (r."QUERY PLAN");
  end loop;
end $$;
select t.ok(not exists (select 1 from plan_text where line like '%Function Scan%'), 'readable_set is inlined, not a function scan');
select t.ok(exists (select 1 from plan_text where line like '%readable_pkey%' or line like '%on readable%'), 'the plan reads rebac.readable directly');
select t.ok(exists (select 1 from plan_text where line like '%InitPlan%'), 'the checks run once as an InitPlan');
select t.eq((select count(*) from doc where container in (select * from rebac.readable_set('alice')))::int, 200, 'fenced count');
-- The checks still fire in the inlined form.
select t.raises($$ select count(*) from doc where container in (select * from rebac.readable_set('nobody')) $$, 'RB404',
                'inlined form keeps RB404');
