---
title: Performance
description: The planner study - a fenced HNSW ordering at eight reader breadths over 500,000 passages, with its method.
---

The design's largest risk was that an inlined fence next to an HNSW scan would pick a bad plan for readers of
middling breadth. This study measures it. The numbers below are from one machine and one synthetic data set; treat
them as the shape of the trade-off, and measure your own.

## Limits

The numbers below come from synthetic, clustered 128-dimension embeddings with a warm cache. Each cell is the median
of 10 queries from one client, with no p95 or p99 and no concurrency, on one 4-CPU machine. Treat them as the shape
of the trade-off, not a capacity figure.

## Method

- Postgres 17, pgvector 0.8.6, 4 CPUs, 6 GB, `shared_buffers = 1GB`, JIT off. Everything cached in memory.
- 500,000 passages in 5,000 containers (100 passages each, spread through the heap), 128 dimensions.
- Embeddings are 200 clustered topics, each passage a topic centre plus noise; queries are near a topic. Topic and
  container are independent.
- HNSW at the defaults (`m = 16`, `ef_construction = 64`, `ef_search = 40`), cosine distance.
- `rebac.readable` holds 875,000 rows: eight named readers at fixed breadths, and 2,000 background readers with
  skewed breadths (most read a little, a few a lot), so the statistics look like a real tenant's.
- k = 20, 10 queries per cell. "rows/k" is the fraction of k rows returned; "recall" is recall@20 against the exact
  answer over the reader's own passages.

## Results

The unfenced HNSW baseline was 1.9 ms at p50. Each cell is p50 latency in ms / rows/k / recall:

| Breadth | Passages | ANN, iterative off | ANN, relaxed_order | Exact (array) | Routed by breadth |
|---|---|---|---|---|---|
| 0.1% | 500 | 5.7 / 0.01 / 0.01 | 107.4 / 0.88 / 0.54 | 1.6 / 1.00 / 1.00 | 4.3 / 1.00 / 1.00 |
| 1% | 5,000 | 3.8 / 0.03 / 0.04 | 11.9 / 1.00 / 0.95 | 7.1 / 1.00 / 1.00 | 11.7 / 1.00 / 0.95 |
| 5% | 25,000 | 1.6 / 0.08 / 0.09 | 5.8 / 1.00 / 0.96 | 28.2 / 1.00 / 1.00 | 7.1 / 1.00 / 0.96 |
| 10% | 50,000 | 1.7 / 0.20 / 0.20 | 6.7 / 1.00 / 0.95 | 54.8 / 1.00 / 1.00 | 6.3 / 1.00 / 0.95 |
| 20% | 100,000 | 1.6 / 0.34 / 0.34 | 5.1 / 1.00 / 0.91 | 107.7 / 1.00 / 1.00 | 5.1 / 1.00 / 0.91 |
| 30% | 150,000 | 1.7 / 0.62 / 0.62 | 4.7 / 1.00 / 0.95 | 161.3 / 1.00 / 1.00 | 4.3 / 1.00 / 0.95 |
| 50% | 250,000 | 1.8 / 0.92 / 0.83 | 4.5 / 1.00 / 0.90 | 263.8 / 1.00 / 1.00 | 4.2 / 1.00 / 0.90 |
| 100% | 500,000 | 1.6 / 1.00 / 0.84 | 3.5 / 1.00 / 0.84 | 481.6 / 1.00 / 1.00 | 3.8 / 1.00 / 0.84 |

"Routed by breadth" used the exact path below 50 containers (`rebac.breadth(p)`) and relaxed ANN otherwise. The
full table, with max latency, the `in`-form exact path and `ef_search = 200`, is in the repository's
`docs/planner.md`.

## What it shows

1. **The plan shape is stable.** At every breadth from 0.1% to 100% the planner chose an HNSW index scan outside
   and a `readable_pkey` probe per candidate inside. From 5% to 50% breadth, fenced ANN with `relaxed_order`
   returned all k rows at recall 0.90 to 0.96, in 4 to 7 ms.
2. **Without iterative scans, the fence silently loses rows**: 8% of k at 5% breadth, 20% at 10%, 62% at 30%. No
   error is raised.
3. **Narrow readers need the exact path.** At 0.1% breadth relaxed ANN hit `hnsw.max_scan_tuples` (20,000), took
   about 100 ms and returned 0.88 of k at recall 0.54. The exact path took 1.6 ms with full recall. The crossover
   was about 1% breadth, roughly 5,000 candidate passages.
4. **The planner does not know a reader's breadth.** It estimates every reader at the tenant's average (451
   containers for a reader of 5). Through the `in` form that produced a parallel sequential scan of
   `rebac.readable` at 15 to 25 ms where 1.6 ms was possible. The array form avoids it.
5. **A larger `ef_search`** raised the broad readers' recall (0.84 to 0.97 at 100% breadth) and changed little for
   narrow ones, whose recall is bounded by the scan limit.

## A second run at 768 dimensions

`examples/hybrid-pgvector-realistic/` reruns the study at 768 dimensions over 500,000 passages, with skewed
container sizes (median 8 passages, largest 2,486). It reports p50, p95 and p99 over 300 queries per cell, plus a
concurrency run. The data is still synthetic, with a warm cache on one 10-CPU machine and `fsync` off. The full
table is in `docs/planner.md`.

| Breadth | ANN, relaxed_order p50 / p99 ms | rows/k | Exact (array) p50 ms |
|---|---|---|---|
| 0.1% | 116 / 264 | 0.88 | 6.7 |
| 1% | 48 / 79 | 1.00 | 44 |
| 5% | 29 / 61 | 1.00 | 188 |
| 10% | 18 / 30 | 1.00 | 276 |
| 20% | 6.5 / 13 | 1.00 | 508 |
| 50% | 2.8 / 5.3 | 1.00 | 1,223 |

- **The fence still returns all k rows** from 1% breadth up, and the plan does not change. At 768 dimensions the
  middle breadths are slower than at 128, so the 4 to 7 ms figure applies to the 128-dimension run only.
- **Concurrency.** Fenced reads levelled at about 420 to 450 queries a second from 8 clients, with p99 under
  110 ms. At 32 clients the machine was CPU-bound and p99 reached 330 to 480 ms.
- **Recall is not meaningful here.** On this data unfenced HNSW itself scored 0.09, because isotropic noise in 768
  dimensions leaves neighbours nearly tied. Fenced recall tracked unfenced recall at the same `ef_search`, so the
  fence costs no recall of its own. The data cannot say more than that.

## Write side: a refresh is a diff

A refresh re-projects a principal from SpiceDB. `rebac.project` writes only the difference from the stored set,
deleting the containers that left and inserting the new ones. What stayed is not rewritten, so it costs no dead
tuple, no index entry and no WAL. Measured with `examples/project-churn/churn.sql`:

- **Set-up.** One tenant of 1,000 principals, each reading 5,000 containers (5 million rows). Each round
  re-projects 200 principals with 1% of their containers replaced. Postgres 17 in Docker on a 10-CPU laptop, five
  rounds.
- **Results.** Per round of 200 refreshes:

  | `rebac.project` | Time | WAL | Rows inserted | Rows deleted |
  |---|---|---|---|---|
  | Delete everything, reinsert (before) | 12.1 s | 395 MB | 1,000,000 | 1,000,000 |
  | Diff (now) | 7.5 s | 8.8 MB | 10,000 | 10,000 |

- **The cost that remains.** A refresh still reads the principal's whole set and compares it with the stored one.
  How the 7.5 s divides between that and the writes was not measured separately.

## Not measured yet

- A planner support function supplying per-principal selectivity, so Postgres could choose the path itself.
- Postgres 18.
- Write-side costs of narrowing at commit and of relay throughput.

## Reproduce it

```sh
docker compose up -d --wait                       # Postgres with pgvector on :55433
export DB=postgres://postgres:rebac@127.0.0.1:55433
psql $DB/postgres -c 'create database rebac_bench'
psql -v ON_ERROR_STOP=1 $DB/rebac_bench -f sql/rebac.sql
psql $DB/rebac_bench -v passages=500000 -v containers=5000 -v dims=128 -v principals=2000 \
  -f examples/hybrid-pgvector/setup.sql
REBAC_BENCH_URL=$DB/rebac_bench python3 examples/hybrid-pgvector/bench.py --k 20 --queries 10
```

`examples/hybrid-pgvector/plan_check.sql` asserts the plan shape, and `hybrid.sql` is a vector leg and a full-text
leg, each fenced before its order and limit, fused by reciprocal rank.
