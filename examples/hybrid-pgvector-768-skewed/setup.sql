-- A synthetic fenced-vector-search tenant, skewed: 768-dim embeddings, Zipf-skewed container sizes, topic-affine
-- containers, readers at 0.1% .. 50% of the passages and a skewed background population.
--   psql -v ON_ERROR_STOP=1 -f setup.sql            (defaults: 500,000 passages, 20,000 containers, 768 dims)
--   psql -v ON_ERROR_STOP=1 -v passages=200000 -v containers=10000 -f setup.sql
-- Needs rebac.sql installed in the same database and pgvector 0.8 or later.
\if :{?passages}   \else \set passages 500000 \endif
\if :{?containers} \else \set containers 20000 \endif
\if :{?dims}       \else \set dims 768 \endif
\if :{?topics}     \else \set topics 500 \endif
\if :{?noise}      \else \set noise 0.6 \endif
\if :{?affinity}   \else \set affinity 0.6 \endif
\if :{?principals} \else \set principals 2000 \endif
\if :{?nqueries}   \else \set nqueries 400 \endif
\if :{?mwm}        \else \set mwm 4GB \endif
\if :{?workers}    \else \set workers 7 \endif
\timing on
create extension if not exists vector;
drop table if exists passage, topic, container_size, bench_query, bench_truth cascade;

-- Container sizes: a Zipf-like tail, size(rank r) ~ 1 / (r + 30). With the defaults the largest container holds about
-- 2,500 passages, the median about 8 and the smallest about 4. Each container has a home topic.
create table container_size as
with w as (select r, 1.0 / (r + 30) as w from generate_series(1, :containers) r),
     s as (select r, w, sum(w) over () as tot from w)
select 'c' || (r - 1) as container, r,
       greatest(1, round(:passages * w / tot))::int as n,
       floor(random() * :topics)::int as home
from s;

-- Topic centres, clustered embeddings: a passage is its topic's centre plus uniform noise. A container's passages take
-- its home topic with probability :affinity and a random topic otherwise, so a narrow reader's containers cover only
-- some topics (a query near a topic the reader cannot see must walk far to find a readable neighbour).
create table topic as
select t as id, (select array_agg(random() - 0.5) from generate_series(1, :dims)) as centre
from generate_series(0, :topics - 1) t;

create table passage (
  id        bigint primary key,
  tenant    text not null,
  container text not null,
  embedding vector(:dims) not null
);
-- Slots are shuffled so a container's passages are spread through the heap, as ingestion order spreads them.
create temp table slot as
select row_number() over (order by random()) as id, cs.container,
       case when random() < :affinity then cs.home else floor(random() * :topics)::int end as topic
from container_size cs cross join lateral generate_series(1, cs.n) g;
insert into passage (id, tenant, container, embedding)
select s.id, 'acme', s.container, (select array_agg(c + (random() - 0.5) * :noise)::vector from unnest(tp.centre) c)
from slot s join topic tp on tp.id = s.topic;
drop table slot;
create index passage_container_idx on passage (tenant, container);

-- Queries are near a topic, as a question is near what it asks about.
create table bench_query as
select g as id, (select array_agg(c + (random() - 0.5) * :noise)::vector from unnest(tp.centre) c) as vec
from generate_series(1, :nqueries) g
join lateral (select centre from topic order by random() + g * 0 limit 1) tp on true;
alter table bench_query add primary key (id);

-- The HNSW build: the whole graph is held in maintenance_work_mem (shared memory when parallel); watch it with
--   select phase, blocks_done, blocks_total, tuples_done, tuples_total from pg_stat_progress_create_index;
set maintenance_work_mem = :'mwm';
set max_parallel_maintenance_workers = :workers;
create index passage_embedding_idx on passage using hnsw (embedding vector_cosine_ops);
reset maintenance_work_mem; reset max_parallel_maintenance_workers;

-- Readers. Each named reader reads random containers (in a per-reader shuffled order) until it holds its fraction of
-- the PASSAGES, so a reader's breadth in containers varies with how many big containers it drew.
delete from rebac.readable where tenant = 'acme';
delete from rebac.principal_state where tenant = 'acme';
create temp table breadth (principal text, fraction float8);
insert into breadth values ('b0001', 0.001), ('b001', 0.01), ('b005', 0.05), ('b010', 0.10), ('b020', 0.20), ('b050', 0.50);
select rebac.project('acme', b.principal, (
         select array_agg(container) from (
           select container, n, sum(n) over (order by md5(b.principal || container)) as cum from container_size
         ) o where o.cum - o.n < b.fraction * (select sum(n) from container_size)), 't')
from breadth b;
-- Background readers: most read a little, a few read a lot (fraction = 0.001 + 0.5 * u^8, mean about 6%).
select count(rebac.project('acme', 'bg' || f.g,
         coalesce((select array_agg(container) from container_size where random() < f.frac), '{}'), 't'))
from (select g, least(1.0, 0.001 + power(random(), 8) * 0.5) as frac from generate_series(1, :principals) g) f;

-- No daemon runs beside this example, so nothing beats the heartbeat or refreshes a projection: switch the bounds off
-- here only (max_projection_age_ms exists only in newer rebac.sql).
update rebac.config set value = '0' where key in ('max_staleness_ms', 'max_projection_age_ms');
vacuum analyze passage;
vacuum analyze rebac.readable;
vacuum analyze rebac.principal_state;
select (select count(*) from passage) as passages, count(*) as containers, max(c) as largest_container,
       percentile_disc(0.5) within group (order by c) as median_container, min(c) as smallest_container
from (select container, count(*) c from passage group by container) x;
select principal, n_containers, (select count(*) from passage p where p.container in
         (select container from rebac.readable r where r.tenant = 'acme' and r.principal = s.principal)) as passages
from rebac.principal_state s where principal like 'b0%' or principal like 'b1%' or principal like 'b5%' order by 3;
select count(*) as readers, count(*) filter (where n_containers = 0) as empty, sum(n_containers) as readable_rows
from rebac.principal_state;
