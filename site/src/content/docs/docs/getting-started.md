---
title: Getting started
description: Install pg_rebac's SQL, project a first readable set, and fence a pgvector query with it.
---

pg_rebac is slice 1 and pre-release. Everything on this page was run against Postgres 17 with pgvector 0.8.

## See it work in one command

With Docker and a clone of the repository:

```sh
docker compose -f examples/quickstart/docker-compose.yml run --rm demo
```

It starts Postgres with pgvector, SpiceDB in memory and `rebacd`, installs `sql/rebac.sql`, writes a small dataset's
relationships through `rebac.write`, waits for the daemon to project two people, and runs one search as each of them.
It then shows the same search filtered after retrieval, a revocation in force at commit, and `RB404` for a principal
nobody has asked about. `docker compose -f examples/quickstart/docker-compose.yml down` forgets it all. The rest of
this page does the same by hand, without the daemon.

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

## A first readable set

In production the `rebacd` daemon fills the projection from SpiceDB and heartbeats. Reads fail closed when the
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

Put the fence in the `where` clause of the query you already have. Postgres inlines `rebac.readable_set` into a
semi-join on the projection's primary key, so it applies before the `order by` and the `limit`.

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
set local hnsw.iterative_scan = relaxed_order;
select id, body from passage
where container in (select * from rebac.readable_set('priya'))
order by embedding <=> '[1,0,0]'
limit 20;
commit;
```

```text
 id |      body
----+-----------------
  1 | Leave policy
  3 | Price list 2027
```

The board minutes are closest to the query and are not returned: priya cannot view `board`.

Set `hnsw.iterative_scan = relaxed_order` on every fenced HNSW read. Without it, pgvector stops after `ef_search`
candidates and the fence silently returns fewer than k rows ([measured](/docs/performance/)).

## Next

- Narrow readers are faster and complete on the exact path: [breadth and the two paths](/docs/concepts/breadth/).
- Keep the projection in step with SpiceDB: [rebacd](/docs/rebacd/).
- Write relationships through `rebac.write` so revocations are in force at commit:
  [revocations and consistency](/docs/concepts/consistency/).
