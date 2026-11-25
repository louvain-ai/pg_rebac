-- A hybrid query: a vector leg and a full-text leg, each fenced BEFORE its order and limit, fused by reciprocal rank.
--   psql -v principal=b010 -v words="'pricing'" -f hybrid.sql
set rebac.tenant = 'acme';
set hnsw.iterative_scan = relaxed_order;   -- without it a fenced HNSW leg silently returns fewer than k rows
set plan_cache_mode = force_custom_plan;   -- the plan depends on the reader's breadth
\set q '(select embedding from passage where id = 7)'
with v as (
  select p.id, row_number() over (order by p.embedding <=> :q) as r
  from passage p
  where p.tenant = 'acme' and p.container in (select * from rebac.readable_set(:'principal'))
  order by p.embedding <=> :q
  limit 100
), k as (
  select p.id, row_number() over (order by ts_rank(p.tsv, query) desc) as r
  from passage p, websearch_to_tsquery('english', :words) query
  where p.tsv @@ query and p.tenant = 'acme' and p.container in (select * from rebac.readable_set(:'principal'))
  order by ts_rank(p.tsv, query) desc
  limit 100
)
select id, sum(1.0 / (60 + r)) as rrf
from (select * from v union all select * from k) x
group by id order by rrf desc limit 20;
