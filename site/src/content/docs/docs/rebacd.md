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
- **Watch.** Follows SpiceDB's Watch stream and counts writes made outside pg_rebac. It is observe-only in slice 1.
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

In **shadow** mode it writes only `rebac.readable_shadow`, relays nothing and re-sends nothing. Use it to prove a
projection against your SpiceDB before anything reads it, then compare with `rebacd verify --shadow`.

```sh
rebacd --config rebac.toml run --shadow
rebacd --config rebac.toml backfill --tenant acme --shadow
```

## The schema at start

At start the daemon reads the SpiceDB schema and derives the relations that can change the projected permission
(see [schema support](/docs/schema-support/)). An empty `rebac.view_relation` is filled. One that differs from the
schema refuses the start, because a missing relation is a grant that never refreshes or a revocation that never
narrows. To accept the new schema:

```sh
rebacd closure            # print what the live schema derives
rebacd closure --write    # store it
rebacd run --accept-schema   # or replace it at start, and re-project everyone
```

## The lazy first read

A read that raises `RB404` asks the daemon and retries:

```sh
curl -X POST localhost:7341/lookup -H 'content-type: application/json' \
  -d '{"tenant": "acme", "principal": "priya"}'
# {"containers": 2, "error": null}
```

The answer is written down, an empty one included. If SpiceDB cannot answer, the reply is a 503, the principal is
marked unavailable (`RB503` on the next read, never an empty set) and queued for retry.

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
