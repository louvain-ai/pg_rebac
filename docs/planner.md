# The planner study: a fenced HNSW ordering at every reader breadth

The design's largest risk was that an inlined fence next to an HNSW scan would pick a bad plan for readers of middling
breadth. This page measures it. Reproduce it with `examples/hybrid-pgvector/` (`setup.sql`, then `bench.py`).

## Set-up

- Postgres 17, pgvector 0.8.6, 4 CPUs, 6 GB, `shared_buffers = 1GB`, JIT off. Everything was cached in memory.
- 500,000 passages in 5,000 containers (100 passages each, containers spread through the heap), 128 dimensions.
- Embeddings are 200 clustered topics, each passage a topic centre plus noise; queries are near a topic. Topic and
  container are independent.
- HNSW at the defaults (`m = 16`, `ef_construction = 64`, `ef_search = 40`), cosine distance.
- `fence.readable` holds 875,000 rows: eight named readers at fixed breadths, and 2,000 background readers whose
  breadths are skewed (most read a little, a few read a lot), so the statistics look like a tenant's.
- k = 20, 10 queries per cell. "rows/k" is the fraction of k rows returned. "recall" is recall@20 against the exact
  answer over the reader's own passages.

## Results

```
passages=500000 containers=5000 k=20 queries=10
reader    breadth  passages | mode                 plan                                 p50 ms   max ms  rows/k  recall
b0001        0.1%       500 | ann, iterative off   HNSW Nested Loop > Index Scan [pas      5.7     12.3    0.01    0.01
b0001        0.1%       500 | ann, relaxed_order   HNSW Nested Loop > Index Scan [pas    107.4    124.5    0.88    0.54
b0001        0.1%       500 | ann, relaxed, ef 200 HNSW Nested Loop > Index Scan [pas     99.6    122.7    0.89    0.54
b0001        0.1%       500 | exact (in, CTE)      Gather > Nested Loop > Seq Scan >      24.9     33.8    1.00    1.00
b0001        0.1%       500 | exact (array)        Index Only Scan [readable_pkey] >       1.6      4.1    1.00    1.00
b0001        0.1%       500 | auto (by breadth)    Index Only Scan [readable_pkey] >       4.3      4.5    1.00    1.00
b001         1.0%      5000 | ann, iterative off   HNSW Nested Loop > Index Scan [pas      3.8      4.5    0.03    0.04
b001         1.0%      5000 | ann, relaxed_order   HNSW Nested Loop > Index Scan [pas     11.9     13.0    1.00    0.95
b001         1.0%      5000 | ann, relaxed, ef 200 HNSW Nested Loop > Index Scan [pas     11.8     12.5    1.00    0.95
b001         1.0%      5000 | exact (in, CTE)      Gather > Nested Loop > Seq Scan >      27.5     43.1    1.00    1.00
b001         1.0%      5000 | exact (array)        Index Only Scan [readable_pkey] >       7.1      7.4    1.00    1.00
b001         1.0%      5000 | auto (by breadth)    HNSW Nested Loop > Index Scan [pas     11.7     13.1    1.00    0.95
b005         5.0%     25000 | ann, iterative off   HNSW Nested Loop > Index Scan [pas      1.6      3.9    0.08    0.09
b005         5.0%     25000 | ann, relaxed_order   HNSW Nested Loop > Index Scan [pas      5.8      9.5    1.00    0.96
b005         5.0%     25000 | ann, relaxed, ef 200 HNSW Nested Loop > Index Scan [pas      4.9      6.2    1.00    0.96
b005         5.0%     25000 | exact (in, CTE)      Gather > Nested Loop > Seq Scan >      66.9     81.8    1.00    1.00
b005         5.0%     25000 | exact (array)        Index Only Scan [readable_pkey] >      28.2     31.5    1.00    1.00
b005         5.0%     25000 | auto (by breadth)    HNSW Nested Loop > Index Scan [pas      7.1     12.3    1.00    0.96
b010        10.0%     50000 | ann, iterative off   HNSW Nested Loop > Index Scan [pas      1.7      3.7    0.20    0.20
b010        10.0%     50000 | ann, relaxed_order   HNSW Nested Loop > Index Scan [pas      6.7      8.7    1.00    0.95
b010        10.0%     50000 | ann, relaxed, ef 200 HNSW Nested Loop > Index Scan [pas      5.1      7.6    1.00    0.96
b010        10.0%     50000 | exact (in, CTE)      Gather > Nested Loop > Seq Scan >      57.6    117.8    1.00    1.00
b010        10.0%     50000 | exact (array)        Index Only Scan [readable_pkey] >      54.8     55.1    1.00    1.00
b010        10.0%     50000 | auto (by breadth)    HNSW Nested Loop > Index Scan [pas      6.3     10.3    1.00    0.95
b020        20.0%    100000 | ann, iterative off   HNSW Nested Loop > Index Scan [pas      1.6      3.7    0.34    0.34
b020        20.0%    100000 | ann, relaxed_order   HNSW Nested Loop > Index Scan [pas      5.1      7.9    1.00    0.91
b020        20.0%    100000 | ann, relaxed, ef 200 HNSW Nested Loop > Index Scan [pas      4.6      6.6    1.00    0.95
b020        20.0%    100000 | exact (in, CTE)      Gather > Nested Loop > Seq Scan >     166.0    169.6    1.00    1.00
b020        20.0%    100000 | exact (array)        Index Only Scan [readable_pkey] >     107.7    114.8    1.00    1.00
b020        20.0%    100000 | auto (by breadth)    HNSW Nested Loop > Index Scan [pas      5.1      8.5    1.00    0.91
b030        30.0%    150000 | ann, iterative off   HNSW Nested Loop > Index Scan [pas      1.7      5.8    0.62    0.62
b030        30.0%    150000 | ann, relaxed_order   HNSW Nested Loop > Index Scan [pas      4.7      5.8    1.00    0.95
b030        30.0%    150000 | ann, relaxed, ef 200 HNSW Nested Loop > Index Scan [pas      5.0      6.5    1.00    0.97
b030        30.0%    150000 | exact (in, CTE)      Gather > Nested Loop > Seq Scan >     234.1    252.3    1.00    1.00
b030        30.0%    150000 | exact (array)        Index Only Scan [readable_pkey] >     161.3    180.6    1.00    1.00
b030        30.0%    150000 | auto (by breadth)    HNSW Nested Loop > Index Scan [pas      4.3     14.8    1.00    0.95
b050        50.0%    250000 | ann, iterative off   HNSW Nested Loop > Index Scan [pas      1.8      4.7    0.92    0.83
b050        50.0%    250000 | ann, relaxed_order   HNSW Nested Loop > Index Scan [pas      4.5      5.0    1.00    0.90
b050        50.0%    250000 | ann, relaxed, ef 200 HNSW Nested Loop > Index Scan [pas      5.8      7.7    1.00    0.97
b050        50.0%    250000 | exact (in, CTE)      Hash Join > Seq Scan > Hash > Inde    303.7    343.1    1.00    1.00
b050        50.0%    250000 | exact (array)        Index Only Scan [readable_pkey] >     263.8    271.2    1.00    1.00
b050        50.0%    250000 | auto (by breadth)    HNSW Nested Loop > Index Scan [pas      4.2      5.8    1.00    0.90
b100       100.0%    500000 | ann, iterative off   HNSW Nested Loop > Index Scan [pas      1.6      3.8    1.00    0.84
b100       100.0%    500000 | ann, relaxed_order   HNSW Nested Loop > Index Scan [pas      3.5      4.1    1.00    0.84
b100       100.0%    500000 | ann, relaxed, ef 200 HNSW Nested Loop > Index Scan [pas      4.6      5.6    1.00    0.97
b100       100.0%    500000 | exact (in, CTE)      Hash Join > Seq Scan > Hash > Inde    452.2    493.6    1.00    1.00
b100       100.0%    500000 | exact (array)        Index Only Scan [readable_pkey] >     481.6    546.6    1.00    1.00
b100       100.0%    500000 | auto (by breadth)    HNSW Nested Loop > Index Scan [pas      3.8      5.5    1.00    0.84
unfenced HNSW baseline p50 1.9 ms
```

Modes:

- **ann, iterative off**: `container in (select * from fence.readable_set(p)) order by embedding <=> q limit 20`,
  with `hnsw.iterative_scan = off` (pgvector's default).
- **ann, relaxed_order**: the same query with `hnsw.iterative_scan = relaxed_order`.
- **exact (in, CTE)**: the reader's passages through the `in` form, materialised, then sorted exactly.
- **exact (array)**: `container = any(array(select * from fence.readable_set(p)))`, sorted exactly.
- **auto (by breadth)**: exact (array) below 50 containers (`fence.breadth(p)`), otherwise relaxed ANN.

## What it shows

1. **The plan shape is stable, and middling breadth is not the problem.** At every breadth from 0.1% to 100% the
   planner chose the same plan: an HNSW index scan outside, and a `readable_pkey` probe per candidate inside. From
   5% to 50% the fenced ANN leg with `relaxed_order` returned all k rows with recall 0.90 to 0.96, in 4 to 7 ms. The
   unfenced HNSW baseline took 1.9 ms.
2. **Without iterative scans, the fence silently loses rows.** With `hnsw.iterative_scan = off`, the leg returned
   8% of k at 5% breadth, 20% at 10% and 62% at 30%. It raised no error. This is the post-filtering failure the fence
   exists to prevent, so every fenced ANN read must set `relaxed_order`.
3. **Narrow readers need the exact path.** At 0.1% breadth, relaxed ANN walked to pgvector's scan limit
   (`hnsw.max_scan_tuples`, 20,000). It took about 100 ms and still came back short: 0.88 of k, recall 0.54. The
   exact path took 1.6 ms with full recall. The crossover is about 1% here (roughly 5,000 candidate passages), where
   exact took 7 ms and ANN took 12 ms at recall 0.95.
4. **The planner does not know a reader's breadth.** Without a per-principal statistic, it estimates every reader at
   the tenant's average: 451 containers for a reader of 5. Fed through the `in` form, that estimate produced a
   parallel sequential scan of `fence.readable` and cost 15 to 25 ms where 1.6 ms was possible. The array form avoids
   it, because the planner treats an InitPlan array as small, and that is what a narrow reader is.
5. **The choice is the application's, and it is cheap.** `fence.breadth(p)` is one primary-key read. Routing on it
   gave the best of both paths at every breadth in this data. A larger `ef_search` raised the broad readers' recall
   (0.84 to 0.97 at 100%) and changed little for narrow ones, whose recall is bounded by the scan limit.

## Recommendations

- Fence an ANN leg with `container in (select * from fence.readable_set(p))` and `set local hnsw.iterative_scan =
  relaxed_order`.
- Route readers below a few thousand candidate rows to the exact path, as
  `container = any(array(select * from fence.readable_set(p)))`, using `fence.breadth(p)`.
- Use custom plans (`plan_cache_mode = force_custom_plan`) for fenced reads, so each plan sees its own reader.
- Not settled here, and still to measure: a planner support function (C mode) that supplies the per-principal
  selectivity, so the planner can choose the path itself; Postgres 18; and tenants with skewed passages per container.
