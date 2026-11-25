-- Two passages per container (200,000), 768 dimensions, clustered on 200 topics, with an HNSW index; and 100 query
-- vectors near the topics. Built once into a template database that each layout is copied from.
-- maintenance_work_mem stays below the server's shared memory (/dev/shm): parallel builds allocate it there.
create extension if not exists vector;
create schema if not exists bench;
create or replace function bench.cid(j int) returns text language sql immutable as $$ select md5('c' || j)::uuid::text $$;
create unlogged table bench_topic as
  select t, array(select random()::real - 0.5 from generate_series(1, 768) where t >= 0) v from generate_series(0, 199) t;
create table passage (id bigint primary key, container text not null, embedding vector(768) not null);
insert into passage
select j * 2 + r, bench.cid(j),
       (select array_agg(tv.x + (random()::real - 0.5) * 0.6 order by tv.o)
        from unnest((select v from bench_topic where t = (hashint4(j * 2 + r) & 1023) % 200)) with ordinality tv(x, o))::vector
from generate_series(0, 99999) j, generate_series(0, 1) r;
create index on passage (container);
create table bench_query as
select q as id, (select array_agg(tv.x + (random()::real - 0.5) * 0.3 order by tv.o)
                 from unnest((select v from bench_topic where t = q * 7 % 200)) with ordinality tv(x, o))::vector(768) as vec
from generate_series(0, 99) q;
set maintenance_work_mem = '1500MB';
set max_parallel_maintenance_workers = 7;
create index on passage using hnsw (embedding vector_cosine_ops);
vacuum analyze passage, bench_query;
