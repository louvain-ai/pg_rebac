# Changelog

## Unreleased (0.2.0)

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
