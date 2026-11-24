---
title: FAQ
description: pg_rebac against Postgres row-level security, against SpiceDB on its own, and the name.
---

## Why not plain row-level security?

Row-level security is the mechanism, not the data. A policy still needs a fast answer to "may this principal view
this container", and for relationship-based access (groups, folders, workspace admins) that answer lives in a
permission service such as SpiceDB.

pg_rebac supplies that answer as an indexed local table and a set Postgres inlines into the plan. You can use it
through RLS: `rebac.enable_rls(table, column)` generates the policy, using the same set. Or apply the fence
explicitly in each query, as the examples do. The two compose: RLS decides where the fence is applied, pg_rebac
decides what it holds.

## Why not ask SpiceDB on every query?

You can, and SpiceDB stays the authority either way. Asking at query time costs a network round trip on every read,
makes every read fail when SpiceDB is down, and leaves you intersecting results in application code. Intersecting
after the fact is how a vector search returns too few rows: take the top k, drop what the reader cannot see, and a
narrow reader gets little or nothing back.

pg_rebac mirrors the permission into Postgres, so the fence applies before the `order by` and the `limit`, in the
same plan as pgvector or full-text search. A SpiceDB outage costs freshness up to the staleness bound, then reads
fail closed. It does not replace SpiceDB's check API for writes, and it does not evaluate caveats.

## Does it need a C extension?

No. Slice 1 is plain SQL and PL/pgSQL, installed with `psql -f`, so it runs on hosted Postgres that does not allow
custom extensions. The `rebacd` daemon needs only an ordinary connection.

## Which Postgres and pgvector versions?

The schema targets Postgres 14 and later. The test suite and the planner study run on Postgres 17 with pgvector
0.8. Fenced ANN reads need pgvector 0.8 or later for `hnsw.iterative_scan`.

## Is it production-ready?

Not yet. It is slice 1 and pre-release. At Louvain it is running in shadow: a shadow `rebacd` projects from SpiceDB
into `rebac.readable_shadow`, the projection is compared against SpiceDB, and no read is switched to it.

## Why the name? Wasn't it pg_fence?

It was, briefly. "pgfence" is already the name of an established Postgres migration-safety CLI, so the project was
renamed to pg_rebac: relationship-based access control (ReBAC, the Zanzibar model SpiceDB implements) inside
Postgres. The schema is `rebac`, the daemon `rebacd`, and the error codes `RB400`, `RB404`, `RB422` and `RB503`.
The two projects are unrelated.

## Licence?

Apache-2.0. The vendored SpiceDB API protos are Apache-2.0, from Authzed.
