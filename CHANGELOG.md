# Changelog

## 0.3.0

- **Grant nodes: a grant to a group or to everyone is stored once.** A grant to a subject set
  (`container#member@group:eng#member`) or a wildcard (`user:*`) is held by its grant node (`#group:eng#member`,
  `#user:*`) in `rebac.readable`, looked up once with the set itself as the subject, and read by every member through
  `rebac.holders_of` at query time. A principal records its memberships from its own lookup (`rebac.node_member`, new)
  and stores only what none of its nodes holds. The fence is still one table and one index
  (`principal = ANY(holders)`): a plan with the HNSW scan outside probes it once per candidate, as in 0.2. A set grant or a wildcard grant refreshes one node
  instead of every member. On the benchmark's tenant (`examples/projection-scale/`) the projection for 100,000 people
  is 0.30 GB where 0.2 needed 26.9 GB, and 2.7 GB at a million people. `node_grants = 'off'` keeps 0.2's per-principal expansion.
- **Every removal narrows grant nodes at commit:** a membership (its `node_member` row and the member's own copies), a
  grant to a set, an arrow, a wildcard. A stale, unavailable or over-cap node (`max_containers_per_node`, new, one
  million) fails its members closed (`RB503`). A principal's set is as fresh as the oldest lookup behind it, its own
  or a node's; the Watch gap and grace and the closure check use that lookup too. Reading as a node name (`#…`) is
  `RB400`.
- **Security fix, also in 0.2.1: a set granted through a via relation is narrowed at commit.** In 0.1.0 to 0.2.0 a
  member who left `group:ops`, or everyone when `workspace#admin@group:ops#member` itself was removed, kept what it
  granted until a refresh. Grants to a set through a via relation are now mirrored in `rebac.set_grant` with their
  object typed (`workspace:w1`); the membership's removal narrows what is under the object, and the grant's removal
  narrows every holder of it, as a grant to a set on the projected type already did. Found by the SpiceDB oracle test
  below.
- **A revocation that names nobody re-projects whom it narrowed.** An arrow, a set grant or a wildcard removed
  records the holders and node containers it narrowed (`rebac.outbox.narrowed`, `narrowed_containers`), and its
  delivery re-projects them at the delete's token, so a principal who also reads the container another way gets it
  back without waiting for the sweep.
- **`rebac.fenced_ids`' ANN leg probes per candidate.** The planner cannot see what a node holds, so the `in` form may
  plan a reader who is broad through a node as an exact sort; `fenced_ids` now writes its HNSW leg as a probe per
  candidate row, which keeps the HNSW scan outside. Its candidate array is sorted, which made the count three times
  faster for a reader of 21,000 containers.
- **SQL:** `rebac.node_member`, `rebac.holders_of`, `rebac.is_node`, `rebac.expanded` (what a principal reads,
  unchecked, for operators), `rebac.stale_nodes`, `rebac.pending_left_nodes`, `rebac.wildcard_closure`,
  `rebac.via_children`, `rebac.set_grant_containers`; `rebac.outbox.narrowed` and `narrowed_containers`; settings
  `node_grants`, `max_containers_per_node`, `wildcard_probe`. `rebac.project` takes the principal's memberships as an
  optional last argument. `rebac.breadth` sums the rows of the principal's holders, so a container held two ways
  counts twice. `rebac.version` changes with a node's rows and with the principal's memberships (with nodes it is a hash
  of every holder's counter, not one counter).
- **Upgrading from 0.2** re-runs `sql/rebac.sql`. 0.2's rows stay and read as before. The first install forces a
  sweep, which records memberships and moves group grants under their nodes, and marks `remirror_pending`: rebacd 0.3
  re-reads every tenant's relationships once at its first start to mirror what 0.2 did not (grants to sets through a
  via relation, wildcard grants). Until it has, leaving such a set waits for a refresh, as in 0.2. `rebacd` 0.3 must run with this SQL; while a
  0.2 daemon still runs, set `node_grants = 'off'`. `rebac.node_member` is new: re-run the daemon role's grants.
- **Tests.** `crates/rebacd/tests/oracle.rs` compares every principal's set with SpiceDB's LookupResources over
  randomised schemas and relationships: after a backfill, at each revocation's commit, after changes applied from
  Watch, and after the queues drain. `tests/sql/18_grant_nodes.sql` and `19_node_races.sql` (dblink, both orders)
  cover nodes; `tests/upgrade/run.sh` installs 0.2.0, projects, applies this SQL twice and checks reads and
  revocations.
- **A dropped Watch stream fails closed per principal, not everyone.** In 0.2.0 the heartbeat required the stream, so
  one Watch hiccup made every read in every tenant `RB503` within 5 s. Now the heartbeat is liveness only, and
  `rebac.checked` calls `rebac.watch_ok` (new): a set is read while its own lookup, or the last time the caught-up
  stream was current (`watch_state.current_at`, new), is within `watch_grace_ms` (new `rebac.config` key, 5000).
  Principals looked up since keep reading; `POST /lookup` re-projects any other at once. A live daemon records
  whether Watch is on (`watch_state.watch_required`, `rebac.watch_required`).
- **Reads stay closed through a Watch replay (fix).** In 0.2.0 a reconnected Watch stream counted as current, and
  the heartbeat resumed, after its first replayed message, so reads reopened while revocations made during the outage
  were still being applied. The daemon now reads SpiceDB's head revision when the stream opens (a fully consistent
  CheckPermission, read-only) and counts the stream current only once its position reaches it. `watch_marker` (new,
  off) waits for a marker relationship instead, for a datastore whose revisions cannot be compared.
- **`POST /lookup` and `GET /status` can require a bearer token** (`lookup_token_env`, the NAME of the variable that
  holds it). A lookup's reply names how many containers a principal reads, and any caller could make the daemon ask
  SpiceDB; with a token set, anything else is a 401. A non-loopback `listen` without a token logs a warning.
- **Every schema refusal states its reason** (the closure's messages said "not supported").
- **Several daemons on one database, safely: leader election.** Only the daemon holding a session advisory lock
  (`1919050596, 1`; shadow `1919050596, 2`) runs Watch, the closure check and the sweep; every daemon relays,
  refreshes and serves `/lookup`. Every Watch write is fenced by `rebac.assert_leader(pid, lock_key)` (new), so a
  deposed leader cannot commit another. `leader_election` (on) and `leader_retry_ms` (2 s) are new settings.
- **A Watch stream on an idle store goes current (fix).** In 0.2.0 a live daemon with Watch on, restarted against an
  idle in-memory SpiceDB, never heartbeated until the next change, so every read was refused. A first start now
  restarts from the head revision read directly, and a resumed stream whose stored position is the head is current
  at once. Watch now follows every type (relations outside the closure are skipped), so a head past the position
  always means changes the stream delivers.
- **`rebac.rls_bypasses(tbl)`: who reads a fenced table without the fence** (superusers, `BYPASSRLS` roles, an owner
  without FORCE, views without `security_invoker`, materialized views). `rebac.enable_rls` warns about each.
- **Docs.** The FAQ answers outages, freshness and zedtokens, RLS bypasses, open transactions after a revocation,
  upgrades and maintenance; NOTICE names the vendored protos' copyright holders.
- **CI: through PgBouncer in transaction mode** (`tests/pooler/run.sh`): a session `SET` leaks to the next client and
  `SET LOCAL` does not; a fenced read, a revocation at commit and `set_principal` work through it. **And as on hosted
  Postgres**: the install beside the usual extensions, re-run over itself, and the daemon's calls as a role with
  privileges on schema `rebac` only. Getting started documents both.
- **`GET /metrics`**: `rebac.status()` as Prometheus gauges, plus `rebacd_leader` and `rebacd_watch_current`. No id is
  ever a label; behind the lookup token when one is set.
- **A schema with non-ASCII text outside a comment or string is a parse error (fix).** `rebac_schema::parse`
  panicked on it (a byte index inside a character). SpiceDB refuses such a schema, so `rebacd` reading a live one
  never met it.
- **A relay outage is logged as `grpc:<code>` (fix)**, not `other`: the SpiceDB status stayed out of the error.

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
