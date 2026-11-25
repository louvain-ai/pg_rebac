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
| `stale` | Whether reads are failing closed on liveness right now. It says nothing about Watch (see below). |
| `pending_outbox` | Relationship writes not yet delivered to SpiceDB. |
| `quarantined` | Rows SpiceDB refused, held with their error for a person to look at. |
| `refresh_queue` | Principals waiting to be re-projected. |
| `external_writes` | Relationship changes seen on Watch that did not come through `rebac.write`. |
| `unavailable_principals` | Principals that read as `RB503` until they are refreshed. |
| `relay_lag_ms` | Age of the oldest undelivered outbox row. Grants wait this long; revocations do not. |
| `watch_age_ms` | Time since the Watch stream last delivered a change or a checkpoint, replayed ones included. |
| `oldest_lookup_ms` | Freshness: age of the oldest lookup behind any projected set. |
| `unfresh_principals` | Sets past `max_projection_age_ms`, or of unknown age, which read as `RB503`. |

Grant nodes (holders named `#…`, one per subject set or wildcard a grant names) are counted with the principals in
these columns: an unfresh or unavailable node fails every member closed, so it belongs in the same alert. List them
with `select * from rebac.principal_state where rebac.is_node(principal)`.

`select * from rebac.freshness()` breaks freshness down per tenant: principals, the oldest lookup, how many are
unknown or past the bound, the refresh queue and the oldest undelivered write.

The daemon serves the same row as JSON at `GET /status`, and `GET /healthz` answers `ok` while the process is up.
`/status` is a 503 when the database cannot be reached.

What each one usually means:

- `unfresh_principals` above zero: the sweep is not keeping up, or a principal's refresh keeps failing. Those
  principals read as `RB503` until re-projected.
- `stale` true: the daemon is down, or cannot reach SpiceDB or Postgres. Every fenced read is `RB503` until it beats
  again.
- `rebacd_watch_current` 0 on the leader, with `stale` false: the Watch stream is down or replaying. Once
  `watch_grace_ms` (5 s) has passed, every principal not looked up since the stream was last current reads as
  `RB503` (hint `rebac_watch`), while `stale` stays false: the heartbeat is liveness only. Alert on the gauge.
- `pending_outbox` growing: the relay is not delivering. Revocations are still in force locally; grants wait.
- `quarantined` above zero: a write SpiceDB refuses on its own. The relay keeps going around it. If it is a
  revocation, SpiceDB still holds the grant, and the next refresh of its principals restores it: alert on this, and
  fix the row (`select seq, op, resource, relation, subject, error from rebac.outbox where error is not null`).
- `external_writes` above zero: something writes SpiceDB directly. Those revocations are not in force at commit.
  They are narrowed from Watch as SpiceDB delivers them.

The daemon serves the same numbers as Prometheus gauges at `GET /metrics` (`rebac_pending_outbox`,
`rebac_relay_lag_ms`, `rebac_watch_age_ms`, `rebac_unfresh_principals` and the rest, named after the columns), plus
`rebacd_leader` and `rebacd_watch_current` for that daemon. They are counts and ages only: no tenant, principal or
container is ever a label. With `lookup_token_env` set, scrape it with the bearer token.

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

The outbox keeps every relationship write. Prune what nothing reads any more from a scheduled job:

```sql
select rebac.prune_outbox();   -- delivered over 7 days ago and superseded; up to 10,000 rows a call
```

It keeps each relationship's latest row (narrowing reads it), undelivered rows and quarantined ones.

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
- **Watch, after the live daemon is gone.** A live daemon with `watch = true` records that reads depend on its Watch
  stream (`rebac.watch_state.watch_required`). Stopping it for good, restarting it in shadow mode or running a 0.2
  daemon leaves that recorded, and once `watch_grace_ms` passes, live reads of principals not looked up since fail
  closed. Switch reads away first, or, to keep reading the live projection without Watch, run
  `select rebac.watch_required(false)`: changes made outside `rebac.write` then wait for a re-projection, as with
  `watch = false`.

pg_rebac has no uninstall script.
