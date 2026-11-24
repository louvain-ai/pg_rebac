# The planner study: a fenced HNSW ordering at every reader breadth

The design's largest risk was that an inlined fence next to an HNSW scan would pick a bad plan for readers of middling
breadth. This page measures it. Reproduce it with `examples/hybrid-pgvector/` (`setup.sql`, then `bench.py`).

## Limits of this study

These limits apply to the 128-dimension study, the first results below. A second run, after them, removes some of
them. Read the numbers with these limits. They are why the cover page states its scope beside them.

- **Synthetic data.** Clustered random embeddings, not a model's. Every container holds exactly 100 passages.
- **128 dimensions.** Production embeddings are usually 768 to 1,536, so distance computations and the index are
  larger.
- **A warm cache.** Everything was in memory, with nothing else running.
- **Ten queries per cell**, so "p50" is the median of ten. There is no p95 or p99.
- **One client.** There were no concurrent readers or writers.
- **One machine**: 4 CPUs, Docker, Postgres 17 and pgvector 0.8.6.

## Set-up

- Postgres 17, pgvector 0.8.6, 4 CPUs, 6 GB, `shared_buffers = 1GB`, JIT off. Everything was cached in memory.
- 500,000 passages in 5,000 containers (100 passages each, containers spread through the heap), 128 dimensions.
- Embeddings are 200 clustered topics, each passage a topic centre plus noise; queries are near a topic. Topic and
  container are independent.
- HNSW at the defaults (`m = 16`, `ef_construction = 64`, `ef_search = 40`), cosine distance.
- `rebac.readable` holds 875,000 rows: eight named readers at fixed breadths, and 2,000 background readers whose
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

- **ann, iterative off**: `container in (select * from rebac.readable_set(p)) order by embedding <=> q limit 20`,
  with `hnsw.iterative_scan = off` (pgvector's default).
- **ann, relaxed_order**: the same query with `hnsw.iterative_scan = relaxed_order`.
- **exact (in, CTE)**: the reader's passages through the `in` form, materialised, then sorted exactly.
- **exact (array)**: `container = any(array(select * from rebac.readable_set(p)))`, sorted exactly.
- **auto (by breadth)**: exact (array) below 50 containers (`rebac.breadth(p)`), otherwise relaxed ANN.

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
   parallel sequential scan of `rebac.readable` and cost 15 to 25 ms where 1.6 ms was possible. The array form avoids
   it, because the planner treats an InitPlan array as small, and that is what a narrow reader is.
5. **The choice is the application's, and it is cheap.** `rebac.breadth(p)` is one primary-key read. Routing on it
   gave the best of both paths at every breadth in this data. A larger `ef_search` raised the broad readers' recall
   (0.84 to 0.97 at 100%) and changed little for narrow ones, whose recall is bounded by the scan limit.

## A second run: 768 dimensions, skewed containers, p99 and concurrency

`examples/hybrid-pgvector-realistic/` addresses some of the limits above. It is still synthetic.

- **Data.** 499,868 passages at 768 dimensions in 20,000 containers, sized about 1/(rank + 30): the largest holds
  2,486 passages, the median 8. Each container takes 60% of its passages from a home topic (500 topics, noise 0.6).
  `rebac.readable` holds 2.17 million rows: six named readers and 2,000 skewed background readers.
- **Server.** Postgres 17.11 and pgvector 0.8.6 in Docker on a 10-CPU laptop (a 15.7 GB VM),
  `shared_buffers = 2GB`, `fsync = off`. HNSW at the defaults, k = 20, a warm cache.
- **Measurement.** Latency is 300 pgbench queries per cell (the first 20 dropped). Rows/k and recall are over 200
  queries against the exact top 20 over the reader's own passages.

```
reader  breadth ctrs  passages | mode                 p50ms  p95ms  p99ms rows/k recall
b0001    0.1%    45      501 | ann, iterative off     4.8    9.5   25.0  0.00  0.00
b0001    0.1%    45      501 | ann, relaxed_order   116.1  207.6  264.0  0.88  0.17
b0001    0.1%    45      501 | exact (array)          6.7   16.1   20.2  1.00  1.00
b001     1.1%   202     5291 | ann, iterative off     4.3    9.5   11.6  0.02  0.02
b001     1.1%   202     5291 | ann, relaxed_order    48.1   67.9   78.5  1.00  0.27
b001     1.1%   202     5291 | exact (array)         43.7   67.7  108.0  1.00  1.00
b005     5.1%  1004    25596 | ann, iterative off     5.6   11.5   14.8  0.11  0.06
b005     5.1%  1004    25596 | ann, relaxed_order    29.0   43.0   60.5  1.00  0.23
b005     5.1%  1004    25596 | exact (array)        187.7  247.6  270.0  1.00  1.00
b010    10.0%  2116    49994 | ann, iterative off     5.9   10.0   13.9  0.21  0.06
b010    10.0%  2116    49994 | ann, relaxed_order    18.0   26.7   30.4  1.00  0.18
b010    10.0%  2116    49994 | exact (array)        276.3  359.6  417.6  1.00  1.00
b020    20.0%  4421    99988 | ann, iterative off     3.0    4.2    7.9  0.40  0.06
b020    20.0%  4421    99988 | ann, relaxed_order     6.5    8.6   12.7  1.00  0.13
b020    20.0%  4421    99988 | exact (array)        507.8  593.6  667.7  1.00  1.00
b050    50.0% 10206   249936 | ann, iterative off     2.4    3.0    3.3  0.94  0.08
b050    50.0% 10206   249936 | ann, relaxed_order     2.8    4.6    5.3  1.00  0.09
b050    50.0% 10206   249936 | exact (array)       1222.5 1346.5 1401.7  1.00  1.00
unfenced HNSW baseline: p50 3.7, p95 6.1, p99 7.3 ms (recall 0.09 at ef_search 40)

concurrent fenced reads, 45 s per level, each query for a random reader
mix (readers)                          clients |  qps  p50ms  p95ms  p99ms
ANN only (1% breadth and up)                 1 |   59   10.0   42.5   57.1
                                             8 |  449   11.0   44.1   49.7
                                            32 |  419   46.3  229.3  326.8
routed (under 1% of passages: exact)         1 |   88    5.8   38.4   47.6
                                             8 |  407   11.4   58.0  106.1
                                            32 |  419   37.7  270.1  483.7
```

What it shows:

1. **The fence still keeps k.** With `relaxed_order`, every reader from 1% breadth up got all k rows, and the plan
   was the same. Without it, readers of 0.1% to 10% got 0% to 21% of k, silently.
2. **At 768 dimensions the middle is slower.** Fenced ANN p50 was 29 ms at 5%, 18 ms at 10%, 6.5 ms at 20% and
   2.8 ms at 50% (p99 5 to 61 ms), against 4 to 7 ms at 128 dimensions. The "4 to 7 ms" figure belongs to the
   128-dimension run only.
3. **The exact path degrades faster.** At about 1% breadth, exact and ANN cost the same (44 and 48 ms), as before.
   Above it the exact path grows quickly (188 ms at 5%, 1.2 s at 50%), so routing by breadth matters more.
4. **The recall here says little.** Unfenced HNSW scored 0.09 on this data at `ef_search = 40`: isotropic noise in
   768 dimensions makes neighbours nearly tied. Fenced recall tracked unfenced recall at the same `ef_search`
   (0.10 against 0.09 at 50% breadth), which is the property the fence must keep. These recall figures are not a
   measure of ANN quality on real embeddings.
5. **Concurrency.** Throughput levelled at about 420 to 450 queries a second from 8 clients on 10 CPUs. At 32
   clients the machine was CPU-bound and p99 rose to 330 to 480 ms.

Limits that remain: synthetic embeddings, a warm cache, one node and one tenant, `fsync = off`, and no writes,
refreshes or narrowing during the run. The 128- and 768-dimension runs changed several things at once (dimensions,
container skew, topic affinity), so they are not a controlled comparison.

## The alternatives: row-level security, and filtering after retrieval

`examples/hybrid-pgvector-realistic/alternatives.py` runs on the same 768-dimension data as the second run (a fresh
build of `setup.sql` with the defaults: 499,868 passages, 20,000 containers, 2.2 million projection rows), on the
same laptop: Postgres 17.11 and pgvector 0.8.6 in Docker, `shared_buffers = 2GB`, `fsync = off`, a warm cache, one
client. Latency is pgbench (the first 10 queries of a cell dropped); "rows" is what query 1 returned. Run on 2026-11-24.

### Row-level security

`--rls` puts the fence in a policy instead of the query, four ways, and reads as a role that is neither a superuser
nor the table's owner (both bypass RLS):

- **query fence**: no policy; `container in (select * from rebac.readable_set(p))` in the query, as above.
- **rls, set**: `rebac.enable_rls`, a restrictive policy on the same set.
- **rls, per row**: a policy calling `rebac.readable_by(p, container)`, pg_rebac's own per-row check. It is PL/pgSQL
  and calls `rebac.checked(p)` for every row, as a call to a check function would. (`setup.sql` switches the
  liveness and freshness bounds off, so those two lookups were skipped here; with them on it costs more.) It is local:
  a per-row call to a remote permission service would cost a round trip per row on top, which was not measured.
- **rls, exists**: the naive inline policy, `exists (select 1 from rebac.readable r where r.tenant = .. and
  r.principal = .. and r.container = passage.container)`.

Two shapes: ANN top-20 (`relaxed_order`) and `count(*)` over every row the reader may see. Up to 200 timed queries
per cell; the per-row `count` cells took 3 to 4 s a query, so they ran 7 to 9 queries and their "p99" is in effect the
maximum.

```
reader  passages shape |           query fence |              rls, set |          rls, per row |           rls, exists
                       |          p50 / p99 ms |          p50 / p99 ms |          p50 / p99 ms |          p50 / p99 ms
b0001        501 ann   |         122.2 / 185.6 |           51.8 / 86.5 |         422.6 / 465.6 |           48.5 / 59.0
b0001        501 count |             0.6 / 1.5 |           17.1 / 20.9 | 3416.8 / 3464.1 (n=9) |           45.9 / 51.1
b001        5291 ann   |          56.3 / 100.0 |           32.0 / 42.7 |          67.5 / 107.4 |           36.8 / 45.0
b001        5291 count |             1.4 / 1.7 |           17.9 / 28.3 | 3367.7 / 3473.4 (n=8) |           47.3 / 55.0
b005       25596 ann   |           19.7 / 27.4 |           17.2 / 24.9 |           25.1 / 38.8 |           17.4 / 31.3
b005       25596 count |             4.9 / 5.7 |           20.1 / 31.4 | 3344.6 / 3915.8 (n=8) |           53.5 / 57.1
b010       49994 ann   |           11.6 / 16.4 |            9.8 / 14.5 |           13.8 / 19.6 |           10.2 / 14.0
b010       49994 count |            9.0 / 10.2 |           21.1 / 41.1 | 3411.0 / 3585.7 (n=8) |           55.5 / 58.1
b020       99988 ann   |             7.1 / 9.8 |            7.1 / 13.7 |            8.3 / 13.4 |            6.6 / 23.9
b020       99988 count |           17.7 / 32.0 |           23.6 / 27.9 | 3591.1 / 3820.1 (n=7) |           61.2 / 64.7
b050      249936 ann   |            6.0 / 23.8 |             5.8 / 9.5 |             5.4 / 9.0 |             5.3 / 8.6
b050      249936 count |           32.0 / 75.6 |           28.0 / 32.6 | 3449.3 / 3502.4 (n=8) |           69.9 / 72.5
```

What it shows:

1. **The fence works through RLS.** Every form returned the same rows. The question is never "RLS or pg_rebac": a
   policy still needs an indexed answer to "may this principal view this container", and the projection is that
   answer.
2. **A policy is planned as a filter, not a join.** Postgres keeps a policy's quals on the scan (security-barrier
   quals are not pulled up into joins), so both the `rls, set` and the `rls, exists` policies become a hashed subplan
   over the reader's containers, probed per row (`EXPLAIN ANALYZE`, below). The query fence is inlined into a
   semi-join instead, which can drive from the reader's containers through `passage (tenant, container)`.
3. **For ANN top-k, the policy forms cost about the same.** At 5% breadth and up every form was within a few
   milliseconds of the query fence (17 to 25 ms at 5%, 5 to 6 ms at 50%). At narrow breadth the hashed probe is
   cheaper than the fence's per-candidate index probe (0.1%: 49 to 52 ms against 122 ms; 1%: 32 to 37 ms against
   56 ms), although the exact path (6.7 ms at 0.1%) is the right one there either way. The per-row function is the
   exception: 423 ms at 0.1% and 68 ms at 1%, because HNSW hands it up to 20,000 candidates and each is a function call.
4. **For anything that reads many rows, the plan shape decides.** Counting a 0.1% reader's 501 rows took 0.6 ms
   with the query fence (driven from its 45 containers), 17 ms through `rebac.enable_rls` and 46 ms through the
   `exists` policy (a scan of all 500,000 rows with a hash probe each), and 3.4 s through the per-row function,
   whatever the reader's breadth (one PL/pgSQL call per row, about 7 µs each). At 50% the query fence took 32 ms, the
   set policy 28 ms, the `exists` policy 70 ms and the per-row function 3.4 s.

Plans (one query each, `EXPLAIN (ANALYZE)`):

```
query fence, count, 0.1% reader:  Nested Loop > Index Only Scan readable_pkey (45 containers)
                                   > Index Only Scan passage_container_idx (11 rows each)        2.2 ms
rls, set, count, 0.1% reader:     Parallel Seq Scan passage, Filter: container = ANY (hashed SubPlan)
                                   (Rows Removed by Filter: 166,456 per worker)                  17.8 ms
rls, exists, count, 0.1% reader:  Seq Scan passage, Filter: container = ANY (hashed SubPlan)
                                   (Rows Removed by Filter: 499,367)                             46.2 ms
query fence, ann, 0.1% reader:    Nested Loop > Index Scan passage_embedding_idx (20,298 candidates)
                                   > Index Only Scan readable_pkey per candidate                 151.6 ms
rls, set, ann, 0.1% reader:       Index Scan passage_embedding_idx, Filter: hashed SubPlan
                                   (Rows Removed by Filter: 20,284)                              66.9 ms
```

### Filtering after retrieval

`--overfetch` fetches the unfenced top N by HNSW (`ef_search = min(N, 1000)`, `iterative_scan = strict_order`, so
N rows come back), keeps the reader's rows and returns the first 20. Every one of the N rows is fetched, as an
application filtering a vector store's results must (the outer sort is on `distance + 0`, so the executor cannot stop
at the 20th readable row). rows/k is over 100 queries; the last line of each reader is the fenced query with
`relaxed_order`, for comparison.

```
p50 ms (rows/k) of fetch-N-then-filter, and the fenced query (relaxed_order)
reader        |         N=20         N=40        N=100        N=200        N=400       N=1000       N=2000       N=4000 |        fence
b0001 0.1%    |   2.2 (0.00)   3.3 (0.00)   6.2 (0.00)  10.7 (0.00)  19.5 (0.01)  44.3 (0.03)  56.9 (0.06)  73.7 (0.13) | 114.4 (0.74)
b001 1.1%     |   2.3 (0.01)   3.5 (0.02)   6.7 (0.05)  11.3 (0.10)  19.2 (0.20)  44.7 (0.52)  52.3 (0.94)  70.3 (1.00) |  42.9 (1.00)
b005 5.1%     |   2.2 (0.06)   3.3 (0.12)   6.7 (0.28)  11.0 (0.54)  20.1 (0.94)  43.2 (1.00)  53.6 (1.00)  70.4 (1.00) |  18.9 (1.00)
b010 10%      |   2.2 (0.11)   3.7 (0.21)   6.6 (0.49)  11.3 (0.91)  19.9 (1.00)  40.8 (1.00)  48.5 (1.00)  70.4 (1.00) |  13.0 (1.00)
b020 20%      |   2.9 (0.22)   4.5 (0.42)   7.3 (0.92)  12.9 (1.00)  21.0 (1.00)  45.1 (1.00)  51.1 (1.00)  74.5 (1.00) |   6.5 (1.00)
b050 50%      |   1.9 (0.49)   4.1 (0.92)   6.7 (1.00)  12.8 (1.00)  22.2 (1.00)  44.6 (1.00)  58.1 (1.00)  68.1 (1.00) |   4.9 (1.00)
p99 ms of fetch-N-then-filter, all readers pooled by N (max over readers): N=20 4, N=40 20, N=100 13, N=200 21, N=400 40, N=1000 94, N=2000 85, N=4000 115
```

What it shows:

1. **Rows kept are about N x breadth / k**, as arithmetic says. With N = k = 20 a 5% reader kept 6% of k, a 10%
   reader 11%; N = 40 (pgvector's default `ef_search`) roughly doubles that. The 128-dimension run's 8%, 20% and 62%
   at 5%, 10% and 30% breadth are this effect inside the database: with `iterative_scan = off` the HNSW scan yields at
   most `ef_search` = 40 candidates before the fence, so about 40 x breadth of them survive.
2. **Filling k needs N of about k / breadth, and the cost grows with N.** Fetching N cost about 2 ms at N = 20, 11 to 13 ms at 200, 19 to 22 ms at 400,
   41 to 45 ms at 1,000 and 68 to 75 ms at 4,000, whoever the reader. The smallest N that filled k was about 100 at
   50% breadth (6.7 ms, against 4.9 ms fenced), 200 at 20% (12.9 against 6.5), 400 at 10% (19.9 against 13.0),
   1,000 at 5% (43.2 against 18.9) and 4,000 at 1% (70.3 against 42.9). And the application must know each reader's
   breadth to choose N: a fixed N either wastes the broad readers' time or silently starves the narrow ones.
3. **Narrow readers have no N that works.** At 0.1% breadth, N = 4,000 kept 13% of k (73.7 ms). Filling k would take
   N of about 20,000, beyond `ef_search`'s maximum of 1,000, so it rests on iterative scanning anyway. The fenced
   ANN leg is no answer there either (114 ms, 0.74 of k); the exact path is (6.7 ms with full recall in the second
   run), which `rebac.fenced_ids` (0.2.0, unreleased) chooses by itself.

Limits: the same as the second run (synthetic embeddings, a warm cache, one node and tenant, `fsync = off`, one
client), plus: the over-fetch rows were filtered inside Postgres, so no network transfer of N rows to an application
is counted, which flatters it; and the over-fetched rows are HNSW's approximate top N, so their recall is not
measured here.

## Recommendations

- Fence an ANN leg with `container in (select * from rebac.readable_set(p))` and `set local hnsw.iterative_scan =
  relaxed_order`.
- Route readers below a few thousand candidate rows to the exact path, as
  `container = any(array(select * from rebac.readable_set(p)))`, using `rebac.breadth(p)`.
- From 0.2.0 (unreleased), `rebac.fenced_ids(p, tbl, q, k)` routes by itself: it counts the reader's candidate rows
  up to `exact_below_rows` (default 5,000) and sorts them exactly below it, or runs the fenced HNSW scan with
  `relaxed_order` above it. Measured on the 128-dimension data (k = 20, p50):

  | Candidate rows | Exact | ANN | ANN recall |
  |---|---|---|---|
  | 500 | 4.9 ms | 85.7 ms | 0.60 |
  | 2,000 | 8.3 ms | 25.1 ms | 0.80 |
  | 5,000 | 14.2 ms | 10.8 ms | 0.92 |
  | 10,000 | 20.9 ms | 6.1 ms | 0.89 |

  The crossover is 4,000 to 5,000 candidate rows; the 768-dimension run crosses at about 5,000 too.
- Use custom plans (`plan_cache_mode = force_custom_plan`) for fenced reads, so each plan sees its own reader.
- Not settled here, and still to measure: a planner support function (C mode) that supplies the per-principal
  selectivity, so the planner can choose the path itself; and Postgres 18. (Skewed containers are covered by the 768-dimension run above.)
