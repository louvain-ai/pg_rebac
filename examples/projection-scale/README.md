# Projection scale: 0.2's per-principal rows against 0.3's grant nodes

The results and what they show are in [`docs/design/projection-scale.md`](../../docs/design/projection-scale.md).

- `model.sql`: the tenant. 100,000 containers; teams of 20, departments of 500, an all-company group and a broad group,
  each granted containers; 20 direct shares per person. UUID-length ids.
- `load_v02.sql`, `load_v03.sql`: the rows each version's `rebac.project` writes for that tenant, loaded in bulk.
  0.2's is installed from the `v0.2.0` tag, 0.3's from `sql/rebac.sql`.
- `passages.sql`: 200,000 passages of 768 dimensions with an HNSW index, built once into a template database.
- `bench.py`: per principal count and layout, storage, fenced query latency (`in` with relaxed ANN, `= any` exact,
  `rebac.fenced_ids`), recall against the exact answer, and `rebac.project` for an unchanged set.
- `report.py`: the markdown tables, from the `results-*.json` files `bench.py` writes.

```sh
docker run -d --name bench-pg -e POSTGRES_PASSWORD=rebac -p 127.0.0.1:55644:5432 --shm-size=2g \
  pgvector/pgvector:pg17 postgres -c shared_buffers=4GB -c maintenance_work_mem=1500MB \
  -c max_parallel_maintenance_workers=7 -c fsync=off -c synchronous_commit=off -c jit=off
export REBAC_BENCH_URL=postgres://postgres:rebac@127.0.0.1:55644
python3 bench.py --principals 10000,100000
python3 bench.py --principals 1000000 --layouts v03
python3 report.py
```

The sweep against SpiceDB is `crates/rebacd/examples/sweep_bench.rs`.
