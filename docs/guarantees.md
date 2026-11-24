# What pg_rebac guarantees, and what it does not

## Guarantees

1. **G1, the fence precedes the read.** Every row a query fenced by `rebac.readable_set` returns lies in a container
   that `rebac.readable` holds for (tenant, principal), at the snapshot the query reads. No row is returned unless
   `rebac.checked` has passed for that query. (A query that is empty for another reason may return empty without
   evaluating it.)
2. **G2, a revocation is in force at commit.** A view revocation written through `rebac.write` is absent from
   the projection for every affected principal when the writing transaction commits, whether or not SpiceDB or the
   daemon is up. That holds exactly for:
   - direct relations;
   - the relations reached through an arrow (`workspace->admin`);
   - memberships of subject sets.
   If a lock cannot be taken within `narrow_lock_timeout_ms`, the COMMIT fails; it never commits an unnarrowed
   revocation.

   Two kinds of removal narrow EVERYONE who holds a container: an arrow tuple (a file leaving its space) and a grant to
   a subject set. They name no principal, so the trigger holds the TENANT exclusively before it reads who the holders
   are. Every projection writer (a refresh, a lazy first read, a retirement) holds its tenant shared, then its
   principal, from before it asks SpiceDB until it commits. So a refresh in flight commits before the holders are read
   (and its row is narrowed), and a refresh that starts later waits for the commit (and subtracts the revocation as
   pending). `tests/sql/09_revocation_race.sql` drives both orders. The cost: such a revocation waits for every refresh
   of its tenant in flight, within `narrow_lock_timeout_ms`.

   **G2 needs READ COMMITTED** (the Postgres default). Under REPEATABLE READ or SERIALIZABLE the commit-time delete reads
   the transaction's first snapshot, which cannot see a refresh that committed after it, so `rebac.write` refuses a
   view revocation there with `RB422`. Grants are allowed at any isolation level.

   G2 for arrows, via relations and sets also needs `edge` and `set_grant` to be complete. Every such write must go
   through `write`, or the tenant must have been backfilled.
3. **G3, no regrowth.** No refresh can restore a container whose revocation is pending in the outbox. No refresh can
   restore anything for a retired principal.
4. **G4, at least as fresh.** A refresh queued by a delivery is looked up at `at_least_as_fresh` the token SpiceDB
   wrote that delivery at. A first read, and anything with no token, is looked up fully consistent.
5. **G5, tenant isolation.** Every key carries the tenant. A tuple whose ids carry another tenant's prefix is refused
   by `rebac.write`, maps to nobody, and is dropped from a lookup.
6. **G6, fail closed.**
   - An unknown principal is `RB404`.
   - A missing tenant is `RB400`.
   - A daemon that is not live, a set older than the freshness bound or of unknown age, an unavailable lookup and an
     over-cap set are each `RB503`.
   - None of these is an empty set, or a wider one.

7. **G7, freshness.** A principal's set is read only while the SpiceDB lookup behind it began within
   `max_projection_age_ms` (one hour by default) of the read; a set with no lookup time is never read. The lookup time
   is taken from the database's clock just before SpiceDB is asked. A lookup made `fully_consistent` reflects every
   relationship SpiceDB held at that instant. One made `at_least_as_fresh` a token may be served from a revision up
   to SpiceDB's revision quantization older (`--datastore-revision-quantization-interval`, 5 s by default). Narrowing
   only removes rows and never moves the time.

   G7 is the bound for what reaches SpiceDB WITHOUT `rebac.write` (another service, the `zed` CLI). Those changes are
   picked up only by a re-projection, and the daemon's sweep re-projects everyone every `sweep_interval_ms`
   (15 minutes by default), so keep the bound above the sweep interval plus the time a sweep takes to drain.
   Revocations through `rebac.write` never wait on it (G2).

   **Liveness is separate.** `max_staleness_ms` (5 s) fails reads closed when the daemon has not heartbeated. A beat
   proves only that the daemon is running and can reach SpiceDB and Postgres. It says nothing about how current any
   set is.

   **On upgrade**, rows written before `looked_up_at` existed have no lookup time. They read as `RB503` until the sweep
   re-projects them (or `POST /lookup` re-projects one). `rebac.status()` counts them as `unfresh_principals`.

## Not guaranteed

- **Grants are eventually consistent.** A grant shows up after relay delivery and a refresh. Until then a reader is
  briefly narrower, which is the one direction a fence may err.
- **Writes not made through `rebac.write`** (another service, the `zed` CLI) are counted on Watch and narrowed by the
  sweep only. G2 does not cover them.
- **The "still held" rule** reads the outbox's net state, so it is exact only when every view relation is written
  through `rebac.write`.
- **Caveats, expiring relationships and exclusion** are refused at start, not evaluated.
- **A read replica** holds G2 only once its replay has passed the commit.
- **Projection rows written by anything but `rebac.project`** (an application's own mirror into `rebac.readable`) do
  not take the lock order, so G2's race argument does not cover them.

## Where slice 1 differs from the design document

- **The planner-shaped primitive is the set, not the boolean.** Postgres never inlines a scalar SQL function whose
  body has a sub-select, so `readable_by(p, c)` written as `exists (...)` is a per-row function call. Measured with
  `EXPLAIN (VERBOSE)`, `container in (select * from rebac.readable_set(p))` is inlined into a semi-join.
  `readable_by` remains, as a per-row check.
- **The principal's check is a pseudo-constant qual.** It is evaluated once, in a gating filter. The principal is
  compared as the caller's own value, so the planner sees its statistics.
- **Liveness fails closed by default** (5 s), as the owner decided, and so does freshness (one hour). Revocations
  through the outbox never wait on either.
- **Consistency is `at_least_as_fresh`** by default, as the owner decided. `fully_consistent` is a setting.
- **Subject sets, one level, are supported in slice 1.** The first production schema uses them.
