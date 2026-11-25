---
title: Why not X?
description: When you do not need pg_rebac, and when you do, against row-level security, asking SpiceDB per query, over-fetching, per-row checks, AuthZed Materialize and the FDW, OpenFGA, Oso and Cerbos, an ACL table of your own, and vector stores with metadata filters.
---

pg_rebac solves one problem: answering "which rows may this person read" inside a Postgres query plan, when the
answer comes from a relationship graph in SpiceDB and the query ends in an `order by` and a `limit`. If you do not
have that problem, you do not need it. This page says, for each alternative, where it is the better choice.

Numbers come from five runs, each named where it is used. All are synthetic data on one machine with a warm cache; the
method and limits of each are in [`docs/planner.md`](https://github.com/louvain-ai/pg_rebac/blob/main/docs/planner.md)
and
[`docs/design/projection-scale.md`](https://github.com/louvain-ai/pg_rebac/blob/main/docs/design/projection-scale.md).

- **The 128-dimension study**: 500,000 passages in 5,000 containers, p50 of 10 queries, 4 CPUs.
- **The alternatives run**: 499,868 passages of 768 dimensions in 20,000 skewed containers, pgbench, one client, a
  10-CPU laptop.
- **The 768-dimension study**: the same data as the alternatives run, with p95 and p99 and the exact path measured.
- **The 100,000-person run**: 100,000 containers, teams, departments and an all-company group, 200,000 passages of
  768 dimensions.
- **The sweep run**: 2,000 principals against SpiceDB (in-memory datastore) on the same laptop as Postgres.

Facts about other products are from their own documentation, read on 2026-11-25 and listed under
[Sources](#sources).

## Summary

| Alternative | Enough on its own when | pg_rebac earns its place when | Measured here |
|---|---|---|---|
| [Row-level security alone](#row-level-security-on-its-own) | Access is a tenant or role column, or a small role table | Access comes from groups, folders and shares in SpiceDB | `count(*)` of a 501-row reader: 0.6 ms fence in the query, 17 ms in a policy |
| [Ask SpiceDB per query](#ask-spicedb-on-every-query) | Single checks, or lists of a few thousand without ranking | You need the top k by relevance or distance over a large set | Not measured on the query path |
| [Over-fetch, then filter](#over-fetch-then-filter) | Every reader sees most of the corpus | Readers' breadth varies, or any reader is narrow | At 5% breadth: N = 1,000 to fill k, 43 ms, against 19 ms fenced |
| [A per-row check function](#a-per-row-check-function) | You check one row at a time | A query scans or ranks many rows | 3.4 s per `count(*)` over 500,000 rows |
| [AuthZed Materialize, SpiceDB FDW](#authzed-materialize-and-the-spicedb-fdw) | You are on AuthZed Dedicated, for a shape pg_rebac refuses and Materialize supports | You want the set in your own Postgres plan, open source, with revocation at commit | Not compared |
| [OpenFGA, Oso, Cerbos](#openfga-oso-and-cerbos) | Your authorization system is one of these | Your authorization system is SpiceDB | Not compared |
| [An ACL table you build](#an-acl-table-you-build-yourself) | Permissions originate in your own database | Permissions originate in SpiceDB and must stay in step with it | Group grants stored once: 302 MB against 26.9 GB at 100,000 people |
| [Vector store metadata filters](#a-vector-store-with-metadata-filters) | Access is per tenant, or a few group tags per document | Access is per person, from a graph, and revocations must hold at once | 5%, 10%, 30% readers got 8%, 20%, 62% of k with the filter after the scan |

## Row-level security on its own

Row-level security is a place to put a predicate, not a source of one. If your rule is `tenant_id =
current_setting('app.tenant')`, or a join to a small `role_member` table you already keep, use RLS and stop here:
it is simpler, it is built in, and pg_rebac would add a daemon and SpiceDB for nothing. pg_rebac matters when the
predicate is "every container this person can view through groups, folders and workspace roles", and that answer lives
in SpiceDB. Then the policy still needs an indexed local copy of it, which is the projection; `rebac.enable_rls`
generates the policy over it.

Measured (the alternatives run, p50, every form returned the same rows): for an ANN top 20, a policy cost about the
same as the fence in the query from 5% breadth up, and was faster for narrow readers (49 to 52 ms against 122 ms at
0.1%), where the exact path is the right answer anyway. For anything that reads many rows the policy is slower,
because Postgres plans a policy as a filter on the scan, not a join: counting a 0.1% reader's 501 rows took 0.6 ms
with the fence in the query, 17 ms through `enable_rls` and 46 ms through a naive `exists` policy
([method](https://github.com/louvain-ai/pg_rebac/blob/main/docs/planner.md#row-level-security)).

Know who skips a policy. Postgres's documentation says superusers and `BYPASSRLS` roles always bypass row security,
table owners do unless the table is `FORCE ROW LEVEL SECURITY`, and referential integrity checks always bypass it.
Views planned with their owner's rights, materialized views and `SECURITY DEFINER` functions owned by such a role do
too; `rebac.rls_bypasses('doc')` lists them ([FAQ](/docs/faq/#can-a-role-read-a-fenced-table-without-the-fence)).
A fence written in the query does not depend on any of that.

## Ask SpiceDB on every query

For a single decision (may Priya open this document, may she edit it) call CheckPermission. pg_rebac does not replace
it, and it is not what decides whether a write is allowed. For a list, AuthZed's own guide describes LookupResources as
working "well for moderate result sizes" and "a heavy request that can cause performance problems when more than 10k
results are involved", and recommends post-filtering with CheckBulkPermissions, then Materialize. If your lists are
short and unranked, that is enough.

The trouble is a ranked query. LookupResources streams resource ids, with an optional limit and a cursor; it does
not know your query, so it cannot return the reader's 20 nearest passages. You need the whole set before you can
rank, or you rank first and check after, which is [over-fetching](#over-fetch-then-filter). Every read also waits on
a network call. pg_rebac asks SpiceDB in the background and reads a local table, though it too fails closed
(`RB503`) about 5 s after the daemon loses SpiceDB.

We have not measured LookupResources on a query path. The nearest figure is the sweep run: 2,000 refreshes, each a
LookupResources returning about 1,100 containers plus writing them to Postgres, took 93 s at four at a time, on
SpiceDB's in-memory datastore sharing a laptop with Postgres
([method](https://github.com/louvain-ai/pg_rebac/blob/main/docs/design/projection-scale.md#measured)). That is
refresh throughput, not a read latency.

## Over-fetch, then filter

Take the top N for everyone, drop what the reader cannot see, return k. If every reader sees most of the corpus, this is
fine: at 50% breadth N = 100 filled k in 6.7 ms against 4.9 ms fenced. It breaks as breadth falls, because about N ×
breadth rows survive the filter, so filling k needs N of about k / breadth, and N must be chosen per reader. A fixed N
wastes the broad readers' time and silently starves the narrow ones. Counts, facets and snippets computed before the
filter also describe rows the reader may not see.

Measured (the alternatives run, unfenced HNSW top N, then filtered inside Postgres, so no transfer of N rows to an
application is counted, which flatters it):

| Breadth | Smallest N that filled k | Its p50 | Fenced p50 |
|---|---|---|---|
| 50% | 100 | 6.7 ms | 4.9 ms |
| 10% | 400 | 19.9 ms | 13.0 ms |
| 5% | 1,000 | 43.2 ms | 18.9 ms |
| 1% | 4,000 | 70.3 ms | 42.9 ms |
| 0.1% | none: N = 4,000 kept 13% of k | 73.7 ms | exact path |

At 0.1% filling k would take N of about 20,000, past pgvector's `ef_search` limit of 1,000. Fenced ANN is poor there
too (0.74 of k); the exact path, which `rebac.fenced_ids` takes for narrow readers, returned all k at full recall in
6.7 ms in the 768-dimension study
([method](https://github.com/louvain-ai/pg_rebac/blob/main/docs/planner.md#filtering-after-retrieval)).

## A per-row check function

A function `may_read(user, row)` in a policy or a `where` clause is the first thing most people write. It is right
for one row at a time, which is why pg_rebac ships one, `rebac.readable_by`. It is wrong for a query that touches many
rows, because the planner calls it once per row and cannot turn it into a join. Measured in the alternatives run, a
policy calling `rebac.readable_by` took 3.4 s per `count(*)` at every breadth (about 7 µs a row over 500,000 rows) and
423 ms for an ANN top 20 at 0.1% breadth, against 0.6 ms and 122 ms for the fence in the query. From 5% breadth
up its ANN top 20 was within a few milliseconds of the fence (5.4 against 6.0 ms at 50%), because HNSW hands it few
candidates. That call is local;
a function that calls a remote permission service per row adds a round trip per row, which we did not measure
([method](https://github.com/louvain-ai/pg_rebac/blob/main/docs/planner.md#row-level-security)).

## AuthZed Materialize and the SpiceDB FDW

**Materialize** is AuthZed's precomputation of the permissions you configure, inspired by Zanzibar's Leopard index.
Its docs say it is available on AuthZed Dedicated only. Accelerated Queries, generally available, speed up SpiceDB's
own CheckPermission, CheckBulkPermissions, LookupResources and LookupSubjects. Event Streams, in early access, let you
keep your own copy of the permission sets, "colocated alongside the data it protects in your application
database, or in a secondary index like a search engine", downloaded with LookupPermissionSets and kept current with
WatchPermissionSets. The docs say an application streaming the changes "must be able to tolerate some lag". Caveats,
wildcard subject types and `.all` intersections cannot be on the path of a materialised permission, and expiring
relationships are not supported. If you are on Dedicated and your schema needs a shape pg_rebac
[refuses](/docs/schema-support/#refused-and-why) and Materialize supports, Materialize is the better fit.
(Materialize's limitations page does not list exclusion, intersection or deeper arrows; we have not found a page that
says how it handles them.)

**The SpiceDB Postgres FDW** is a proxy that speaks the Postgres wire protocol and turns each query into SpiceDB calls
(CheckPermission, LookupResources or LookupSubjects). It is in open-source SpiceDB as well as AuthZed's editions, and
its docs call it experimental. Joins between its tables and local tables work; subqueries are not supported, only
equality and `AND` are pushed down, and `ORDER BY` and aggregation run in Postgres after the rows arrive. It is a
convenient way to ask SpiceDB in SQL. It is still a call to SpiceDB per query, so the section above applies, and its
docs point to Materialize for large datasets.

pg_rebac is the third shape: Apache-2.0, on any Postgres 14 or later, a projection Postgres plans as a join, a
revocation written through `rebac.write` removed before its transaction commits, and narrower schema support.

## OpenFGA, Oso and Cerbos

pg_rebac reads SpiceDB only. If your authorization runs on one of these, use what it offers; each turns a policy into
something a database can filter on, in a different way.

- **Cerbos** is stateless. Its PlanResources call returns a condition tree for a principal, a resource kind and an
  action, and your application converts it into a database query, with reference adapters for Prisma, Drizzle,
  SQLAlchemy, Mongoose and others. The attributes the conditions test must be columns you hold. For rules such as
  "owner is me" or "department is mine", that is all you need.
- **Oso Cloud's Local Authorization** generates SQL that your application runs against its own PostgreSQL or MySQL,
  from a YAML mapping of policy facts to queries over your tables; Oso does not read your data. Its docs list
  unsupported logic, including recursion that mixes facts held in Oso with facts in your database.
- **OpenFGA's** guide to searching with permissions offers three patterns: search then batch check; a local index
  built from its changes endpoint, re-checking results because the index can lag revocations; and ListObjects then
  search, which it calls impractical as the list grows, since "a partial list from the API is not enough" to sort.
  The second pattern is what pg_rebac does for SpiceDB, with a revocation written through `rebac.write` applied at
commit rather than re-checked.

## An ACL table you build yourself

If permissions originate in your own database (a `document_share` table your application writes), join it and you
are done. You do not need SpiceDB or pg_rebac.

If they originate in SpiceDB and you mirror them into a table yourself, this is the list of what we had to build to
trust that mirror, and what each costs if it is missing:

- **Revocation at commit.** A share removed in your transaction must leave the mirror before the commit returns, even
  when SpiceDB or the daemon is down, and a refresh racing that commit must not put it back ([G2,
  G3](/docs/concepts/consistency/)).
- **Ordered delivery.** Writes reach SpiceDB through an outbox, in order, revocations first, without one tenant
  waiting behind another.
- **Writes made elsewhere.** A revocation written straight to SpiceDB has to come back through Watch, or reads fail
  closed.
- **Freshness you can state.** Refreshes at least as fresh as the write that queued them, a sweep, and reads that
  fail (`RB503`) once a set is too old, rather than serving it.
- **The schema.** Which relations can change the permission, derived from the live SpiceDB schema, and a refusal for
  shapes that cannot be narrowed soundly.
- **Size.** Group and company-wide grants stored once, not once per member. In the 100,000-person run that was
  302 MB against 26.9 GB for every grant expanded per person
  ([method](https://github.com/louvain-ai/pg_rebac/blob/main/docs/design/projection-scale.md#measured)).
- **Proof.** `rebacd verify` diffs the mirror against SpiceDB and fails on any extra row.

A mirror without these works in a demo, and can leak when a revocation races a refresh.

## A vector store with metadata filters

If access is per tenant, a vector store's own isolation is the simple answer: Pinecone recommends a namespace per
tenant, and pgvector's README suggests a partial index when you filter on a few distinct values and partitioning when
you filter on many. If each document carries a few group tags and readers belong to a few groups, a metadata filter on
the tags can be enough.

Per-person access from a graph is harder. Pinecone's docs say a metadata filter still scans the whole namespace,
limit `$in` to 10,000 values, and call filtering by large lists of user ids an anti-pattern, suggesting groups,
namespaces or client-side post-filtering instead. pgvector's README says that with an approximate index the filter
is applied after the scan, so at 10% selectivity and the default `ef_search` of 40 about 4 rows match, unless
iterative scans (0.8.0 and later) are on. We measured that in the 128-dimension study: with iterative scans off,
readers of 5%, 10% and 30% breadth got 8%, 20% and 62% of k, with no error; with `relaxed_order` they got all k at
recall 0.90 to 0.96 in 4 to 7 ms ([method](https://github.com/louvain-ai/pg_rebac/blob/main/docs/planner.md#results)).
pg_rebac keeps the vectors and the fence in the same Postgres, so a share revoked through `rebac.write` is out of
the next query.

## What pg_rebac costs

To be fair to every alternative above: pg_rebac needs SpiceDB and a daemon, `rebacd`, to run, and reads fail closed
(`RB503`) about 5 s after the daemon stops or loses SpiceDB. A principal's first read is `RB404` until your application
asks the daemon to project it. It projects one permission of one resource type. It refuses exclusion, intersection,
caveats, expiring relationships and arrows more than one hop away. Grants are eventually consistent; only a revocation
written through `rebac.write` is in force at commit, and one written straight to SpiceDB waits for Watch. And every
number on this page is from synthetic data on one machine.

## Sources

Read on 2026-11-25.

- AuthZed, [Materialize overview](https://authzed.com/docs/materialize/getting-started/overview) and
  [limitations](https://authzed.com/docs/materialize/getting-started/limitations)
- AuthZed, [Using Postgres FDW with SpiceDB](https://authzed.com/docs/spicedb/ops/postgres-fdw)
- AuthZed, [Querying data](https://authzed.com/docs/spicedb/concepts/querying-data) (LookupResources and
  CheckBulkPermissions)
- AuthZed, [`LookupResourcesRequest`](https://github.com/authzed/api/blob/main/authzed/api/v1/permission_service.proto)
  (`optional_limit`, `optional_cursor`, a streamed response)
- PostgreSQL, [Row security policies](https://www.postgresql.org/docs/current/ddl-rowsecurity.html)
- pgvector, [README: filtering and iterative index scans](https://github.com/pgvector/pgvector#filtering), and
  [`hnsw.ef_search`'s range, 1 to 1000](https://github.com/pgvector/pgvector/blob/master/src/hnsw.c)
- Pinecone, [Implement multitenancy](https://docs.pinecone.io/guides/index-data/implement-multitenancy)
- OpenFGA, [Search with permissions](https://openfga.dev/docs/interacting/search-with-permissions)
- Oso, [Local Authorization](https://www.osohq.com/docs/authorization-data/local-authorization)
- Cerbos, [Filtering resources by permission](https://docs.cerbos.dev/cerbos/latest/recipes/filtering-resources)
