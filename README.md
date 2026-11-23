# pg_fence

Permission-aware SQL for Postgres. SpiceDB stays the authority for who may read what. pg_fence keeps a local,
indexed projection of one permission, and gives every query the same fence:

```sql
set local fence.tenant = 'acme';
set local hnsw.iterative_scan = relaxed_order;   -- see "Vector search" below
select id from passage
where container in (select * from fence.readable_set('priya'))
order by embedding <=> $1
limit 20;
```

Postgres inlines `fence.readable_set` into a semi-join on the projection's primary key. So the fence sits inside the
same plan as a pgvector ordering, a full-text match or anything else, and applies before the order and the limit.

**Status:** slice 1, pre-release. It has the pure-SQL mode, the `fenced` daemon, and the tests and studies below.

## What you get

- **One read primitive.** `fence.readable_set(principal)` is the containers a principal may view, optionally
  `within` a list (an intersection, which can only narrow). `fence.readable_by(principal, container)` is a per-row
  check, for single lookups.
- **Three different answers.**
  - `FN404` means the principal was never asked: call the daemon's `POST /lookup`, then retry.
  - An empty set means the principal was asked and can read nothing.
  - `FN503` means unavailable: the projection is stale, over its cap, or SpiceDB could not be asked.
  - A missing `fence.tenant` is `FN400`. None of these is ever an empty set by accident.
- **Revocations in force at commit.** Write relationships with `fence.write(op, resource, relation, subject)` in
  your own transaction. A deferred trigger removes every revocation it queued from the projection before the
  transaction commits, whether or not SpiceDB or the daemon is up.
- **A staleness bound.** Reads fail closed (`FN503`) once the daemon has not heartbeated for `max_staleness_ms`
  (default 5000). The daemon beats only while it can reach both SpiceDB and Postgres.
- **The daemon, `fenced`.** It:
  - relays the outbox to SpiceDB in order, revocations first, without letting one tenant wait behind another, and
    quarantines a row SpiceDB refuses on its own;
  - refreshes principals at least as fresh as the write that queued them;
  - backfills a tenant from ReadRelationships and LookupResources;
  - re-sends each recent revocation once from a sweep;
  - follows Watch to count writes made outside pg_fence (observe-only in slice 1);
  - derives the relations that can change the permission from the SpiceDB schema itself, and refuses a schema it
    cannot narrow soundly.
- **`fenced verify`.** It diffs the projection against SpiceDB, across tenants, against the schema's closure, or
  against a shadow projection. It reports extra rows (a violation, exit 2) and missing rows (freshness) separately,
  as counts only.

## Quickstart

```sh
# 1. The schema, on any Postgres 14+ (no extension needed):
psql -v ON_ERROR_STOP=1 -f sql/fence.sql

# 2. The daemon:
cargo build --release -p fenced
DATABASE_URL=postgres://… SPICEDB_TOKEN=… ./target/release/fenced --config fence.toml run
```

`fence.toml` holds connections and tuning only (see `crates/fenced/src/config.rs`). What is projected (the resource
type, the permission, the tenant separator) lives in the `fence.config` table, which the SQL and the daemon both read.

## Supported schemas (slice 1)

The projected permission, `container#view` by default, may be any union (`+`) of:

- direct relations;
- a self-arrow (`parent->view`);
- an arrow to another type and a relation there (`workspace->admin`);
- subject sets one level deep (`group#member`, where `group#member` holds plain object types).

Refused, each with its reason:

- exclusion (`-`): removing a tuple can grant;
- intersection (`&`, `.all()`);
- caveats and expiring relationships;
- wildcards;
- nested subject sets;
- arrows more than one hop away.

## Vector search

[`docs/planner.md`](docs/planner.md) measures a fenced HNSW ordering at eight reader breadths, with 500,000 passages
and pgvector 0.8:

- Readers of 5% to 50% breadth get all k rows at recall 0.90 to 0.96 in 4 to 7 ms, provided
  `hnsw.iterative_scan = relaxed_order` is set.
- Without it, the fence silently returns fewer than k rows.
- Readers below about 1% breadth belong on the exact path, `container = any(array(select * from
  fence.readable_set(p)))`. Choose it with `fence.breadth(p)`.

## Tests

```sh
docker compose up -d --wait                           # Postgres (pgvector) on :55433, SpiceDB on :50061
FENCE_TEST_URL=postgres://postgres:fence@127.0.0.1:55433 tests/sql/run.sh
cargo test && cargo clippy --all-targets
FENCE_IT=1 cargo test -p fenced --test integration    # the daemon against the real SpiceDB
```

## Licence

Apache-2.0. See `LICENSE` and `NOTICE`. The vendored SpiceDB API protos are Apache-2.0 (Authzed); see
`crates/fenced/proto/LICENSE-authzed-api`.
