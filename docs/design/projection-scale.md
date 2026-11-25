# Projection scale: grant nodes

Status: implemented in 0.3.0 (`node_grants = 'on'`, the default). [`guarantees.md`](../guarantees.md) states what
the code guarantees; this page is why it is built this way, and what it measured.

## The problem

0.2 stored one row per (tenant, principal, container) the principal may view. A grant to a group was expanded once
per member, and a grant to everyone once per principal. The number of rows was people times the containers each one
reads, and a row with UUID-length ids costs 260 to 300 bytes with its two indexes. At 100,000 people reading 1% of
1,000,000 containers that is 10⁹ rows, 260 to 300 GB, and every reader is at `max_containers_per_principal`
(100,000), so reads fail closed with `over_cap`. Grants to people were never the cost. Grants to groups and to
everyone, multiplied out per person, were.

## The design

A grant to a subject set (`container#member@group:eng#member`) or to a wildcard (`container#member@user:*`) is held
by a **grant node**, stored once:

- A node is a holder in `rebac.readable` whose name starts with `#`: `#group:eng#member`, `#user:*`. It has a row in
  `rebac.principal_state` (state, lookup time, cap) and takes the principal lock order. A principal may not be named
  that way, and `rebac.checked` refuses one.
- A node's rows come from LookupResources with the set itself as the subject, so they are exactly what SpiceDB says
  the set may view, through self-arrows and via relations included. SpiceDB cannot look up `user:*`, so a wildcard
  node is looked up as a stand-in id (`wildcard_probe`) and intersected with the mirrored wildcard grants and what is
  under them; a grant made to the stand-in itself never reaches anyone.
- A principal's memberships come from its own refresh: one LookupResources per subject-set relation of the closure
  (`group#member`), stored in `rebac.node_member`. A wildcard node needs no rows: every principal with a subject of
  its type stands for it.
- A principal stores only the containers none of its nodes holds. Its lookup still returns everything it reads
  (SpiceDB does not say through which path), so the subtraction is done against the nodes' rows as they are when it
  refreshes. Nothing relies on it being complete: a container stored both ways is read once and narrowed by every
  revocation.

### The read path

`rebac.readable_set(p)` reads one table:

```sql
select r.container from rebac.readable r
where (select rebac.checked(p)) and r.tenant = (select rebac.tenant())
  and r.principal = any((select rebac.holders_of((select rebac.tenant()), p))::text[])
```

`rebac.holders_of(t, p)` is the principal and its projected nodes, a short array computed once per query in an
InitPlan. A plan with the HNSW index outside therefore probes `readable_pkey` once per candidate, as in 0.2, with
`principal = ANY(holders)` in the index condition. A two-table form (the principal's rows
`union` the nodes' rows) was built first and measured: the planner turned it into a hash join over a sequential scan
of the passages at 5% to 30% breadth, where the single table keeps the probe.

The planner estimates a principal's rows from the average holder, because the holders are an InitPlan value it cannot
see while planning. A reader who is broad through one large node can therefore get the exact plan from the `in` form.
`rebac.fenced_ids` counts the reader's candidate rows itself and, above `exact_below_rows`, writes its HNSW leg as a
probe per candidate (a sub-select with `limit 1`, which the planner cannot turn into a join), so the HNSW scan stays
outside whatever the estimate. The measurements below are through both forms.

### Freshness and failing closed

`rebac.checked` reads the principal's state and its nodes' in one query. The set is as old as the oldest lookup
behind it, and the freshness bound, the Watch gap, the Watch grace and the closure's last change are all checked
against that time. A node that is unavailable (over `max_containers_per_node`, one million) fails every member with
`RB503`, never narrower or wider. `POST /lookup` re-projects a principal and any node a read would refuse.

### Narrowing at commit (G2)

Every removal narrows nodes in the same transaction, under the same lock order: tenant, then principals and nodes
sorted together.

- **A membership** (`group:eng#member@user:ana`) deletes the `node_member` row, and ana's own copies of what the set
  holds. A copy is found by walking each of her rows up its self-arrows, and one hop through an arrow to another type,
  to the set's node rows or a mirrored grant to the set. The copies are rows she stored before the node held them, or
  rows 0.2 wrote.
- **A grant to a set** names the node (`rebac.principals_of` maps `group:acme/eng#member` to `#group:eng#member`), so
  it narrows the node's rows, and their self-arrow closure, like a direct revocation. A grant through a via relation
  (`workspace#admin@group:ops#member`) narrows the containers under the workspace the same way.
- **A wildcard grant and an arrow tuple** narrow every holder of the container, nodes included, under the tenant held
  exclusively, as in 0.2.
- **Every node that loses a container** takes its members' own copies of it with it.

A removal that names nobody records whom it narrowed on its outbox row (`narrowed`, and `narrowed_containers` for what
nodes lost). Its delivery re-projects them at the delete's token, so a principal who also reads a container another
way gets it back at once rather than at the next sweep.

`tests/sql/19_node_races.sql` races a node's refresh against a set-grant revocation, and a member's refresh against a
membership revocation, in both orders. `crates/rebacd/tests/oracle.rs` compares every principal's set with SpiceDB's
LookupResources over randomised schemas and relationships, at each revocation's commit (never wider) and after the
queues drain (equal). It found one bug that 0.2 also had: leaving a set granted through a via relation was not
narrowed at commit, because `set_grant` mirrored only grants on the projected type. Such grants are now mirrored with
their object typed (`workspace:w1`).

### Grants

A grant to a set refreshes its node only (one lookup), not every member. A wildcard grant refreshes `#user:*`, not
everyone the tenant knows. A membership refreshes the member. An arrow tuple asks SpiceDB who views the resource,
LookupSubjects for users and, per subject-set relation, for sets, and refreshes both.

## Measured

**Hardware.** Apple M1 Max (10 cores, 64 GB), macOS; Docker Desktop with 10 CPUs and 16 GB; Postgres 17 with
pgvector 0.8 (`shared_buffers` 4 GB, `jit` off, `fsync` off for loading speed); SpiceDB 1.56.2 with the in-memory
datastore. Everything ran on one machine, so SpiceDB and Postgres shared its cores.

**The tenant** (`examples/projection-scale/model.sql`). 100,000 containers with UUID-length ids. Teams of 20 people,
each team granted 5 containers; departments of 500, each granted 100; an all-company group granted 1,000 (1%); 20
direct shares per person, spread at random; one person in 10,000 is also in a group granted 20,000 containers (20%).
A typical reader reads 1,125 containers, a broad one about 21,000. 200,000 passages (two per container) of 768
dimensions, with an HNSW index (defaults). "0.2" is the v0.2.0 SQL with every grant expanded per person, as its
`rebac.project` wrote it; "0.3" is this version's layout, as its `rebac.project` writes it.

**Storage** (`rebac.readable`, `rebac.node_member` and `rebac.principal_state`, with their indexes, freshly loaded).
0.2 at a million people was not loaded; its row is the 100,000-person row per principal, times ten. Size per
principal falls as people are added in 0.3, because the groups' rows are shared:

| Principals | Layout | Rows in `rebac.readable` | of them grant nodes' | Memberships | Size (tables and indexes) | Per principal |
|---|---|---|---|---|---|---|
| 10,000 | 0.2 | 11,267,769 | 0 | 0 | 2.7 GB | 270.0 kB |
| 10,000 | 0.3 | 174,369 | 25,500 | 30,001 | 60 MB | 6.0 kB |
| 100,000 | 0.2 | 112,677,957 | 0 | 0 | 26.9 GB | 269.3 kB |
| 100,000 | 0.3 | 746,531 | 66,000 | 300,010 | 302 MB | 3.0 kB |
| 1,000,000 | 0.3 | 6,273,922 | 471,000 | 3,000,100 | 2.7 GB | 2.7 kB |
| 1,000,000 | 0.2 | about 1.13 × 10⁹, extrapolated | 0 | 0 | about 269 GB, extrapolated | 269 kB |

**Fenced vector search** (one client, 300 queries per cell after 30 to warm, recall@20 against the exact answer over
the reader's own passages). "Typical" is a reader of about 1,125 containers, "broad" one of about 21,000; "exact plan"
marks where the planner chose a sort over the reader's rows rather than the HNSW scan outside:

| Principals | Layout | Reader | Breadth | `in`, relaxed ANN p50 / p95 | `= any`, exact p50 / p95 | `rebac.fenced_ids` p50 / p95 (path, recall) |
|---|---|---|---|---|---|---|
| 10,000 | 0.2 | typical | 1,125 | 22.2 / 38.0 ms (exact plan) | 17.0 / 27.2 ms | 15.7 / 21.3 ms (exact, 1.0) |
| 10,000 | 0.2 | broad | 21,124 | 3.2 / 35.8 ms | 167.8 / 255.8 ms | 22.6 / 29.8 ms (ann, 1.0) |
| 10,000 | 0.3 | typical | 1,116 | 18.6 / 25.1 ms (exact plan) | 16.7 / 27.5 ms | 17.1 / 21.6 ms (exact, 1.0) |
| 10,000 | 0.3 | broad | 21,123 | 321.1 / 427.1 ms (exact plan) | 220.2 / 288.6 ms | 25.7 / 51.2 ms (ann, 1.0) |
| 100,000 | 0.2 | typical | 1,125 | 24.8 / 36.4 ms (exact plan) | 17.8 / 26.6 ms | 17.5 / 42.5 ms (exact, 1.0) |
| 100,000 | 0.2 | broad | 21,124 | 87.2 / 131.7 ms (exact plan) | 165.9 / 187.4 ms | 21.2 / 110.1 ms (ann, 0.975) |
| 100,000 | 0.3 | typical | 1,112 | 15.2 / 17.7 ms (exact plan) | 13.8 / 16.4 ms | 13.7 / 14.9 ms (exact, 1.0) |
| 100,000 | 0.3 | broad | 21,108 | 283.1 / 329.4 ms (exact plan) | 195.2 / 208.2 ms | 20.1 / 32.1 ms (ann, 0.965) |
| 1,000,000 | 0.3 | typical | 1,112 | 18.8 / 26.6 ms (exact plan) | 15.9 / 18.2 ms | 14.6 / 29.1 ms (exact, 1.0) |
| 1,000,000 | 0.3 | broad | 21,108 | 284.3 / 361.3 ms (exact plan) | 199.3 / 227.8 ms | 20.3 / 56.6 ms (ann, 0.978) |

`rebac.fenced_ids` is within a few milliseconds of 0.2 for both readers: at 100,000 people 0.3 is faster (broad
reader 20.1 against 21.2 ms p50, 32 against 110 ms p95), at 10,000 slower (25.7 against 22.6 ms p50, 51 against
30 ms p95). On the rows 0.2 left, before the sweep re-projects them, it measured 14.4 ms for the broad reader under
both versions' SQL (10,000 people, 200 queries each). The `in` form is
as fast for the typical reader. For the broad reader it is slower: 0.3 reads its breadth through a node the planner
cannot see while planning ([the read path](#the-read-path)), and gets the exact plan; 0.2 got the HNSW plan at 10,000
people from the principal's most-common-value statistics, and lost it at 100,000.

**A refresh, on the database side** (`rebac.project` for an unchanged set, 200 principals, the sweep's usual case):

| Principals | Layout | `rebac.project`, unchanged set, p50 / p95 |
|---|---|---|
| 10,000 | 0.2 | 11.5 / 15.3 ms |
| 10,000 | 0.3 | 13.2 / 16.5 ms |
| 100,000 | 0.2 | 83.7 / 139.9 ms |
| 100,000 | 0.3 | 15.7 / 19.2 ms |
| 1,000,000 | 0.3 | 100.7 / 107.8 ms |

0.2 at 100,000 people reads 27 GB that no longer fits in `shared_buffers`. 0.3 at a million took 101 ms; where that
time goes was not measured.

**The sweep, against SpiceDB** (`crates/rebacd/examples/sweep_bench.rs`; this run used the stock Postgres of
`docker-compose.yml`, not the tuned one above). The same tenant at 2,000 principals,
written to SpiceDB (67,897 relationships), backfilled, then one full sweep worked by the daemon's own refresh pass at
`refresh_concurrency` 4, with grant nodes off (0.2's per-principal expansion) and on:

| | `node_grants = 'off'` | `node_grants = 'on'` |
|---|---|---|
| Backfill | 141 s | 79 s |
| One sweep | 2,000 lookups in 93 s, 22 per second | 2,105 lookups (2,000 principals, 105 nodes) in 100 s, 21 per second |
| `rebac.readable` after it | 2,269,554 rows, 759 MB | 61,414 rows, 24 MB, and 5,995 memberships |

A sweep is bound by SpiceDB answering each principal's LookupResources, and grant nodes do not change that: each
principal's lookup still returns its whole set, and the membership lookups and the node lookups are added. At 21 a
second on this machine, a sweep of 100,000 principals takes about 80 minutes, against the 15-minute default interval
(the design's estimate: each lookup within 36 ms). The backfill is faster because a principal writes 30 rows rather
than 1,125. Revocations do not wait on the sweep (G2 at commit, G10 from Watch), so at this scale raise
`sweep_interval_ms`, and `max_projection_age_ms` above it, to what SpiceDB can serve.

On the database side alone, `rebac.project` for an unchanged set (a sweep's usual case: read, subtract, compare,
write nothing) took 11.5 ms p50 in 0.2 and 13.2 ms in 0.3 at 10,000 principals; 0.3 also records memberships and
subtracts what the nodes hold.

## What it does not change

- **A principal's lookup.** A refresh still asks SpiceDB for everything the principal reads, plus one membership
  lookup per subject-set relation, because LookupResources does not say through which path a container is held. A
  sweep costs SpiceDB what it did in 0.2, and a little more. What grant nodes remove is the fan-out of a grant (one
  node lookup in place of one per member) and the rows.
- **The `in` form's estimate for a reader who is broad through a node.** The planner sees no holders while planning
  (they are an InitPlan value), so it cannot route that reader to the HNSW plan; `rebac.fenced_ids` routes by a
  count instead. The path to fixing the `in` form itself is a planner support function (`SUPPORT`, in C, for the
  extension build) that returns the reader's row estimate from `rebac.principal_state`.
- **Promotion by size.** Every set and wildcard a grant names becomes a node, however small. A threshold (expand a
  set per member below N members times M containers) is not built: a node of 20 people costs a few rows more than
  their expansion, and none of the measurements needed it.
- **Arrow targets as nodes.** `workspace:w1#admin` is still expanded per admin. It is a set in all but name, and the
  same machinery would hold it once; not built.
- **Nested sets and longer arrows** are still refused by the closure ([`schema-depth.md`](schema-depth.md)).

## Reproduce it

```sh
docker run -d --name bench-pg -e POSTGRES_PASSWORD=rebac -p 127.0.0.1:55644:5432 --shm-size=2g \
  pgvector/pgvector:pg17 postgres -c shared_buffers=4GB -c maintenance_work_mem=1500MB \
  -c max_parallel_maintenance_workers=7 -c fsync=off -c synchronous_commit=off -c jit=off
REBAC_BENCH_URL=postgres://postgres:rebac@127.0.0.1:55644 \
  python3 examples/projection-scale/bench.py --principals 10000,100000
REBAC_BENCH_URL=postgres://postgres:rebac@127.0.0.1:55644 \
  python3 examples/projection-scale/bench.py --principals 1000000 --layouts v03

docker run -d -p 127.0.0.1:50262:50051 authzed/spicedb:v1.56.2 serve --grpc-preshared-key rebac-test-key \
  --datastore-engine memory
REBAC_BENCH_PG=postgres://postgres:rebac@127.0.0.1:55644 \
  cargo run --release -p rebacd --example sweep_bench -- 2000 4     # principals, refresh_concurrency
```
