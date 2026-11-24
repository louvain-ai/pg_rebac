# rebacd

The [pg_rebac](https://pgrebac.com) daemon. It relays relationship writes queued in Postgres (`rebac.outbox`) to
SpiceDB in order, and keeps the local projection (`rebac.readable`) that queries fence themselves with.

```sh
cargo install rebacd
rebacd --config rebac.toml run
```

The SQL (`rebac.sql`) is installed separately, with `psql` or a migration; it is attached to each GitHub Release. A
container image is published at `ghcr.io/louvain-ai/rebacd`.

Documentation: <https://pgrebac.com>. Source: <https://github.com/louvain-ai/pg_rebac>. Licence: Apache-2.0.
