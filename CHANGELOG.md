# Changelog

## 0.3.0

- **Reads stay closed through a Watch replay (fix).** In 0.2.0 a reconnected Watch stream counted as current, and
  the heartbeat resumed, after its first replayed message, so reads reopened while revocations made during the outage
  were still being applied. The daemon now reads SpiceDB's head revision when the stream opens (a fully consistent
  CheckPermission, read-only) and counts the stream current only once its position reaches it. `watch_marker` (new,
  off) waits for a marker relationship instead, for a datastore whose revisions cannot be compared.
- **`POST /lookup` and `GET /status` can require a bearer token** (`lookup_token_env`, the NAME of the variable that
  holds it). A lookup's reply names how many containers a principal reads, and any caller could make the daemon ask
  SpiceDB; with a token set, anything else is a 401. A non-loopback `listen` without a token logs a warning.

## 0.2.0

- **Benchmarks: the alternatives.** `examples/hybrid-pgvector-realistic/alternatives.py` measures row-level security
  (the fence in a policy, a naive `exists` policy, a per-row check function) and filtering after retrieval (fetch the
  unfenced top N, filter, keep 20) on the 768-dimension data. Results and method are in `docs/planner.md`.
- **Revocations made outside `rebac.write` are applied from Watch (G10).** Watch was observe-only; a delete written
  straight to SpiceDB (another service, the `zed` CLI) waited for the sweep (15 minutes) or the freshness bound
  (one hour). `rebacd` now applies each Watch message in one transaction with its position (`rebac.apply_watch`):
  revocations are narrowed at once under the commit's lock order, arrow tuples and set grants are mirrored, and
  everyone affected is refreshed fully consistent. Its own relayed writes are recognised by token and skipped.
- **With Watch on, the heartbeat requires the stream.** While Watch is down or a message cannot be applied, reads fail
  closed (`RB503`) after `max_staleness_ms`. A store that cannot serve Watch needs `watch = false`.
- **A Watch position that cannot be resumed records a gap.** None stored, refused by SpiceDB, or older than
  `watch_resume_max_age_ms` (new, one hour): the stream restarts from head, sets looked up before it are `RB503`
  until re-projected, and the sweep is forced. **Upgrading from 0.1 records one gap on the first start** (0.1's
  position applied nothing): reads are `RB503` until the forced sweep re-projects each principal.
- **`rebac.fenced_ids(p, tbl, q, k, …)`: breadth-routed nearest neighbours.** It counts the reader's candidate rows
  up to `exact_below_rows` (new setting, 5,000) and sorts them exactly below it, or runs the fenced HNSW scan with
  `relaxed_order` above it, so a narrow reader no longer trades recall by accident. Measured crossover: 4,000 to
  5,000 candidate rows.
- **Wildcards on the projected type** (`relation member: user | user:*`). A wildcard grant refreshes every principal
  the tenant knows; its removal narrows every holder of the container at commit (and from Watch), like an arrow's. A
  wildcard on a relation reached through an arrow, or inside a subject set, is still refused.
- **SQL:** `rebac.outbox.origin` (`write` or `watch`), `rebac.watch_state.gap_at`, `rebac.apply_watch`,
  `rebac.watch_restarted`, `rebac.watch_alive`, `rebac.mirror`, `rebac.queue_refreshes`. A refresh queued fully
  consistent (`zedtoken = ''`) stays so when a later enqueue carries a token. The sweep no longer re-sends a
  revocation that a later change seen on Watch superseded.

## 0.1.1

- **The relay's re-narrowing takes no tenant lock.** In 0.1.0 it held the tenant lock shared across its SpiceDB
  writes. A slow SpiceDB could therefore delay an arrow or set-grant revocation past `narrow_lock_timeout_ms` and fail
  its commit.
- **npm: the package is `pg-rebac`** (`npm i pg-rebac`), unscoped.
- **RLS: binding a principal is refused read-only.** `rebac.set_principal` raises `25006` in a read-only transaction
  or on a replica. This is now documented and tested.
- **Benchmarks.** The 128-dimension study's limits are stated beside its numbers. A second run,
  `examples/hybrid-pgvector-realistic/`, uses 768 dimensions with skewed containers and reports p50, p95 and p99
  over 1 to 32 clients.
- **Docs.** Removing an arrow or a set grant, once delivered, is scoped under G3 for `at_least_as_fresh`. The cover
  and README carry the 768-dimension numbers with their recall.

## 0.1.0

The first release. Against the pre-release, it:

- **closes the arrow and set-grant revocation race** with a tenant lock. Every projection writer takes it shared;
  such a revocation takes it exclusively before it reads the holders. Revocations through `rebac.write` must commit
  under READ COMMITTED (`RB422` otherwise);
- **adds a freshness bound.** Each set carries `looked_up_at`, and `max_projection_age_ms` (one hour) fails closed
  on an old or unknown one. The heartbeat is liveness only. It also adds `rebac.freshness()` and more columns in
  `rebac.status()`;
- **re-derives the closure on a live schema change.** A change to what grants view fails reads closed (`RB503`)
  until every tenant is rebuilt (`rebac.closure_state`, `closure_check_ms`);
- **makes the RLS policy `RESTRICTIVE`.** `principal_source => 'session'` with `rebac.set_principal` keeps an
  untrusted role from choosing the principal;
- **makes `rebac.project` write a diff.** In a churn test, WAL per round fell from 395 MB to 8.8 MB;
- **adds CI**: Rust, the SQL suite on Postgres 14 to 17, the race tests and the SpiceDB integration test.
