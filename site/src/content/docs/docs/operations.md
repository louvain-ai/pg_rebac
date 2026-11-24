---
title: Operations
description: Monitoring the projection and the daemon, the checks to schedule, and how to roll back.
---

## What to watch

`rebac.status()` is one row, readable from SQL:

```sql
select * from rebac.status();
```

| Column | What it tells you |
|---|---|
| `heartbeat_age_ms` | Liveness: age of the newest daemon heartbeat. Reads fail closed past `max_staleness_ms`. |
| `stale` | Whether reads are failing closed on liveness right now. |
| `pending_outbox` | Relationship writes not yet delivered to SpiceDB. |
| `quarantined` | Rows SpiceDB refused, held with their error for a person to look at. |
| `refresh_queue` | Principals waiting to be re-projected. |
| `external_writes` | Relationship changes seen on Watch that did not come through `rebac.write`. |
| `unavailable_principals` | Principals that read as `RB503` until they are refreshed. |
| `relay_lag_ms` | Age of the oldest undelivered outbox row. Grants wait this long; revocations do not. |
| `watch_age_ms` | Time since the Watch stream last delivered a change or a checkpoint. |
| `oldest_lookup_ms` | Freshness: age of the oldest lookup behind any projected set. |
| `unfresh_principals` | Sets past `max_projection_age_ms`, or of unknown age, which read as `RB503`. |

`select * from rebac.freshness()` breaks freshness down per tenant: principals, the oldest lookup, how many are
unknown or past the bound, the refresh queue and the oldest undelivered write.

The daemon serves the same row as JSON at `GET /status`, and `GET /healthz` answers `ok` while the process is up.
`/status` is a 503 when the database cannot be reached.

What each one usually means:

- `unfresh_principals` above zero: the sweep is not keeping up, or a principal's refresh keeps failing. Those
  principals read as `RB503` until re-projected.
- `stale` true: the daemon is down, or cannot reach SpiceDB or Postgres. Every fenced read is `RB503` until it beats
  again.
- `pending_outbox` growing: the relay is not delivering. Revocations are still in force locally; grants wait.
- `quarantined` above zero: a write SpiceDB refuses on its own. The relay keeps going around it.
- `external_writes` above zero: something writes SpiceDB directly. Those revocations are narrowed by the sweep, not
  at commit.

## Logs

`REBACD_LOG_FORMAT=json` writes one JSON object per line. A failure is logged by its kind
(`postgres:<SQLSTATE>`, `grpc:<code>`, `pool`, `io`, `other`), never its text, so log lines carry no tenant content.
The full text stays on the queue row, for example `rebac.refresh.last_error`.

## Checks to schedule

```sh
rebacd verify --all-tenants --sample 200   # one line of totals; exits 2 on any extra row
rebacd verify --isolation                  # no principal id reads across tenants
rebacd verify --closure                    # the stored closure still matches the schema
```

Alert on exit status 2. Extra rows are a fence violation; missing rows are a freshness gap and are reported
separately.

## Prove before you switch

1. Install `rebac.sql` beside whatever you use today. Nothing reads it yet.
2. Run `rebacd run --shadow` and `rebacd backfill --tenant <t> --shadow`. Shadow mode writes only
   `rebac.readable_shadow` and relays nothing.
3. Compare with `rebacd verify --tenant <t> --shadow`, or against SpiceDB, until extras and missings are both zero.
4. Then point reads at `rebac.readable_set`, keeping your current path behind a flag.

## Rolling back

The projection is a cache of SpiceDB plus pending revocations, so it can be rebuilt with `rebacd backfill` at any
time. To step back:

- **Reads:** switch your read path back to its previous source. Nothing in pg_rebac needs to change.
- **The daemon:** stopping it is safe for revocations written through `rebac.write`, which narrow at commit without
  it. Reads start failing closed after `max_staleness_ms`; that is the bound doing its job.
- **Live to shadow:** restart with `--shadow` (or `mode = "shadow"`). Relaying stops and only the shadow table is
  written.

pg_rebac has no uninstall script in slice 1.
