# A more realistic study of fenced pgvector search

The first study (`../hybrid-pgvector/`) used 128 dimensions, 5,000 equal containers, 10 queries per cell and one
client. This one uses 768 dimensions, Zipf-skewed container sizes, topic-affine containers, 300 queries per cell with
p50/p95/p99, recall against the exact answer over 200 queries, and a concurrency run at 1, 8 and 32 clients.

## Run it

Needs Postgres 17 with pgvector 0.8 or later, `psql` and `pgbench` on PATH, and Python 3.12 or later (no packages).

```
createdb rebac_bench_real
psql -v ON_ERROR_STOP=1 -d rebac_bench_real -f sql/rebac.sql
psql -v ON_ERROR_STOP=1 -d rebac_bench_real -f examples/hybrid-pgvector-realistic/setup.sql
REBAC_BENCH_URL=postgres://user:pass@host:port/rebac_bench_real python3 examples/hybrid-pgvector-realistic/bench.py
```

`setup.sql` takes `-v passages=… -v containers=… -v dims=… -v mwm=4GB -v workers=7` (defaults 500,000 / 20,000 / 768).
It builds the HNSW index with a large `maintenance_work_mem` and parallel workers (shared memory must hold the graph:
raise `--shm-size` for a container). `bench.py` keeps the exact ground truth in `bench_truth` between runs
(`--retruth` recomputes it; the first run spends most of its time there). `--skip-concurrency`, `--tx`, `--seconds`
shorten a run.

`alternatives.py` runs on the same database: `--rls` compares the fence in the query with row-level security
(`rebac.enable_rls`, a naive `exists` policy, a per-row check function), and `--overfetch` measures fetching the
unfenced top N and filtering it. Results and method are in `docs/planner.md`, "The alternatives".

## What is in the data

- Container sizes follow size(r) ~ 1 / (r + 30): the largest holds about 2,500 passages, the median 8, the smallest 4.
- Each container has a home topic; 60% of its passages come from it. Embeddings are a topic centre plus uniform noise
  (500 topics, noise 0.6), so a narrow reader sees only some topics.
- Readers: b0001, b001, b005, b010, b020, b050 read 0.1%, 1%, 5%, 10%, 20% and 50% of the passages (random
  containers until the fraction is reached), plus 2,000 skewed background readers (mean about 6%).
- `rebac.readable` is written with `rebac.project`. The staleness bounds are switched off in this database only,
  because no daemon runs beside it.

Latency is measured by `pgbench` (one transaction is one fenced query, the first 20 dropped), recall and rows/k inside
Postgres. The cache is warm (the 2 GB heap and 2 GB index sit in the OS page cache after setup and the ground-truth pass; the first 20 queries of each cell are dropped).
