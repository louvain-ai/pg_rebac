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
- **Sweep.** Every `sweep_interval_ms` (15 minutes) re-projects principals at low priority and re-sends each recent
  revocation once, after `sweep_settle_ms` (5 minutes).
- **Heartbeat.** Beats every `heartbeat_ms` while both SpiceDB and Postgres answer. See
  [the two bounds](/docs/concepts/consistency/#two-bounds-liveness-and-freshness). A beat is liveness, not
  freshness.
- **Watch.** Applies SpiceDB's Watch stream: a revocation made outside `rebac.write` is
  narrowed in one transaction with the stream's position, under the same lock order as a commit, and everyone it
  affects is refreshed fully consistent; a grant queues a refresh. While the stream is down or a message cannot be
  applied, the heartbeat stops, so reads fail closed.
  A schema change on the stream starts a closure re-check at once, and the stream reopens on the new closure's types.
  A reopened stream replays what happened while it was down, and reads stay closed until the whole replay is applied:
  the daemon touches a marker relationship (`<resource_type>:__rebac_watch_marker`) when the stream opens, and counts
  the stream current only once Watch delivers that marker.
- **Closure.** Re-reads the live schema every `closure_check_ms` (10 seconds), and at once on Watch's notice, so a
  schema change is caught with `watch = false` too. See [schema changes while running](#schema-changes-while-running).
- **HTTP.** `POST /lookup`, `GET /healthz` and `GET /status` on `listen`.

## Configuration

`rebac.toml` holds connections and tuning only. Unknown keys are refused. Without a file, every key takes its
default.

```toml
database_url_env  = "DATABASE_URL"       # the NAME of the variable holding the connection string
spicedb_endpoint  = "http://127.0.0.1:50051"
spicedb_token_env = "SPICEDB_TOKEN"      # the NAME of the variable holding the token, never the token
consistency       = "at_least_as_fresh"  # or "fully_consistent"
mode              = "live"               # or "shadow"
listen            = "127.0.0.1:7341"
lookup_token_env  = "REBACD_LOOKUP_TOKEN" # optional: the NAME of the variable holding the bearer token /lookup needs
watch             = true
```

| Key | Default | |
|---|---|---|
| `deadline_ms` | 5000 | Deadline of a SpiceDB call |
| `read_deadline_ms` | 3000 | Deadline of a lazy lookup from `POST /lookup` |
| `heartbeat_ms` | 1000 | |
| `relay_batch` | 200 | Rows per relay pass |
| `relay_per_tenant` | 50 | Rows per tenant per pass |
| `relay_window` | 5000 | |
| `relay_idle_ms` | 200 | |
| `relay_lock_timeout_ms` | 15000 | |
| `relay_statement_timeout_ms` | 60000 | |
| `refresh_concurrency` | 4 | Principals refreshed at once |
| `refresh_lease_ms` | 60000 | |
| `refresh_idle_ms` | 250 | |
| `sweep_interval_ms` | 900000 | |
| `sweep_settle_ms` | 300000 | |
| `closure_check_ms` | 10000 | How often the live schema is re-read for a closure change |

What is projected lives in the `rebac.config` table, which the SQL and the daemon both read, so the two cannot
disagree:

| Key | Default |
|---|---|
| `resource_type` | `container` |
| `permission` | `view` |
| `subject_type` | `user` |
| `tenant_separator` | `/` (object ids are `<tenant>/<id>`; empty when ids carry no prefix) |
| `implicit_subject` | `true` |
| `max_staleness_ms` | 5000 (liveness: the daemon's heartbeat) |
| `max_projection_age_ms` | 3600000 (freshness: each set's last lookup; keep it above `sweep_interval_ms`) |
| `empty_ttl_ms` | 60000 |
| `max_containers_per_principal` | 100000 |
| `narrow_lock_timeout_ms` | 5000 |

## Shadow and live

In **live** mode the daemon relays the outbox and keeps `rebac.readable`.

In **shadow** mode it writes `rebac.readable_shadow` and never `rebac.readable`, relays nothing and re-sends nothing.
It still stores the closure in `rebac.view_relation` and mirrors arrows and set grants it reads in a backfill. Use it to prove a
projection against your SpiceDB before anything reads it, then compare with `rebacd verify --shadow`.

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
marked unavailable (`RB503` on the next read, never an empty set) and queued for retry.

The reply names how many containers the principal reads, and any caller can make the daemon ask SpiceDB. So
`POST /lookup` and `GET /status` are for your application only:

- `listen` defaults to loopback (`127.0.0.1:7341`).
- With `lookup_token_env` set, both require `Authorization: Bearer <token>`, compared in constant time; anything else
  is a 401 that asks SpiceDB nothing. `GET /healthz` needs no token.
- On any other address without a token, the daemon logs a warning at start.

```sh
curl -X POST localhost:7341/lookup -H "authorization: Bearer $REBACD_LOOKUP_TOKEN" \
  -H 'content-type: application/json' -d '{"tenant": "acme", "principal": "priya"}'
```

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

Pages through the tenant's relationships with ReadRelationships, projects every principal they name with
LookupResources, and mirrors the tenant's arrow edges. It prints one JSON line and exits 1 if any principal failed.

## verify

`rebacd verify` diffs the projection and prints counts only, never ids, because an id is tenant content.

```sh
rebacd verify --tenant acme [--sample 200]   # against SpiceDB, minus pending revocations
rebacd verify --tenant acme --shadow         # against rebac.readable_shadow
rebacd verify --isolation                    # across tenants that share a principal id
rebacd verify --closure                      # rebac.view_relation against the live schema
rebacd verify --all-tenants [--shadow]       # one line of totals, for a scheduled job
```

Extra rows (the projection grants what SpiceDB does not) are a fence violation and exit 2. Missing rows (SpiceDB
grants what the projection lacks yet) are a freshness gap, reported separately and never summed with extras. A
closure mismatch also exits 2.

## Logs

Set `REBACD_LOG_FORMAT=json` for one JSON object per line; otherwise text. The filter is `RUST_LOG`-style
(`info` by default). A failure is logged by its kind only: `postgres:<SQLSTATE>`, `grpc:<code>`, `pool`, `io` or
`other`. Its text, which can quote a key or a subject, never goes in a log line.
