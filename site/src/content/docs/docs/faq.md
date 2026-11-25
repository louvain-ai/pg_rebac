---
title: FAQ
description: pg_rebac against filtering after retrieval, Postgres row-level security, SpiceDB on its own, AuthZed Materialize and the SpiceDB FDW, and the name.
---

For each alternative side by side (row-level security, SpiceDB per query, over-fetching, Materialize, OpenFGA, Oso,
Cerbos, a table of your own, vector-store filters) and when it is the better choice, see [Why not X?](/docs/why-not/).

## Why not just filter results after retrieval?

Because the search's `limit` runs before your filter, and everything it computed on the way is about rows the reader
may not see.

- **Top-k starvation.** Take the top 20, drop what the reader may not view, and a reader of 5% of the corpus keeps
  about one row. The [planner study](/docs/performance/) measures the in-database form of the same mistake, an HNSW
  scan with `hnsw.iterative_scan = off`: readers of 5%, 10% and 30% breadth got 8%, 20% and 62% of k, with no error.
- **Over-fetching costs latency and has no floor.** Filling k rows for a reader of breadth b takes about k/b
  candidates. Fenced in the plan, readers of 5% to 50% got all k rows at recall 0.90 to 0.96 in 4 to 7 ms, against
  1.9 ms unfenced; readers below about 1% take the exact path, 1.6 ms at full recall.
- **Counts, snippets and rankings leak.** A total, a highlighted snippet, a facet count or a page offset computed before
  the filter says something about rows the reader may not view. With the fence in the `where` clause, no such row is
  ranked, counted or returned.

The [quickstart](/docs/getting-started/#see-it-work-in-one-command) shows the first point on seventeen rows:
`curl -fsSL https://pgrebac.com/quickstart.yml | docker compose -f - run --rm demo` runs one vector search as two
people, then filters after retrieval, and the second person gets one row of five.

## Why not plain row-level security?

Row-level security is the mechanism, not the data. A policy still needs a fast answer to "may this principal view
this container", and for relationship-based access (groups, folders, workspace admins) that answer lives in a
permission service such as SpiceDB.

pg_rebac supplies that answer as an indexed local table and a set Postgres inlines into the plan. You can use it through
RLS: `rebac.enable_rls(table, column)` generates a restrictive policy, using the same set. Whoever may choose the
principal is spelled out in [row-level security](/docs/concepts/readable-set/#row-level-security). Or apply the fence
explicitly in each query, as the examples do. The two compose: RLS decides where the fence is applied, pg_rebac decides
what it holds.

Measured on 500,000 passages ([performance](/docs/performance/)): for an ANN top-20 a policy on the readable set
cost about the same as the fence in the query. For a query that reads many rows, a policy is planned as a filter on
the scan, not a join: counting a narrow reader's 501 rows took 0.6 ms with the fence in the query and 17 ms through
the policy. A policy that calls a check function per row took 3.4 s, whatever the reader.

## How large does the projection get?

A grant to a person is one row per container. A grant to a group or to everyone is one row per container, held by the
group's grant node, plus one membership row per member: it is not multiplied by the number of members. Measured on the
benchmark's tenant (teams, departments, an all-company group and 20 direct shares per person, 100,000 containers,
UUID-length ids), the projection takes the sizes in [Performance](/docs/performance/#projection-scale), against 0.2's
one row per person per container. The cap on what one person stores, `max_containers_per_principal`, now counts only
their own grants.

## How does it compare with AuthZed Materialize and the SpiceDB FDW?

Both are AuthZed's. As their docs describe them on 2026-11-24:

- **AuthZed Materialize** precomputes the permissions you configure. It is available on AuthZed Dedicated only. Its
  Accelerated Queries speed up SpiceDB's own check and lookup calls; its Event Streams, in early access, stream
  permission sets for you to store in your own database or search index. Caveats, wildcards, `.all` intersections and
  expiring relationships cannot be on the path of a materialised permission, and the docs say an application
  consuming the stream must tolerate some lag
  ([limitations](https://authzed.com/docs/materialize/getting-started/limitations)).
- **The SpiceDB Postgres FDW** is a proxy that speaks the Postgres wire protocol and turns each query into SpiceDB
  API calls (CheckPermission, LookupResources or LookupSubjects). Its docs call it experimental, and list joins
  between its tables and subqueries as unsupported ([Postgres FDW](https://authzed.com/docs/spicedb/ops/postgres-fdw)).
- **pg_rebac** is Apache-2.0 and lives inside Postgres:
  - the projection is an ordinary table that `rebacd` keeps, and Postgres plans the fence as a join, with tested
    paths for pgvector's HNSW scans (relaxed iterative scans, and an exact path for narrow readers);
  - a warm read makes no call to SpiceDB;
  - a revocation written through `rebac.write` is removed in the same transaction that writes it, for the
    [schema shapes it supports](/docs/schema-support/): unions of direct relations, arrows and one level of subject
    sets.
    Schemas outside those are refused.

Choose Materialize for a schema shape pg_rebac refuses and Materialize supports, or to feed permissions to more than one
datastore. Choose pg_rebac for an open-source fence that is part of the Postgres query plan, on any Postgres, with
revocations tied to the writing transaction.

## Why not ask SpiceDB on every query?

You can, and SpiceDB stays the authority either way. Asking at query time costs a network round trip on every read,
makes every read fail when SpiceDB is down, and leaves you intersecting results in application code. Intersecting
after the fact is how a vector search returns too few rows: take the top k, drop what the reader cannot see, and a
narrow reader gets little or nothing back.

pg_rebac mirrors the permission into Postgres, so the fence applies before the `order by` and the `limit`, in the
same plan as pgvector or full-text search. A SpiceDB outage costs at most the liveness bound (5 s), then reads
fail closed. It does not replace SpiceDB's check API for writes, and it does not evaluate caveats.

## What happens when SpiceDB or rebacd is down?

Reads fail closed, with bounds you set:

- a revocation through `rebac.write` is in force at commit whether SpiceDB and the daemon are up or not;
- the daemon heartbeats while it reaches SpiceDB and Postgres; with no beat for `max_staleness_ms` (5 s), every read
  is `RB503`;
- if only the Watch stream drops, a principal still reads while Watch was current, or its own lookup began, within
  `watch_grace_ms` (5 s); the rest are `RB503` until Watch is back or `POST /lookup` re-projects them;
- a set whose lookup is older than `max_projection_age_ms` (one hour) is `RB503`.

Run two or more daemons for availability: one leads (Watch, the closure check, the sweep), and another takes over
within 2 s of a clean stop, or about 25 s after its host dies ([rebacd](/docs/rebacd/#several-daemons)).

## How fresh is it, and what about zedtokens?

A revocation through `rebac.write` is in force when its transaction commits. A revocation written straight to SpiceDB
leaves the projection as Watch delivers it, or reads fail closed. A grant shows up after the relay delivers it and a
refresh runs, which looks up `at_least_as_fresh` the zedtoken SpiceDB wrote it at. That default can be served up to
SpiceDB's revision quantization (5 s) before a later delete, so a relayed arrow or group revocation may briefly regrow
until the sweep re-sends it; `consistency = "fully_consistent"` closes that window
([consistency](/docs/concepts/consistency/)).

## Can a role read a fenced table without the fence?

Yes, as with any row-level security: a superuser, a role with `BYPASSRLS`, the table's owner (unless the table is
`FORCE ROW LEVEL SECURITY`; `rebac.enable_rls(..., force => true)` sets it), a view planned with its owner's rights
(give it `security_invoker`, Postgres 15+), a materialized view, and a `SECURITY DEFINER` function owned by such a
role. `rebac.enable_rls` warns about each role and view it finds, and `select * from rebac.rls_bypasses('doc')` lists
them at any time. Fencing in the query (`container in (select * from rebac.readable_set(p))`) does not depend on any
of this.

## What does a transaction that is already open see after a revocation?

What its snapshot saw. Postgres reads are snapshot reads: a READ COMMITTED query started after the revoking commit
sees the narrowed projection, but a REPEATABLE READ or SERIALIZABLE transaction that took its snapshot before keeps
reading the old rows until it ends. Keep fenced reads in short transactions. A revocation itself must commit under
READ COMMITTED, the Postgres default; `rebac.write` refuses one under REPEATABLE READ or SERIALIZABLE with `RB422`.

## How do I upgrade?

Run the new release's `rebac.sql` over the old one (`psql -v ON_ERROR_STOP=1 -f pg_rebac--<version>.sql`): every
statement is `create … if not exists`, `create or replace` or an `alter … if not exists`, so it applies in place and
keeps the projection. Then restart `rebacd` on the same release. The CHANGELOG names each upgrade that makes reads
`RB503` until principals are re-projected (0.1 to 0.2 records one Watch gap): run `rebacd backfill --tenant <t>` per
tenant first, or let the forced sweep do it, and `POST /lookup` re-projects any one principal at once.

From 0.2 to 0.3, reads stay open throughout on 0.2's rows, which hold every grant expanded per principal. The install
forces one sweep, which records each principal's memberships and moves group and wildcard grants under their grant
nodes; until a principal is re-projected, a new grant to a set reaches it only through that refresh. rebacd 0.3 also
re-reads every tenant's relationships once at its first start, to mirror what 0.2 did not (grants to sets through a via
relation, wildcard grants); until it has, leaving such a set waits for a refresh, as it did in 0.2. Upgrade every
`rebacd` with the SQL, or set `node_grants = 'off'` while a 0.2 daemon still runs. `rebac.node_member` is new, so re-run
the daemon role's grants.

## Who maintains it?

Louvain builds and maintains pg_rebac and runs it in its own product. It is Apache-2.0 with no CLA: contributions are
signed off under the Developer Certificate of Origin (`git commit -s`), so the licence of every line stays Apache-2.0.
Report a vulnerability privately, as `SECURITY.md` describes.

## Does it work on RDS, Cloud SQL, Neon or Supabase, and behind PgBouncer?

It needs a role that can create a schema (a database owner; no superuser), PL/pgSQL, and pgvector only for vector
search; [what the install needs](/docs/getting-started/#install-the-sql) lists the privileges and the daemon's own
role. Applications can read through PgBouncer in transaction mode with `set local`; `rebacd` connects directly or
through a session-mode pooler.

## Does it need a C extension?

No. It is plain SQL and PL/pgSQL, installed with `psql -f`, so it runs on hosted Postgres that does not allow
custom extensions. The `rebacd` daemon needs only an ordinary connection.

## Which Postgres and pgvector versions?

Postgres 14 and later. CI runs the whole SQL suite on Postgres 14, 15, 16 and 17 with pgvector, as a superuser and
again as a database owner that is not one. The planner studies ran on Postgres 17 with pgvector 0.8. Fenced ANN reads
need pgvector 0.8 or later for `hnsw.iterative_scan`; nothing else needs pgvector.

## Who uses it?

Louvain mirrors every permission change into pg_rebac and verifies it against SpiceDB daily:

- Louvain's own permission projection is mirrored into pg_rebac's tables, in each writer's transaction;
- a shadow `rebacd` re-projects every reader from SpiceDB into `rebac.readable_shadow`;
- once a day, `rebacd verify --all-tenants --shadow` compares the two and exits non-zero on any extra row.

## Why the name? Wasn't it pg_fence?

It was, briefly. "pgfence" is already the name of an established Postgres migration-safety CLI, so the project was
renamed to pg_rebac: relationship-based access control (ReBAC, the Zanzibar model SpiceDB implements) inside
Postgres. The schema is `rebac`, the daemon `rebacd`, and the error codes `RB400`, `RB404`, `RB422` and `RB503`.
The two projects are unrelated.

## Licence?

Apache-2.0. The vendored SpiceDB API protos are Apache-2.0, from Authzed.
