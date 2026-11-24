# pg_rebac

**Search everything. Return only what's authorised.**

Native relationship-based authorisation for PostgreSQL, pgvector and AI retrieval: SpiceDB permissions, enforced
inside the Postgres plan, before the `order by` and the `limit`.

- **Fenced ANN in 4 to 7 ms at 5% to 50% reader breadth**, all k rows at recall 0.90 to 0.96. That is p50 of 10
  queries, one client, on 500,000 synthetic 128-dimension passages with a warm cache. A 768-dimension run with
  skewed containers took 3 to 29 ms (p99 5 to 61 ms), with concurrency measured too; see
  [the studies and their limits](docs/planner.md).
- **No remote authorisation call on a warm read.** A projected principal's fenced query reads local tables only.
  The first read of a principal asks the daemon once (`RB404`, then retry).

[![Licence: Apache-2.0](https://img.shields.io/badge/licence-Apache--2.0-4a3fd0)](LICENSE)
[![Postgres 14+](https://img.shields.io/badge/postgres-14%2B-336791)](sql/rebac.sql)
[![Status: pre-release](https://img.shields.io/badge/status-pre--release-7a8494)](#status)
[![Docs: pgrebac.com](https://img.shields.io/badge/docs-pgrebac.com-4a3fd0)](https://pgrebac.com)

<p align="center">
  <img src="docs/img/two-readers.svg" width="760" alt="The same full-text query run as two people. alice, who reads handbook and payroll, gets five rows from those two containers; bob, who reads handbook and deals, gets five rows from his. Filtered after retrieval instead, bob would get one row of five, because rows from containers he cannot read (board and payroll) take the other four places.">
</p>

SpiceDB stays the authority for who may read what. pg_rebac keeps a local, indexed projection of one permission, and
gives every query the same fence:

```sql
set local rebac.tenant = 'acme';
set local hnsw.iterative_scan = relaxed_order;   -- see "Vector search" below
select id from passage
where container in (select * from rebac.readable_set('priya'))
order by embedding <=> $1
limit 20;
```

Postgres inlines `rebac.readable_set` into a semi-join on the projection's primary key. So the fence sits inside the
same plan as a pgvector ordering, a full-text match or anything else, and applies before the order and the limit.

## Try it in one command

You need Docker with Compose. From a clone:

```sh
docker compose -f examples/quickstart/docker-compose.yml run --rm demo
```

It starts Postgres with pgvector, SpiceDB (in memory) and the `rebacd` daemon (built from this repository), installs
`sql/rebac.sql`, seeds a small dataset through `rebac.write`, waits for the daemon to relay it to SpiceDB and project
two people, then shows:

1. the same search as `alice` and as `bob`, each getting five rows from what they may read;
2. the same search filtered after retrieval instead, where `bob` gets one row of five;
3. a revocation in force when its transaction commits, before the daemon has relayed it;
4. a principal nobody has asked about failing with `RB404`, never an empty set.

The first run pulls the Rust, Postgres and SpiceDB base images and builds the daemon (42 s for the build on a
10-core laptop, measured from a fresh clone with no build cache); after that a run takes about five seconds. Every file it uses is in
[`examples/quickstart/`](examples/quickstart/). `docker compose -f examples/quickstart/docker-compose.yml down` stops it
and forgets everything; nothing listens on your host.

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
  something about them. A fence in the `where` clause means no such row is ranked, counted or returned. (This point is
  about the shape of the query, not a measurement.)
- **Failure has to be an answer, not an empty page.** Here a stale projection is `RB503`, a principal nobody has asked
  about is `RB404`, and a missing tenant is `RB400`; none of them is an empty set by accident, and none of them returns
  rows it has not checked.

## Status

Slice 1, pre-release. It has the pure-SQL mode, the `rebacd` daemon, and the tests and studies below.

## What you get

- **One read primitive.** `rebac.readable_set(principal)` is the containers a principal may view, optionally
  `within` a list (an intersection, which can only narrow). `rebac.readable_by(principal, container)` is a per-row
  check, for single lookups.
- **Three different answers.**
  - `RB404` means the principal was never asked: call the daemon's `POST /lookup`, then retry.
  - An empty set means the principal was asked and can read nothing.
  - `RB503` means unavailable: the daemon is not live, the principal's set is too old or of unknown age, it is over
    its cap, or SpiceDB could not be asked.
  - A missing `rebac.tenant` is `RB400`. None of these is ever an empty set by accident.
- **Revocations in force at commit.** Write relationships with `rebac.write(op, resource, relation, subject)` in
  your own transaction. A deferred trigger removes every revocation it queued from the projection before the
  transaction commits, whether or not SpiceDB or the daemon is up. This holds for direct relations, arrows and
  subject sets, including a refresh racing the commit, and needs READ COMMITTED: a revocation in a REPEATABLE READ
  or SERIALIZABLE transaction is refused (`RB422`). See [`docs/guarantees.md`](docs/guarantees.md).
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
  - follows Watch to count writes made outside pg_rebac (observe-only in slice 1);
  - derives the relations that can change the permission from the SpiceDB schema itself, and refuses a schema it
    cannot narrow soundly;
  - re-derives them whenever the live schema changes: a change to what grants view fails every read closed
    (`RB503`) until every tenant has been rebuilt under the new closure.
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

TLS to Postgres follows the connection string's `sslmode`, with libpq's meanings:

- `verify-full` checks the chain (the public roots, or `sslrootcert=<pem file>`) and the host name. Use it for a
  hosted Postgres.
- `require` encrypts without verifying the server.
- `prefer` (the default) takes TLS when it is offered.
- `disable` is plaintext.
- `verify-ca` is refused.

`rebac.toml` holds connections and tuning only (see `crates/rebacd/src/config.rs`). What is projected (the resource
type, the permission, the tenant separator) lives in the `rebac.config` table, which the SQL and the daemon both read.

## Supported schemas (slice 1)

The projected permission, `container#view` by default, may be any union (`+`) of:

- direct relations;
- a self-arrow (`parent->view`);
- an arrow to another type and a relation there (`workspace->admin`);
- subject sets one level deep (`group#member`, where `group#member` holds plain object types).

Refused, each with its reason:

- exclusion (`-`): removing a tuple can grant;
- intersection (`&`, `.all()`);
- caveats and expiring relationships;
- wildcards;
- nested subject sets;
- arrows more than one hop away.

## Vector search

[`docs/planner.md`](docs/planner.md) measures a fenced HNSW ordering at eight reader breadths, with 500,000 passages
and pgvector 0.8:

- Readers of 5% to 50% breadth get all k rows at recall 0.90 to 0.96 in 4 to 7 ms, provided
  `hnsw.iterative_scan = relaxed_order` is set.
- Without it, the fence silently returns fewer than k rows.
- Readers below about 1% breadth belong on the exact path, `container = any(array(select * from
  rebac.readable_set(p)))`. Choose it with `rebac.breadth(p)`.

## Tests

```sh
docker compose up -d --wait                           # Postgres (pgvector) on :55433, SpiceDB on :50061
REBAC_TEST_URL=postgres://postgres:rebac@127.0.0.1:55433 tests/sql/run.sh
cargo test && cargo clippy --all-targets
REBAC_IT=1 cargo test -p rebacd --test integration    # the daemon against the real SpiceDB
REBAC_TLS_IT=1 REBAC_TLS_CA=$(tests/tls/up.sh) cargo test -p rebacd --test tls   # sslmode against a TLS Postgres
```

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs on every push to main and every pull request:

- `cargo clippy -D warnings` and `cargo test`;
- the SQL suite on Postgres 14, 15, 16 and 17 with pgvector;
- 20 rounds of the revocation-versus-refresh races and the held-lock fault (`tests/sql/run.sh '0[69]_*'`);
- the daemon against a real SpiceDB (`REBAC_IT=1`).

The TLS test runs locally only.

## Website

The cover page and docs live in [`site/`](site/) (Astro Starlight): `cd site && npm ci && npm run dev`.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Report a vulnerability privately, as [SECURITY.md](SECURITY.md)
describes, never in a public issue.

## Licence

Apache-2.0. See `LICENSE` and `NOTICE`. The vendored SpiceDB API protos are Apache-2.0 (Authzed); see
`crates/rebacd/proto/LICENSE-authzed-api`.
