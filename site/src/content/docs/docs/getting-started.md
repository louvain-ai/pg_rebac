---
title: Getting started
description: Install pg_rebac's SQL, project a first readable set, and fence a pgvector query with it.
---

Everything on this page was run against Postgres 17 with pgvector 0.8.

## See it work in one command

With Docker (Compose 2.23 or later), and nothing to clone:

```sh
curl -fsSL https://pgrebac.com/quickstart.yml | docker compose -f - run --rm demo
```

It starts Postgres with pgvector, SpiceDB (in memory) and the released `rebacd` image, installs the release's SQL
(checked against its SHA-256), writes a small dataset's relationships through `rebac.write`, and waits for the daemon
to relay them to SpiceDB and project two people. Then it runs one vector search as each of them (over three-number embeddings written by
hand, so it needs no model). What you see, abridged:

```text
1. The same vector search, top 5 nearest to "salary pay review", fenced inside the query:

   as alice (reads handbook and payroll)
 container |       title
-----------+-------------------
 handbook  | Annual pay review
 payroll   | Salary bands 2026
 handbook  | Pay days
 payroll   | Payroll cut-off
 payroll   | Overtime pay

   as bob (reads handbook and deals)
 container |       title
-----------+--------------------
 handbook  | Annual pay review
 handbook  | Pay days
 deals     | Commission plan
 deals     | Reseller pay terms
 handbook  | Expenses

2. Filtering AFTER retrieval instead: take the top 5 for everyone, then drop what bob may not read.
   Passages bob cannot read (board) take 4 of the 5 places. bob asked for 5 rows and gets back:
 rows_left_for_bob
-------------------
                 1

3. Revoke bob from deals. The revocation is in force when its transaction commits, before rebacd relays it:
   bob, the same search again
 container |       title
-----------+-------------------
 handbook  | Annual pay review
 handbook  | Pay days
 handbook  | Expenses

4. A principal nobody has asked about is an error (RB404), never an empty set:
   mallory → RB404 "this principal has not been projected" (an error, never an empty set)

You just saw one vector search return different rows per person, fenced inside the query, and a revocation
in force at commit. Stop it and forget everything:
  curl -fsSL https://pgrebac.com/quickstart.yml | docker compose -f - down -v
Docs: https://pgrebac.com/docs/getting-started/
```

The first run pulls four images; after that a run takes about six seconds. Nothing listens on your host, and
`curl -fsSL https://pgrebac.com/quickstart.yml | docker compose -f - down -v` stops it and forgets everything. The file
is generated from [`examples/quickstart/`](https://github.com/louvain-ai/pg_rebac/tree/main/examples/quickstart) by `bundle.sh`; from a clone,
`docker compose -f examples/quickstart/docker-compose.yml run --rm demo` runs the same demo with `rebacd` built from
source.

The rest of this page does the same by hand, without the daemon: install the SQL, project a readable set, fence a
pgvector query.

## Install the SQL

The whole schema is one file of plain SQL and PL/pgSQL. It needs no C extension, so it installs on any Postgres 14
or later, hosted or not. Everything lives in the schema `rebac`.

```sh
psql -v ON_ERROR_STOP=1 -f sql/rebac.sql
```

From a Node application's own migrations, the same SQL is bundled on npm:

```sh
npm i pg-rebac
```

```ts
import { migrate } from "pg-rebac";
await migrate((text) => client.query(text)); // node-postgres; postgres.js: (text) => sql.unsafe(text)
```

Running it again is safe: tables are created `if not exists` and functions are replaced.

**What the install needs.** The role that runs it must be able to create a schema in the database: the database's
owner is enough, and no superuser is needed. pgvector is needed only for fenced vector search, and on hosted Postgres
it is created by the provider's console or an administrator, as for any extension. CI installs as a non-superuser
owner on Postgres 14 to 17, with and without pgvector, beside `pg_stat_statements`, `pgcrypto`, `pg_trgm`, `citext`,
`uuid-ossp` and `btree_gin`, and re-runs the install over itself.

**The daemon's role** needs privileges on the schema `rebac` only, and CI runs the daemon's calls as such a role:

```sql
create role rebacd login password '…';
grant usage on schema rebac to rebacd;
grant select, insert, update, delete on all tables in schema rebac to rebacd;
grant usage, select on all sequences in schema rebac to rebacd;
grant execute on all functions in schema rebac to rebacd;
```

Re-run the grants after an upgrade that adds tables or functions.

**Connection poolers.** Applications can read through PgBouncer in transaction mode; CI runs that with one shared
server connection. Set `rebac.tenant` and `rebac.principal` with `set local` (or `set_config(..., true)`) inside the
transaction that reads: a session-level `set` stays on the server connection and is seen by the next client the pooler
hands it to. `rebac.set_principal`'s binding ends with its transaction, so it is safe there. Connect `rebacd` to
Postgres itself or through a pooler in session mode: its leader lock is a session advisory lock, and through a
transaction pooler that lock belongs to whichever server connection took it.

## A first readable set

In a deployment, the `rebacd` daemon fills the projection from SpiceDB and heartbeats. Reads fail closed when the
heartbeat is older than `max_staleness_ms` (5,000 by default), and when a principal's set was looked up longer ago
than `max_projection_age_ms` (one hour). To try the SQL on its own, turn both bounds off and project a principal by
hand with `rebac.project`, the same entry point the daemon uses:

```sql
-- no daemon in this walkthrough: no heartbeat, and nothing re-projects
update rebac.config set value = '0' where key in ('max_staleness_ms', 'max_projection_age_ms');

-- tenant 'acme', principal 'priya', two containers she may view (looked up now)
select rebac.project('acme', 'priya', array['handbook', 'pricing'], null);
```

Every read names its tenant with a transaction-local setting, then asks for the set:

```sql
begin;
set local rebac.tenant = 'acme';
select * from rebac.readable_set('priya');   -- handbook, pricing
select rebac.breadth('priya');               -- 2
commit;
```

Without a tenant, the read is an error (`RB400`), not an empty set. A principal nobody has asked about is `RB404`:

```text
ERROR:  rebac: this principal has not been projected
HINT:  ask the daemon (POST /lookup) to project it, then retry
```

See [the readable set](/docs/concepts/readable-set/) for the three answers a read can give.

## Fence a pgvector query

`rebac.fenced_ids(principal, table, query vector, k)` returns the k nearest rows the principal may read, fenced
before the ordering and the limit, never after.

```sql
create extension if not exists vector;
create table passage (
  id        bigserial primary key,
  tenant    text not null,
  container text not null,
  body      text not null,
  embedding vector(3) not null
);
create index on passage using hnsw (embedding vector_cosine_ops);
insert into passage (tenant, container, body, embedding) values
  ('acme', 'handbook', 'Leave policy',    '[1,0,0]'),
  ('acme', 'board',    'Board minutes',   '[0.9,0.1,0]'),
  ('acme', 'pricing',  'Price list 2027', '[0.8,0.2,0]');

begin;
set local rebac.tenant = 'acme';
select p.id, p.body
from rebac.fenced_ids('priya', 'passage', '[1,0,0]'::vector, 20, tenant_col => 'tenant') f
join passage p on p.id = f.id::bigint
order by f.distance;
commit;
```

```text
 id |      body
----+-----------------
  1 | Leave policy
  3 | Price list 2027
```

The board minutes are closest to the query and are not returned: priya cannot view `board`.

`rebac.fenced_ids` picks the path by the reader's breadth: below `exact_below_rows` candidate rows (5,000) it sorts
them exactly, with full recall; above it, it runs the HNSW scan with `hnsw.iterative_scan = relaxed_order` and a
fence probe per candidate, so it returns all k rows ([measured](/docs/performance/)). For a filter that is not a
vector ordering, put the set in your own `where` clause: `container in (select * from rebac.readable_set('priya'))`.

## Next

- Narrow readers are faster and complete on the exact path: [breadth and the two paths](/docs/concepts/breadth/).
- Keep the projection in step with SpiceDB: [rebacd](/docs/rebacd/).
- Write relationships through `rebac.write` so revocations are in force at commit:
  [revocations and consistency](/docs/concepts/consistency/).
- Retrieval with the fence built in: [use it with Louvain Engine](#use-it-with-louvain-engine).

## Use it with Louvain Engine

[Louvain Engine](https://github.com/louvain-ai/louvain-engine) is open-source, permission-aware hybrid retrieval for
company knowledge. Its `@louvain/engine-pg-rebac` binding takes each reader's readable set from pg_rebac, so SpiceDB
decides who reads which collection: the engine runs its pgvector and keyword legs narrowed to that set, re-checks every
row, and fuses them. A reader pg_rebac was never asked about is looked up through `rebacd` once; an unavailable
projection fails the search instead of returning less.

The engine's [`examples/pg-rebac`](https://github.com/louvain-ai/louvain-engine/tree/main/examples/pg-rebac) runs the
whole stack (the engine, Postgres, pg_rebac's SQL, SpiceDB and `rebacd`) from its repository root, with Docker,
Node 22 and pnpm 10:

```sh
git clone https://github.com/louvain-ai/louvain-engine && cd louvain-engine
examples/pg-rebac/run.sh
```

```text
"salary review" as alice:
  1. salary-bands-2027
  2. payroll-cut-off
  3. annual-pay-review

"salary review" as bob:
  1. annual-pay-review

"salary review" as mallory:
  (nothing this person may read)
```
