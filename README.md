# pg_rebac

**Search everything. Return only what's authorized.**

Native relationship-based authorization for PostgreSQL, pgvector and AI retrieval: SpiceDB permissions, enforced
inside the Postgres plan, before the `order by` and the `limit`.

- **Fenced ANN in 4 to 7 ms at 5% to 50% reader breadth**, all k rows at recall 0.90 to 0.96: p50 of 10 queries,
  one client, 500,000 synthetic 128-dimension passages, warm cache, through the `in` form, before 0.1.0
  (`rebac.fenced_ids`, which 0.3 uses, at 100,000 people: below). At 768 dimensions with skewed containers the
  same readers took 3 to 29 ms (p99 5 to 61 ms), on synthetic data where recall says little (unfenced HNSW scored
  0.09 too). Method and limits: [the studies](docs/planner.md).
- **No remote authorization call on a warm read.** A projected principal's fenced query reads local tables only.
  The first read of a principal asks the daemon once (`RB404`, then retry).

[![Licence: Apache-2.0](https://img.shields.io/badge/licence-Apache--2.0-4a3fd0)](LICENSE)
[![Postgres 14+](https://img.shields.io/badge/postgres-14%2B-336791)](sql/rebac.sql)
[![Docs: pgrebac.com](https://img.shields.io/badge/docs-pgrebac.com-4a3fd0)](https://pgrebac.com)

<p align="center">
  <img src="docs/img/two-readers.svg" width="760" alt="The same vector search run as two people. alice, who reads handbook and payroll, gets five rows from those two containers; bob, who reads handbook and deals, gets five rows from his. Filtered after retrieval instead, bob would get one row of five, because rows from a container he cannot read (board) take the other four places.">
</p>

SpiceDB stays the authority for who may read what. pg_rebac keeps a local, indexed projection of one permission, and
gives every query the same fence. For a vector search, the k nearest rows a person may read:

```sql
set local rebac.tenant = 'acme';
select p.id, p.body
from rebac.fenced_ids('priya', 'passage', $1, 20) f
join passage p on p.id = f.id::bigint
order by f.distance;
```

`rebac.fenced_ids` counts the reader's candidate rows and sorts them exactly when there are few, or runs the HNSW
scan with a fence probe per candidate when there are many (see "Vector search" below). For any other filter, put
the set in the `where` clause:

```sql
select * from ticket
where container in (select * from rebac.readable_set('priya'))
  and status = 'open';
```

Postgres inlines `rebac.readable_set` into a semi-join on the projection's primary key. So the fence sits inside the
same plan as a full-text match, a join or anything else, and applies before the order and the limit.

## Try it in one command

With Docker (Compose 2.23 or later), and nothing to clone:

```sh
curl -fsSL https://pgrebac.com/quickstart.yml | docker compose -f - run --rm demo
```

It starts Postgres with pgvector, SpiceDB (in memory) and the released `rebacd` image, installs the release's SQL
(checked against its SHA-256), writes a small dataset's relationships through `rebac.write`, and waits for the daemon to
relay them to SpiceDB and project two people. Then it runs one vector search as each of them (over three-number
embeddings written by hand, so it needs no model). What you see, abridged:

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
   mallory → RB404 "this principal has not been projected"

You just saw one vector search return different rows per person, fenced inside the query, and a revocation
in force at commit. Stop it and forget everything:
  curl -fsSL https://pgrebac.com/quickstart.yml | docker compose -f - down -v
Docs: https://pgrebac.com/docs/getting-started/
```

The first run pulls four images; after that a run takes about six seconds. Nothing listens on your host, and the
`down -v` line it prints removes it. The file is generated from [`examples/quickstart/`](examples/quickstart/) by
`bundle.sh`; from a clone, `docker compose -f examples/quickstart/docker-compose.yml run --rm demo` runs the same demo
with `rebacd` built from source.

**Next steps:**

- Install the SQL into your own Postgres 14+: `psql -v ON_ERROR_STOP=1 -f sql/rebac.sql`, or `npm i pg-rebac` and
  run its `migrate` from your migrations ([Install on your own Postgres](#install-on-your-own-postgres)).
- Run the daemon: `docker pull ghcr.io/louvain-ai/rebacd`, or `cargo install rebacd`.
- Read [Getting started](https://pgrebac.com/docs/getting-started/), then fence your first pgvector query.

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

## Why not just filter results after retrieval?

Because a search engine's `limit` runs before your filter, and everything it computed on the way leaks.

- **Top-k starvation.** Fetch the top 20, drop what the user may not read, and a user who reads 5% of the corpus gets
  about one row. [`docs/planner.md`](docs/planner.md) measures the in-database version of the same mistake (pgvector's
  HNSW scan with `hnsw.iterative_scan = off`, so the filter only sees the scan's first candidates) on 500,000
  passages: readers of 5%, 10% and 30% breadth got **8%, 20% and 62% of k**, with no error. The quickstart shows it on
  seventeen rows: bob asks for five and gets one.
- **Over-fetching to compensate costs latency and still has no floor.** To fill k rows for a reader of breadth b you
  must fetch about k/b candidates and hope; a narrow reader needs hundreds of times k. Fenced inside the plan with
  `relaxed_order`, readers of 5% to 50% breadth got **all k rows at recall 0.90 to 0.96 in 4 to 7 ms**, against
  1.9 ms for the same HNSW query with no fence at all. A reader below about 1% breadth takes the exact path,
  `container = any(array(select * from rebac.readable_set(p)))`: **1.6 ms with full recall** at 0.1% breadth, where
  relaxed ANN took about 100 ms for recall 0.54.
- **Counts, snippets and rankings leak.** Anything computed before the filter (a "1,240 results" total, a highlighted
  snippet, a facet count, a "did you mean", a page offset) was computed over rows the user may not read, and says
  something about them. A fence in the `where` clause means no such row is ranked, counted or returned.
- **Failure has to be an answer, not an empty page.** A stale projection is `RB503`, a principal nobody has asked
  about is `RB404`, and a missing tenant is `RB400`. None returns rows it has not checked.

Row-level security, SpiceDB per query, AuthZed Materialize, OpenFGA, Oso, Cerbos, a table of your own and vector-store
filters are each the better choice somewhere: [Why not X?](https://pgrebac.com/docs/why-not/) says where.

## At Louvain

Louvain mirrors every permission change into pg_rebac and verifies it against SpiceDB daily: its permission
projection is written into pg_rebac's tables in each writer's transaction, a shadow `rebacd` re-projects every reader
from SpiceDB, and `rebacd verify --all-tenants --shadow` compares the two and fails on any extra row.

## What you get

- **One read primitive.** `rebac.readable_set(principal)` is the containers a principal may view, optionally
  `within` a list (an intersection, which can only narrow). `rebac.readable_by(principal, container)` is a per-row
  check, for single lookups.
- **Three different answers.**
  - `RB404` means the principal was never asked: call the daemon's `POST /lookup`, then retry.
  - An empty set means the principal was asked and can read nothing.
  - `RB503` means unavailable: the daemon is not live, the principal's set is too old or of unknown age, it is over
    its cap, or SpiceDB could not be asked.

  A missing `rebac.tenant` is an error too, `RB400`.
- **Revocations in force at commit.** Write relationships with `rebac.write(op, resource, relation, subject)` in
  your own transaction. A deferred trigger removes every revocation it queued from the projection before the
  transaction commits, whether or not SpiceDB or the daemon is up. This holds for direct relations, arrows, subject
  sets and wildcards, including a refresh racing the commit, and needs READ COMMITTED: a revocation in a REPEATABLE READ
  or SERIALIZABLE transaction is refused (`RB422`). See [`docs/guarantees.md`](docs/guarantees.md).
- **Group and company-wide grants stored once.** A grant to a subject set or a wildcard is one row per container,
  held by its grant node, which each member reads through at query time. The projection grows with grants, not with
  people times containers: at 100,000 people, 302 MB where per-principal rows took 26.9 GB
  ([measured](docs/design/projection-scale.md)).
- **Row-level security, if you want it.** `rebac.enable_rls(table, column)` generates a restrictive policy over the
  readable set, and `rebac.rls_bypasses(table)` lists every role and view that reads the table without it.
- **Two bounds, both failing closed (`RB503`).**
  - *Liveness*: the daemon has heartbeated within `max_staleness_ms` (default 5000). It beats only while it can
    reach both SpiceDB and Postgres. This proves the daemon is up, not that any set is current.
  - *Freshness*: the SpiceDB lookup behind this principal's set began within `max_projection_age_ms` (default one
    hour, above the daemon's 15-minute sweep, which re-projects everyone). A set of unknown age is never read.
    `rebac.freshness()` reports the oldest lookup per tenant.
- **The daemon, `rebacd`.** It:
  - relays the outbox to SpiceDB in order, revocations first, without letting one tenant wait behind another, and
    quarantines a row SpiceDB refuses on its own;
  - refreshes principals at least as fresh as the write that queued them;
  - backfills a tenant from ReadRelationships and LookupResources;
  - re-sends each recent revocation once from a sweep;
  - applies SpiceDB's Watch, so a revocation written straight to SpiceDB is narrowed as it is delivered, or reads
    fail closed (G10);
  - derives the relations that can change the permission from the SpiceDB schema itself, and refuses a schema it
    cannot narrow soundly;
  - re-derives them whenever the live schema changes: a change to what grants view fails every read closed
    (`RB503`) until every tenant has been rebuilt under the new closure;
  - runs as several daemons on one database: one leads (an advisory lock, every Watch write fenced by it) and runs
    Watch, the closure check and the sweep, and every daemon relays, refreshes and serves lookups;
  - serves `POST /lookup`, `GET /status` and `GET /metrics` (Prometheus, counts and ages only), behind a bearer
    token when `lookup_token_env` names one.
- **`rebacd verify`.** It diffs the projection against SpiceDB, across tenants, against the schema's closure, or
  against a shadow projection. It reports extra rows (a violation, exit 2) and missing rows (freshness) separately,
  as counts only.

## Install on your own Postgres

```sh
# 1. The schema, on any Postgres 14+ (no extension needed):
psql -v ON_ERROR_STOP=1 -f sql/rebac.sql
#    or from your own migrations, with the SQL bundled for Node: npm i pg-rebac

# 2. The daemon:
cargo build --release -p rebacd
DATABASE_URL=postgres://… SPICEDB_TOKEN=… ./target/release/rebacd --config rebac.toml run
```

The install needs a role that can create a schema (a database owner is enough) and no superuser. Through PgBouncer
in transaction mode, set `rebac.tenant` and `rebac.principal` with `set local`; connect `rebacd` directly or through a
session-mode pooler. [Getting started](https://pgrebac.com/docs/getting-started/#install-the-sql) lists the daemon
role's grants.

To upgrade, run the new release's SQL over the old one, then restart `rebacd` at the same release. Every statement
applies in place and keeps the projection; the [FAQ](https://pgrebac.com/docs/faq/#how-do-i-upgrade) says what each
upgrade re-projects, and from 0.2 to 0.3 reads stay open throughout.

TLS to SpiceDB: an `https://` endpoint is verified against the public roots or `spicedb_ca_file`; a plaintext
`http://` one is refused at a public address unless `spicedb_insecure = true`, since it would send the SpiceDB key in
the clear.

TLS to Postgres follows the connection string's `sslmode`, with libpq's meanings:

- `verify-full` checks the chain (the public roots, or `sslrootcert=<pem file>`) and the host name. Use it for a
  hosted Postgres.
- `require` encrypts without verifying the server.
- `prefer` (the default) takes TLS when it is offered.
- `disable` is plaintext.
- `verify-ca` is refused.

`rebac.toml` holds connections and tuning only (see `crates/rebacd/src/config.rs`). What is projected (the resource
type, the permission, the tenant separator) lives in the `rebac.config` table, which the SQL and the daemon both read.

## Supported schemas

The projected permission, `container#view` by default, may be any union (`+`) of:

- direct relations;
- a self-arrow (`parent->view`);
- an arrow to another type and a relation there (`workspace->admin`);
- subject sets one level deep (`group#member`, where `group#member` holds plain object types);
- wildcards (`user:*`) on a relation of the projected type: a wildcard grant is stored once, for everyone, and its
  removal narrows every holder of the container.

Refused, each with its reason:

- exclusion (`-`): adding a tuple to its subtracted side revokes, and only removals narrow;
- intersection (`&`): narrowing keeps a container another granted relation still holds, which is right for a union only;
- the intersection arrow (`.all()`): removing one of its tuples can grant;
- caveats and expiring relationships;
- wildcards on a relation reached through an arrow, or inside a subject set;
- nested subject sets;
- arrows more than one hop away.

## Vector search

Use `rebac.fenced_ids(p, tbl, q, k)`. It counts the reader's candidate rows up to `exact_below_rows` (default
5,000) and sorts them exactly below it, with full recall, or runs the HNSW scan with `relaxed_order` and a fence probe
per candidate above it, so every reader gets all k rows on the path that is fast for its breadth.

- At 100,000 people (100,000 containers, 768 dimensions, k = 20), it answered a reader of 1,125 containers in 13.7 ms
  p50 (exact, recall 1.0) and a reader of 21,000 in 20.1 ms (ANN, recall 0.965)
  ([`docs/planner.md`](docs/planner.md#measured-at-100000-people)).
- Without `relaxed_order` a fenced HNSW scan silently returns fewer than k rows: 8% of k at 5% breadth, 20% at 10%,
  62% at 30%, on the planner study's 500,000 passages.
- Narrow readers belong on the exact path: below about 5,000 candidate rows it is both faster and complete (4.9 ms
  at 500 rows against 85.7 ms for ANN at recall 0.60). `fenced_ids` takes it for them.

## Tests

```sh
docker compose up -d --wait                           # Postgres (pgvector) on :55433, SpiceDB on :50061
REBAC_TEST_URL=postgres://postgres:rebac@127.0.0.1:55433 tests/sql/run.sh
cargo test && cargo clippy --all-targets
REBAC_IT=1 cargo test -p rebacd --test integration    # the daemon against the real SpiceDB
REBAC_TLS_IT=1 REBAC_TLS_CA=$(tests/tls/up.sh) cargo test -p rebacd --test tls   # sslmode against a TLS Postgres
```

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs on every push to main and every pull request:

- `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test`;
- the SQL suite on Postgres 14, 15, 16 and 17 with pgvector, as a superuser and as a database owner that is not one;
- 20 rounds of the revocation races (`06`, `09`, `13`, `19`) and the held-lock fault, and the upgrade from 0.2.0;
- the SQL through PgBouncer in transaction mode;
- the daemon against a real SpiceDB (`REBAC_IT=1`), and SpiceDB as an oracle over randomised schemas;
- the quickstart both ways, checked by `examples/quickstart/check-output.sh`: the file pgrebac.com serves (which must be
  what `examples/quickstart/bundle.sh` writes) and the demo built from the commit.

The TLS test runs locally only.

## Website

The cover page and docs live in [`site/`](site/) (Astro Starlight): `cd site && npm ci && npm run dev`.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Report a vulnerability privately, as [SECURITY.md](SECURITY.md)
describes, never in a public issue.

## Licence

Apache-2.0. See `LICENSE` and `NOTICE`. The vendored SpiceDB API protos are Apache-2.0 (Authzed); see
`crates/rebacd/proto/LICENSE-authzed-api`.
