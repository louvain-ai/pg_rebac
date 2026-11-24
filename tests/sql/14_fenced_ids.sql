-- rebac.fenced_ids: the k nearest rows a principal may read, exact below the threshold, fenced ANN above it.
create extension if not exists vector;
select t.beat();
set rebac.tenant = 'acme';
create table doc (id bigint primary key, tenant text not null, container text not null, embedding vector(2) not null);
insert into doc select g, 'acme', 'c' || (g % 10), ('[' || cos(g) || ',' || sin(g) || ']')::vector
from generate_series(1, 2000) g;
insert into doc select 100000 + g, 'globex', 'c1', '[1,0]' from generate_series(1, 50) g;  -- another tenant, same id
create index doc_container_idx on doc (tenant, container);
create index doc_hnsw on doc using hnsw (embedding vector_cosine_ops);
select rebac.project('acme', 'nar', array['c1'], 't', 'live', now());
select rebac.project('acme', 'wide', array['c0','c1','c2','c3','c4','c5','c6','c7','c8'], 't', 'live', now());

-- The narrow reader (200 rows) is below the threshold: exact, complete, and only its own container and tenant.
select t.eq((select array_agg(distinct path) from rebac.fenced_ids('nar', 'doc', '[1,0]'::vector, 20, tenant_col => 'tenant')),
            array['exact'], 'below the threshold: exact');
select t.eq((select count(*) from rebac.fenced_ids('nar', 'doc', '[1,0]'::vector, 20, tenant_col => 'tenant')), 20::bigint, 'k rows');
select t.ok(not exists (select 1 from rebac.fenced_ids('nar', 'doc', '[1,0]'::vector, 20, tenant_col => 'tenant') f
                        join doc d on d.id::text = f.id where d.container <> 'c1' or d.tenant <> 'acme'), 'fenced');
select t.eq((select array_agg(id order by distance, id) from rebac.fenced_ids('nar', 'doc', '[1,0]'::vector, 5, tenant_col => 'tenant')),
            (select array_agg(id::text order by embedding <=> '[1,0]', id) from
               (select * from doc where tenant = 'acme' and container = 'c1' order by embedding <=> '[1,0]', id limit 5) x),
            'exact is the true top k');

-- The wide reader (1800 rows) above a threshold of 500: ANN, k rows, in order, fenced.
select t.eq((select array_agg(distinct path) from rebac.fenced_ids('wide', 'doc', '[1,0]'::vector, 20, tenant_col => 'tenant',
             exact_below => 500)), array['ann'], 'above the threshold: ann');
select t.eq((select count(*) from rebac.fenced_ids('wide', 'doc', '[1,0]'::vector, 20, tenant_col => 'tenant', exact_below => 500)),
            20::bigint, 'k rows from ann');
select t.ok(not exists (select 1 from rebac.fenced_ids('wide', 'doc', '[1,0]'::vector, 20, tenant_col => 'tenant', exact_below => 500) f
                        join doc d on d.id::text = f.id where d.container = 'c9' or d.tenant <> 'acme'), 'ann is fenced');
select t.ok((select bool_and(ok) from (select distance >= lag(distance, 1, '-infinity') over (order by ordinality) as ok
             from rebac.fenced_ids('wide', 'doc', '[1,0]'::vector, 20, tenant_col => 'tenant', exact_below => 500)
             with ordinality) x), 'returned in distance order');
-- The setting is the default threshold; the iterative-scan setting does not leak into the caller.
update rebac.config set value = '100' where key = 'exact_below_rows';
select t.eq((select array_agg(distinct path) from rebac.fenced_ids('nar', 'doc', '[1,0]'::vector, 20, tenant_col => 'tenant')),
            array['ann'], 'exact_below_rows is configurable');
select t.eq(current_setting('hnsw.iterative_scan'), 'off', 'the caller''s setting is untouched');

-- Fenced like every read: unknown principal RB404, no tenant RB400, a bad operator RB422.
select t.raises($$ select * from rebac.fenced_ids('nobody', 'doc', '[1,0]'::vector, 5) $$, 'RB404', 'unknown principal');
select t.raises($$ select * from rebac.fenced_ids('nar', 'doc', '[1,0]'::vector, 5, op => '; drop table doc') $$, 'RB422',
                'only a distance operator');
reset rebac.tenant;
select t.raises($$ select * from rebac.fenced_ids('nar', 'doc', '[1,0]'::vector, 5) $$, 'RB400', 'no tenant');
