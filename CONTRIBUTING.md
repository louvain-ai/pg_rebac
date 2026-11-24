# Contributing

Thank you for wanting to improve pg_rebac. It is small on purpose: one file of SQL (`sql/rebac.sql`), one daemon
(`crates/rebacd`) and the tests and studies that hold them to their promises. A change is welcome when it keeps them
that way.

## Find something to work on

Issues labelled [`good first issue`](https://github.com/louvain-ai/pg_rebac/labels/good%20first%20issue) are scoped to
a day or less and say where to start. Before a larger change (a new schema shape, a new mode, a change to what a read
returns), open an issue first so we can agree on the shape before you write it.

## Run it

```sh
docker compose -f examples/quickstart/docker-compose.yml run --rm demo   # see it work, end to end
docker compose up -d --wait                                              # the test stack: Postgres on :55433, SpiceDB on :50061
```

## Before you open a pull request

Every one of these must pass; they are what a reviewer runs.

```sh
REBAC_TEST_URL=postgres://postgres:rebac@127.0.0.1:55433 tests/sql/run.sh    # the SQL suite
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
REBAC_IT=1 cargo test -p rebacd --test integration                           # the daemon against a real SpiceDB
```

- **A change to what a read returns ships with a test that fails without it.** The SQL tests are plain `psql` scripts
  with assertions that raise (`tests/sql/_helpers.sql`); the daemon's are in `crates/rebacd/tests`.
- **The fence fails closed.** A change must never turn an error (`RB400`, `RB404`, `RB503`) into an empty set, or an
  empty set into rows. If a change could, say how the tests show it does not.
- **A claim about speed comes with its measurement.** `docs/planner.md` and `examples/hybrid-pgvector/` show the
  shape: the set-up, the numbers and the script that reproduces them.
- **Docs move with the code.** Update `README.md`, `docs/` and the site (`site/src/content/docs/`) in the same pull
  request when behaviour, a guarantee or a limit changes. `cd site && npm ci && npm run build` must pass.
- Keep a pull request to one concern, and say in its description what it changes and how you checked it.
- Never include credentials, real personal data or a real customer's text in code, tests or fixtures. Use invented
  names.

## Sign your commits (DCO)

Every commit must be signed off under the Developer Certificate of Origin (<https://developercertificate.org/>): by
signing off you certify that you wrote the change, or otherwise have the right to submit it under this repository's
licence. `git commit -s` adds the line:

```
Signed-off-by: Your Name <you@example.com>
```

## Security

Do not open a public issue for a vulnerability. See [SECURITY.md](SECURITY.md).

## Conduct

Everyone taking part is expected to follow the [code of conduct](CODE_OF_CONDUCT.md).
