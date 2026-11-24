# Changelog

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
