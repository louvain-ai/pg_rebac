---
title: rebacd
description: The sync daemon - configuration, shadow and live modes, TLS to Postgres, the lookup endpoint and verify.
---

`rebacd` keeps the projection in step with SpiceDB. It is a Rust binary that needs an ordinary Postgres connection
and SpiceDB's gRPC API. Several replicas can run at once; they coordinate through `SKIP LOCKED` claims and
advisory locks in Postgres.

## Build and run

```sh
cargo build --release -p rebacd
DATABASE_URL=postgres://… SPICEDB_TOKEN=… ./target/release/rebacd --config rebac.toml run
```

Or with the repository's Dockerfile:

```sh
docker build -t rebacd .
docker run -e DATABASE_URL=… -e SPICEDB_TOKEN=… -v $PWD/rebac.toml:/etc/rebac.toml rebacd --config /etc/rebac.toml run
```

The SQL is installed separately, with `psql` or your migrations. No published image exists yet.

## What it runs

- **Relay.** Delivers the outbox to SpiceDB in order, revocations first, at most `relay_per_tenant` rows per tenant
  per pass so one tenant never waits behind another. A row SpiceDB refuses is retried alone and quarantined, never
  deleted. A relation SpiceDB's schema does not have yet is deferred, not quarantined, for a 10-minute settle
  window, to survive a rolling deploy.
- **Refresh.** Re-projects queued principals with LookupResources, at least as fresh as the write that queued them.
  A principal's refresh also asks which subject sets it is a member of (one LookupResources per set relation) and
  stores only what none of its grant nodes holds. A grant node is refreshed the same way, with the set itself as the
  subject; a wildcard's node is looked up as `wildcard_probe` and kept to the containers mirrored as granted to the
  wildcard.
- **Sweep.** Every `sweep_interval_ms` (15 minutes) re-projects principals and grant nodes at low priority and
  re-sends each recent revocation once, after `sweep_settle_ms` (5 minutes).
- **Heartbeat.** Beats every `heartbeat_ms` while both SpiceDB and Postgres answer. See
  [the two bounds](/docs/concepts/consistency/#two-bounds-liveness-and-freshness). A beat is liveness, not
  freshness.
- **Watch.** Applies SpiceDB's Watch stream: a revocation made outside `rebac.write` is
  narrowed in one transaction with the stream's position, under the same lock order as a commit, and everyone it
  affects is refreshed fully consistent; a grant queues a refresh. While the stream is down or a message cannot be
  applied, a principal still reads while its own lookup, or the last time the stream was current, is within
  `watch_grace_ms` (5 s, in `rebac.config`); the rest are `RB503` until the stream is back or `POST /lookup`
  re-projects them. The heartbeat is liveness only and does not wait on the stream.
  A schema change on the stream starts a closure re-check at once, and the stream reopens.
  A reopened stream replays what happened while it was down, and reads stay closed until the whole replay is applied:
  the daemon reads SpiceDB's head revision when the stream opens (a fully consistent check, read-only) and counts the
  stream current only once its position reaches that revision. `watch_marker = true` waits for a marker
  relationship it touches instead, for a datastore whose revisions it cannot compare.
- **Closure.** Re-reads the live schema every `closure_check_ms` (10 seconds), and at once on Watch's notice, so a
  schema change is caught with `watch = false` too. See [schema changes while running](#schema-changes-while-running).
- **HTTP.** `POST /lookup`, `GET /healthz`, `GET /status` and `GET /metrics` (Prometheus) on `listen`.

## Configuration

`rebac.toml` holds connections and tuning only. Unknown keys are refused. Without a file, every key takes its
default.

```toml
database_url_env  = "DATABASE_URL"       # the NAME of the variable holding the connection string
spicedb_endpoint  = "http://127.0.0.1:50051"  # https:// for TLS; see "TLS to SpiceDB" below
spicedb_token_env = "SPICEDB_TOKEN"      # the NAME of the variable holding the token, never the token
consistency       = "at_least_as_fresh"  # or "fully_consistent"
mode              = "live"               # or "shadow"
listen            = "127.0.0.1:7341"
lookup_token_env  = "REBACD_LOOKUP_TOKEN" # optional: the NAME of the variable holding the bearer token /lookup needs
watch             = true
```

| Key | Default | What it sets |
|---|---|---|
| `spicedb_ca_file` | none | PEM file of the CA that signed SpiceDB's certificate (`https://` endpoints) |
| `spicedb_insecure` | false | Allow a plaintext `http://` endpoint at a public address |
| `deadline_ms` | 5000 | Deadline of a SpiceDB call |
| `read_deadline_ms` | 3000 | Deadline of a lazy lookup from `POST /lookup` |
| `heartbeat_ms` | 1000 | How often the heartbeat is written, the leader checks its lock, and a current Watch stream says so |
| `relay_batch` | 200 | Rows per relay pass |
| `relay_per_tenant` | 50 | Rows per tenant per pass |
| `relay_window` | 5000 | The oldest rows of each tenant one pass considers, so a flood in one tenant costs the others nothing |
| `relay_idle_ms` | 200 | Wait between relay passes when the outbox is empty (failures back off from it, to 10 s) |
| `relay_lock_timeout_ms` | 15000 | `lock_timeout` of a relay pass's transaction |
| `relay_statement_timeout_ms` | 60000 | `statement_timeout` of a relay pass's transaction, and the bound on all its SpiceDB calls together; the transaction's `idle_in_transaction_session_timeout` is this plus `deadline_ms` and 1 s |
| `refresh_concurrency` | 4 | Principals refreshed at once |
| `refresh_lease_ms` | 60000 | How long a claimed refresh is held before another daemon may take it |
| `refresh_idle_ms` | 250 | Wait between refresh passes when both queues are empty |
| `sweep_interval_ms` | 900000 | How often the sweep re-projects everyone and re-sends recent revocations once |
| `sweep_settle_ms` | 300000 | How old a delivered revocation must be before the sweep re-sends it |
| `closure_check_ms` | 10000 | How often the live schema is re-read for a closure change |
| `watch_resume_max_age_ms` | 3600000 | The oldest Watch position resumed from; older restarts from head with a gap |
| `watch_marker` | false | A reopened stream waits for a marker relationship it touches, not for the head revision |
| `leader_election` | true | Only the daemon holding the leader lock runs Watch, the closure check and the sweep |
| `leader_retry_ms` | 2000 | How often a daemon that does not lead tries the lock |

What is projected lives in the `rebac.config` table, which the SQL and the daemon both read, so the two cannot
disagree:

| Key | Default |
|---|---|
| `resource_type` | `container` |
| `permission` | `view` |
| `subject_type` | `user` |
| `tenant_separator` | `/` (object ids are `<tenant>/<id>`; empty when ids carry no prefix; a tenant name may not contain it, `RB422`) |
| `implicit_subject` | `true` |
| `max_staleness_ms` | 5000 (liveness: the daemon's heartbeat) |
| `max_projection_age_ms` | 3600000 (freshness: each set's last lookup; keep it above `sweep_interval_ms`) |
| `empty_ttl_ms` | 60000 |
| `max_containers_per_principal` | 100000 (a principal's own rows; what it reads through grant nodes is not counted) |
| `max_containers_per_node` | 1000000 (a grant node's rows; over it the node is unavailable and its members read `RB503`) |
| `node_grants` | `on` (a grant to a subject set or a wildcard is stored once, under its grant node; `off` expands it per principal, as 0.2 did) |
| `wildcard_probe` | `__rebac_anyone__` (the id a wildcard node is looked up as: SpiceDB cannot look up `user:*`; grant nothing to it) |
| `exact_below_rows` | 5000 (`rebac.fenced_ids` sorts exactly below this many candidate rows, and runs the fenced ANN scan above) |
| `narrow_lock_timeout_ms` | 5000 (how long a narrowing waits for a principal's lock before it fails the commit) |
| `watch_grace_ms` | 5000 (with Watch on: how long a set stays readable after both its lookup and the stream's last current moment) |

## Several daemons

Run two or more `rebacd run` against one database for availability, or through a rolling deploy. One leads: it holds a
session advisory lock, `pg_try_advisory_lock(1919050596, 1)`, on a connection of its own, and only it runs Watch, the
closure check and the sweep. Every daemon relays the outbox (rows are claimed with `skip locked`, and one
relationship's rows are delivered in order), refreshes principals (under leases) and serves `POST /lookup`.

If the leader stops, another daemon takes the lock within `leader_retry_ms` and resumes Watch from the applied position.
If its host or network dies instead, Postgres releases the lock once the session's TCP keepalives give up, about 25 s
(the lock session sets 10 s idle, 5 s interval, 3 probes). A deposed leader cannot commit one more Watch write: each
runs in a transaction that first calls `rebac.assert_leader` with the lock connection's backend pid, which raises
(`RB503`) unless that backend still holds the lock. A shadow daemon takes `(1919050596, 2)`, so it never displaces the
live leader.

The lock needs a real session. Point `DATABASE_URL` at Postgres itself, or at a pooler in session mode: through a
pooler in transaction mode, a session-level advisory lock belongs to whichever server connection ran it.

## Shadow and live

In **live** mode the daemon relays the outbox and keeps `rebac.readable`.

In **shadow** mode it writes `rebac.readable_shadow` and never `rebac.readable`, relays nothing and re-sends nothing. It
expands every grant per principal there, whatever `node_grants` says, so the shadow is the reference to compare a live
projection's grant nodes against. It still stores the closure in `rebac.view_relation` and mirrors arrows and set grants
it reads in a backfill. Use it to prove a projection against your SpiceDB before anything reads it, then compare with
`rebacd verify --shadow`.

```sh
rebacd --config rebac.toml run --shadow
rebacd --config rebac.toml backfill --tenant acme --shadow
```

## The schema at start

At start the daemon reads the SpiceDB schema and derives the relations that can change the projected permission
(see [schema support](/docs/schema-support/)). An empty `rebac.view_relation` is filled. One that differs from the
schema refuses the start, because a missing relation is a grant that never refreshes or a revocation that never
narrows. The refusal also marks the closure invalid, so reads fail closed (`RB503`) rather than run on a list that no
longer matches. To accept the new schema:

```sh
rebacd closure               # print what the live schema derives
rebacd closure --write       # store it and rebuild every tenant
rebacd run --accept-schema   # or store it and rebuild at start
```

## Schema changes while running

The daemon re-derives the closure whenever SpiceDB's schema text changes, and acts on what it finds:

- **The same relations grant view**: nothing happens. A schema change elsewhere is not a projection change.
- **Different relations grant view**: in one transaction it stores the new list and marks `rebac.closure_state`
  invalid. Every read is `RB503` from that commit. Then, tenant by tenant, it:
  - drops mirrored tuples of relations that no longer grant view;
  - re-reads arrows and set grants from SpiceDB;
  - re-projects every principal.

  Once every tenant's relationships have been re-read, it marks the closure valid. A principal whose lookup began
  before the change, or whose re-projection failed, stays `RB503` until it is re-projected; the rebuild forces a
  sweep for that. If a tenant's relationships cannot be read, the closure stays invalid and the rebuild is retried.
  One replica rebuilds at a time.
- **A closure pg_rebac cannot project soundly** (exclusion, say): the closure is marked invalid and every read is
  `RB503` until the schema is supported again, which then rebuilds. A schema text the daemon cannot parse counts
  as this too, even where the part it cannot parse is outside the projected permission.

**In shadow mode** the daemon stores the new list and rebuilds the shadow projection, but never marks the live
closure invalid or prunes the mirrors: whatever maintains the live projection owns its reads.

The rebuild re-reads every tenant, so a large deployment is unreadable for as long as that takes. Plan schema changes
that touch the projected permission as you would a migration.

## The lazy first read

A read that raises `RB404` asks the daemon and retries:

```sh
curl -X POST localhost:7341/lookup -H 'content-type: application/json' \
  -d '{"tenant": "acme", "principal": "priya"}'
# {"containers": 2, "error": null}
```

The answer is written down, an empty one included. If SpiceDB cannot answer, the reply is a 503, the principal is
marked unavailable (`RB503` on the next read) and queued for retry. A grant node the principal
reads through that a read would refuse (stale, from before a Watch gap or a closure change, or unavailable) is
re-projected in the same call. `containers` counts the principal's own rows. A node name (`#…`) is a 400.

The reply names how many containers the principal reads, and any caller can make the daemon ask SpiceDB. So
`POST /lookup`, `GET /status` and `GET /metrics` are for your application only:

- `listen` defaults to loopback (`127.0.0.1:7341`).
- With `lookup_token_env` set, all three require `Authorization: Bearer <token>`, compared in constant time; anything
  else is a 401 that asks SpiceDB nothing. `GET /healthz` needs no token.
- On any other address without a token, the daemon logs a warning at start.

```sh
curl -X POST localhost:7341/lookup -H "authorization: Bearer $REBACD_LOOKUP_TOKEN" \
  -H 'content-type: application/json' -d '{"tenant": "acme", "principal": "priya"}'
```

## TLS to SpiceDB

The daemon sends SpiceDB's preshared key with every call, so how it reaches SpiceDB is decided by
`spicedb_endpoint`'s scheme:

- `https://` is TLS. The server's certificate is verified against the public roots, or against `spicedb_ca_file`
  (a PEM file) for a private CA, and must name the endpoint's host.
- `http://` is plaintext. It is allowed to a loopback address, and to a private one (10/8, 172.16/12, 192.168/16,
  fc00::/7, link-local) with a warning at start. To a public address it is refused unless `spicedb_insecure = true`.

## TLS to Postgres

TLS follows the connection string's `sslmode`, with libpq's meanings, using rustls:

| `sslmode` | Behaviour |
|---|---|
| `verify-full` | Checks the chain (the public roots, or `sslrootcert=<pem file>`) and the host name. Use it for a hosted Postgres. |
| `require` | Encrypts without verifying the server. |
| `prefer` | The default. Takes TLS when the server offers it. |
| `disable` | Plaintext. |
| `verify-ca` | Refused. |

The test suite runs these against a real TLS Postgres.

## Backfill

```sh
rebacd backfill --tenant acme
```

Pages through the tenant's relationships with ReadRelationships, mirrors the tenant's arrow edges and its grants to
subject sets and wildcards, projects every grant node they name and then every principal. It prints one JSON line and
exits 1 if any principal failed.

## verify

`rebacd verify` diffs the projection and prints counts only, never ids, because an id is tenant content.

```sh
rebacd verify --tenant acme [--sample 200]   # against SpiceDB, minus pending revocations
rebacd verify --tenant acme --shadow         # against rebac.readable_shadow
rebacd verify --isolation                    # across tenants that share a principal id
rebacd verify --closure                      # rebac.view_relation against the live schema
rebacd verify --all-tenants [--shadow]       # one line of totals, for a scheduled job
```

Each principal's set is compared as it reads it, through its grant nodes. Extra rows (the projection grants what
SpiceDB does not) are a fence violation and exit 2. Missing rows (SpiceDB
grants what the projection lacks yet) are a freshness gap, reported separately and never summed with extras. A
closure mismatch also exits 2.

## Logs

Set `REBACD_LOG_FORMAT=json` for one JSON object per line; otherwise text. The filter is `RUST_LOG`-style
(`info` by default). A failure is logged by its kind only: `postgres:<SQLSTATE>`, `grpc:<code>`, `pool`, `io` or
`other`. Its text, which can quote a key or a subject, never goes in a log line.
