---
title: Breadth and the two paths
description: When to fence an HNSW ordering, when to sort exactly, and the iterative_scan setting that makes the first one correct.
---

A fenced vector read has two shapes. Which one is right depends on how much of the tenant the reader can see,
which `rebac.breadth(p)` tells you in one primary-key read.

## The ANN path

```sql
set local hnsw.iterative_scan = relaxed_order;
select id from passage
where container in (select * from rebac.readable_set(p))
order by embedding <=> $1
limit 20;
```

The planner keeps the HNSW index scan outside and probes `readable_pkey` for each candidate. In the planner study
it chose that plan at every breadth from 0.1% to 100%.

### iterative_scan is required

With pgvector's default, `hnsw.iterative_scan = off`, the index returns `ef_search` candidates and the fence
discards the ones the reader cannot see. Nothing raises an error. The query simply returns fewer than k rows:

| Breadth | Rows returned, iterative off | Rows returned, relaxed_order |
|---|---|---|
| 5% | 8% of k | 100% of k |
| 10% | 20% of k | 100% of k |
| 30% | 62% of k | 100% of k |

So set `relaxed_order` on every fenced ANN read, as `set local` in the read's transaction.

## The exact path

```sql
select id from passage
where container = any(array(select * from rebac.readable_set(p)))
order by embedding <=> $1
limit 20;
```

For a narrow reader, ANN is the wrong tool even with iterative scans: the scan walks deep into the graph looking
for rows the reader may see. At 0.1% breadth, relaxed ANN reached pgvector's scan limit (`hnsw.max_scan_tuples`,
20,000), took about 100 ms and still came back short, with recall 0.54. The exact path took 1.6 ms with full
recall.

Use the array form for the exact path, not the `in` form. The planner estimates every reader at the tenant's
average breadth. Through the `in` form that produced a parallel sequential scan of `rebac.readable` for a narrow
reader, at 15 to 25 ms where 1.6 ms was possible. The planner treats an InitPlan array as small, which is what a
narrow reader is.

## Choosing

```sql
select rebac.breadth('priya');   -- containers this principal reads
```

In the study the crossover was about 1% breadth, roughly 5,000 candidate passages, where exact took 7 ms and ANN
took 12 ms at recall 0.95. The study's router sent readers below 50 containers to the exact path and the rest to
relaxed ANN, and it returned all k rows at every measured breadth. The right threshold depends on your passages per container; measure it with
[the study's harness](/docs/performance/#reproduce-it).

Run fenced reads with custom plans (`set local plan_cache_mode = force_custom_plan`), so each plan sees its own
reader.

## Not settled yet

A planner support function that supplies each principal's selectivity, so Postgres could choose the path itself,
is future work. So are measurements on Postgres 18 and on tenants with skewed passages per container.
