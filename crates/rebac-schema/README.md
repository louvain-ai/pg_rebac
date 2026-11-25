# rebac-schema

Parses a SpiceDB schema and derives which relations can change one permission. It is the schema reader behind
[`rebacd`](https://crates.io/crates/rebacd), the sync daemon of [pg_rebac](https://pgrebac.com): it tells the daemon
which relationship writes can change a permission that the Postgres projection holds, so only those trigger a refresh.

Part of [pg_rebac](https://github.com/louvain-ai/pg_rebac). Licence: Apache-2.0.
