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
   - memberships of subject sets;
   - wildcard grants on the projected type (`container#member@user:*`).
   If a lock cannot be taken within `narrow_lock_timeout_ms`, the COMMIT fails; it never commits an unnarrowed
   revocation.

   Three kinds of removal narrow EVERYONE who holds a container: an arrow tuple (a file leaving its space), a grant to
   a subject set and a wildcard grant. They name no principal, so the trigger holds the TENANT exclusively before it reads who the holders
   are. Every projection writer (a refresh, a lazy first read, a retirement) holds its tenant shared, then its
   principal, from before it asks SpiceDB until it commits. So a refresh in flight commits before the holders are read
   (and its row is narrowed), and a refresh that starts later waits for the commit (and subtracts the revocation as
   pending). `tests/sql/09_revocation_race.sql` drives both orders. The cost: such a revocation waits for every refresh
   of its tenant in flight, within `narrow_lock_timeout_ms`. The relay's own re-narrowing of the deletes it
   delivers takes no tenant lock, so a relay waiting on SpiceDB never holds such a revocation up.

   **G2 needs READ COMMITTED** (the Postgres default). Under REPEATABLE READ or SERIALIZABLE the commit-time delete reads
   the transaction's first snapshot, which cannot see a refresh that committed after it, so `rebac.write` refuses a
   view revocation there with `RB422`. Grants are allowed at any isolation level.

   G2 for arrows, via relations and sets also needs `edge` and `set_grant` to be complete. Every such write must go
   through `write`, be seen on Watch (G10), or the tenant must have been backfilled.
3. **G3, no regrowth.** No refresh can restore a container whose revocation is pending in the outbox. No refresh can
   restore anything for a retired principal.

   "Pending" ends when the relay delivers the delete. A refresh looked up `at_least_as_fresh` an OLDER token after
   that may, by SpiceDB's consistency rules, be served from a revision up to the revision quantization window
   (5 s by default) before the delete. For a direct relation this heals within seconds: the delivery queues that
   principal's refresh at the delete's token. For an arrow tuple or a grant to a subject set, the holders it narrowed
   are not named by the delivery, so a container regrown this way stays until the sweep re-sends the revocation
   (after `sweep_settle_ms`) or re-projects them. This is reasoned from SpiceDB's documented semantics, not observed.
   `consistency = "fully_consistent"` closes it.
4. **G4, at least as fresh.** A refresh queued by a delivery is looked up at `at_least_as_fresh` the token SpiceDB
   wrote that delivery at. A first read, and anything with no token, is looked up fully consistent.
5. **G5, tenant isolation.** Every key carries the tenant. A tuple whose ids carry another tenant's prefix is refused
   by `rebac.write`, maps to nobody, and is dropped from a lookup. A wildcard subject (`user:*`) carries no tenant:
   SpiceDB grants it to every `user`, in every tenant, but the resource still carries its tenant, so only that
   tenant's principals can project it (a lookup drops another tenant's ids).
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

   With `watch = false`, G7 is the bound for what reaches SpiceDB WITHOUT `rebac.write` (another service, the `zed`
   CLI): those changes are picked up only by a re-projection, and the daemon's sweep re-projects everyone every
   `sweep_interval_ms` (15 minutes by default), so keep the bound above the sweep interval plus the time a sweep takes
   to drain. With Watch on (the default), G10 bounds them instead. Revocations through `rebac.write` never wait on
   either (G2).

   **Liveness is separate.** `max_staleness_ms` (5 s) fails reads closed when the daemon has not heartbeated. A beat
   proves only that the daemon is running and can reach SpiceDB and Postgres. It says nothing about how current any
   set is.

   **On upgrade**, rows written before `looked_up_at` existed have no lookup time. They read as `RB503` until the sweep
   re-projects them (or `POST /lookup` re-projects one). `rebac.status()` counts them as `unfresh_principals`.

8. **G8, the closure matches the live schema.** The daemon re-derives the closure whenever SpiceDB's schema text
   changes: on Watch's notice, and by re-reading it every `closure_check_ms` (10 s). A change to what grants view
   marks `rebac.closure_state` invalid in the same transaction that stores the new list, so every read is `RB503`
   until every tenant's relationships are re-read into the mirrors. After that, a principal looked up before the
   change (or of unknown lookup time) stays `RB503` until it is re-projected. An unsupported or unparseable schema
   keeps every read `RB503`. A daemon in shadow mode never changes the live closure state. Between a
   schema write in SpiceDB and the daemon noticing it (up to `closure_check_ms`, or the next start), reads run on the
   old closure; that window is the one this guarantee does not close.

9. **G9, RLS cannot be widened.** The policy `rebac.enable_rls` generates is `RESTRICTIVE`, so it is ANDed with every
   other policy on the table. With `principal_source => 'session'`, a role without `EXECUTE` on
   `rebac.set_principal` cannot choose the principal, and needs no access to the projection itself.

10. **G10, a revocation made outside `rebac.write` is in force within Watch's delivery, or reads fail closed.** With
    `watch = true` (the default, live mode), the daemon follows SpiceDB's Watch over every type in the closure and
    applies each message in ONE transaction that also records the stream's position (`rebac.apply_watch`):
    - A DELETE of a closure relation that did not come through `rebac.write` is narrowed as G2 narrows a commit: the
      same function (`narrow_seqs`), the same lock order (the tenant exclusively for an arrow tuple or a grant to a
      subject set, then each principal), the same `narrow_lock_timeout_ms`. That covers direct relations, arrows, via
      relations, memberships of subject sets and grants to them, with everything under a lost container through
      self-arrows. A TOUCH carrying a caveat or an expiry is applied as a removal (the closure refuses both, so only a
      schema change the daemon has not yet re-checked could produce one; G8). No other TOUCH can narrow a union.
    - A TOUCH is a grant, and stays lazy: it is mirrored into `edge` and `set_grant` (so a later revocation through
      either path finds it) and queues a refresh.
    - Every principal narrowed, and every principal or resource the change names, is refreshed FULLY CONSISTENT. A
      refresh claimed earlier at an older token re-reads its queue row under the principal's lock and looks up fully
      consistent too, so no refresh can be served from a revision before the change (the window G3 leaves for
      relayed deletes does not apply here).
    - The daemon's own relayed writes are recognised and skipped: the same operation on the same relationship is in
      the outbox either undelivered or delivered at the very token Watch reports.

    **The bound.** While the stream is connected, a revocation made directly in SpiceDB leaves the projection within
    the time SpiceDB's Watch takes to deliver it plus one `apply_watch` transaction. (Measured once in the integration
    test, SpiceDB in memory on the same host: 26 to 70 ms over three runs, from the WriteRelationships reply to the
    row being gone.
    pg_rebac does not bound SpiceDB's delivery itself.) Otherwise reads fail closed:
    - **The stream is down, or a message cannot be applied** (a lock not taken within `narrow_lock_timeout_ms`): the
      daemon stops beating the heartbeat, so every read is `RB503` within `max_staleness_ms` (5 s), plus the
      heartbeat's own SpiceDB deadline when the connection died silently (HTTP/2 keepalive ends such a stream within
      10 s). The message is retried from the same position; nothing is half-applied.
    - **On reconnect** the stream resumes from the position applied through, so a change made while it was down is
      replayed and applied in order. Reads stay closed through the WHOLE replay: once the stream is open, the daemon
      touches a marker relationship (`<resource_type>:__rebac_watch_marker`, on a direct relation of the projected
      type, subject `<subject_type>:__rebac_watch_marker`), and the stream counts as current, and the heartbeat
      resumes, only when Watch delivers the marker's own revision. Watch delivers in revision order, so by then every
      change committed before the stream reopened has been applied. A marker not delivered within six times
      `deadline_ms` reopens the stream. The marker carries no tenant prefix and is dropped before `rebac.apply_watch`,
      so it grants nobody anything; the projected relation must accept `<subject_type>` directly.
    - **A position that cannot be resumed**: none stored (a first start, or the first start after upgrading from
      0.1, whose observe-only position applied nothing), one SpiceDB refuses (`INVALID_ARGUMENT`, `OUT_OF_RANGE`,
      `FAILED_PRECONDITION`, `NOT_FOUND`), or one whose last confirmation is older than `watch_resume_max_age_ms`
      (one hour). The age check exists because SpiceDB may resume from a revision past its garbage-collection window
      without an error and silently skip what it collected (observed with the in-memory datastore and a 2 s window).
      The stream restarts from SpiceDB's head and records `gap_at`: every principal looked up before it is `RB503`
      until it is re-projected, and the sweep is forced at once to do that (`POST /lookup` re-projects one sooner).
      Every refresh already queued is re-queued fully consistent, so none is served from before the gap.
      `rebacd backfill` and `rebacd run` store a position before they project anything, so the gap on a first start
      precedes every lookup it makes.

    With several daemons, each applies Watch and each beats only while its own stream is applying; reads stay open
    while any one of them beats. A store that cannot serve Watch fails every read closed with Watch on: set
    `watch = false` there, and G7's sweep bound applies instead. In shadow mode Watch only counts, under its own position
    (`watch_shadow`), so a shadow daemon never moves the live one's.

## Not guaranteed

- **RLS policies made by an earlier version stay permissive** until `rebac.enable_rls` is run again on the table.
- **The session binding holds only for a role that logs in as itself.** A session whose login role is trusted can
  `RESET ROLE` after a `SET ROLE` and bind any principal; SQL you do not trust must connect as the untrusted role.
- **The principal setting is trusted input.** With the default `principal_source => 'setting'`, and for any direct
  call of `rebac.readable_set(p)`, whoever can run SQL chooses the principal. Use that form only for an application
  role that sets it after authenticating the person.

- **Grants are eventually consistent.** A grant shows up after relay delivery and a refresh. Until then a reader is
  briefly narrower, which is the one direction a fence may err.
- **Writes not made through `rebac.write`** (another service, the `zed` CLI) are not in force at any commit of
  yours: G10 bounds them by Watch's delivery, or, with `watch = false`, G7 by the sweep. G2 does not cover them.
- **The "still held" rule** reads the outbox's net state, so it is exact only when every view relation is written
  through `rebac.write` or seen on Watch. Where it is not exact it narrows too much, never too little, and the refresh
  that follows restores the rest.
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
- **Subject sets, one level, are supported in slice 1.** Louvain's schema, the first pg_rebac runs against (in shadow), uses them.
