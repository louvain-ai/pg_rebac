# What pg_fence guarantees, and what it does not

## Guarantees

1. **G1, the fence precedes the read.** Every row a query fenced by `fence.readable_set` returns lies in a container
   that `fence.readable` holds for (tenant, principal), at the snapshot the query reads. No row is returned unless
   `fence.checked` has passed for that query. (A query that is empty for another reason may return empty without
   evaluating it.)
2. **G2, a revocation is in force at commit.** A view revocation written through `fence.write` is absent from
   `fence.readable` for every affected principal when the writing transaction commits, whether or not SpiceDB or the
   daemon is up. That covers direct relations, arrows, the relations reached through an arrow, and memberships of
   subject sets. If a principal's lock cannot be taken within `narrow_lock_timeout_ms`, the COMMIT fails; it never
   commits an unnarrowed revocation.
3. **G3, no regrowth.** No refresh can restore a container whose revocation is pending in the outbox. No refresh can
   restore anything for a retired principal.
4. **G4, at least as fresh.** A refresh queued by a delivery is looked up at `at_least_as_fresh` the token SpiceDB
   wrote that delivery at. A first read, and anything with no token, is looked up fully consistent.
5. **G5, tenant isolation.** Every key carries the tenant. A tuple whose ids carry another tenant's prefix is refused
   by `fence.write`, maps to nobody, and is dropped from a lookup.
6. **G6, fail closed.**
   - An unknown principal is `FN404`.
   - A missing tenant is `FN400`.
   - A stale daemon, an unavailable lookup and an over-cap set are each `FN503`.
   - None of these is an empty set, or a wider one.

## Not guaranteed

- **Grants are eventually consistent.** A grant shows up after relay delivery and a refresh. Until then a reader is
  briefly narrower, which is the one direction a fence may err.
- **Writes not made through `fence.write`** (another service, the `zed` CLI) are counted on Watch and narrowed by the
  sweep only. G2 does not cover them.
- **The "still held" rule** reads the outbox's net state, so it is exact only when every view relation is written
  through `fence.write`.
- **Caveats, expiring relationships and exclusion** are refused at start, not evaluated.
- **A read replica** holds G2 only once its replay has passed the commit.

## Where slice 1 differs from the design document

- **The planner-shaped primitive is the set, not the boolean.** Postgres never inlines a scalar SQL function whose
  body has a sub-select, so `readable_by(p, c)` written as `exists (...)` is a per-row function call. Measured with
  `EXPLAIN (VERBOSE)`, `container in (select * from fence.readable_set(p))` is inlined into a semi-join.
  `readable_by` remains, as a per-row check.
- **The principal's check is a pseudo-constant qual.** It is evaluated once, in a gating filter. The principal is
  compared as the caller's own value, so the planner sees its statistics.
- **Staleness fails closed by default** (5 s), as the owner decided. Revocations through the outbox never wait on it.
- **Consistency is `at_least_as_fresh`** by default, as the owner decided. `fully_consistent` is a setting.
- **Subject sets, one level, are supported in slice 1.** The first production schema uses them.
