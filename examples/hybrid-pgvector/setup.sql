-- A tenant's passages, readers of every breadth, and an HNSW index: the planner study and the hybrid example.
--   psql -v passages=500000 -v containers=5000 -v dims=128 -v principals=2000 -f setup.sql
-- Needs rebac.sql installed and pgvector 0.8 or later.
create extension if not exists vector;
drop table if exists passage;
create table passage (
  id        bigint primary key,
  tenant    text not null,
  container text not null,
  body      text not null,
  tsv       tsvector,
  embedding vector(:dims) not null
);
-- Clustered embeddings (200 topics, each passage a topic centre plus noise), so an ANN index has structure to find;
-- uniform random vectors defeat HNSW whatever the fence does. Topic and container are independent.
drop table if exists topic;
create table topic as
select t as id, (select array_agg(random() - 0.5) from generate_series(1, :dims) d where d > t * 0) as centre
from generate_series(0, 199) t;
insert into passage (id, tenant, container, body, embedding)
select g, 'acme', 'c' || (g % :containers),
       'passage ' || g || ' about ' || (array['pricing','renewal','security','roadmap','hiring','incident'])[1 + g % 6],
       (select array_agg(c + (random() - 0.5) * 0.3)::vector from unnest(tp.centre) c)
from generate_series(1, :passages) g
join topic tp on tp.id = (hashint4(g) & 2147483647) % 200;
update passage set tsv = to_tsvector('english', body);
create index passage_container_idx on passage (tenant, container);
create index passage_tsv_idx on passage using gin (tsv);
set maintenance_work_mem = '1GB';
set max_parallel_maintenance_workers = 3;
create index passage_embedding_idx on passage using hnsw (embedding vector_cosine_ops);

-- Readers. Named readers at fixed breadths, and a background population so rebac.readable's statistics look like a
-- real tenant's (most people read a little, a few read a lot).
delete from rebac.readable where tenant = 'acme';
delete from rebac.principal_state where tenant = 'acme';
create temp table breadth (principal text, fraction float8);
insert into breadth values ('b0001', 0.001), ('b001', 0.01), ('b005', 0.05), ('b010', 0.10), ('b020', 0.20),
                           ('b030', 0.30), ('b050', 0.50), ('b100', 1.0);
insert into breadth select 'bg' || g, least(1.0, 0.002 + power(random(), 4) * 0.4) from generate_series(1, :principals) g;
insert into rebac.readable (tenant, principal, container)
select 'acme', b.principal, 'c' || c
from breadth b cross join lateral (
  select c from generate_series(0, :containers - 1) c order by md5(b.principal || c) limit ceil(b.fraction * :containers)
) x;
insert into rebac.principal_state (tenant, principal, state, n_containers, version, projected_at)
select 'acme', principal, 'projected', count(*), 1, now() from rebac.readable where tenant = 'acme' group by principal;
insert into rebac.watch_state (scope, heartbeat_at) values ('*', now())
on conflict (scope) do update set heartbeat_at = now();
analyze;
-- No daemon runs beside this example, so nothing beats the heartbeat: switch the staleness bound off here only.
update rebac.config set value = '0' where key = 'max_staleness_ms';
