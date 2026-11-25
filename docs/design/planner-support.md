# Planner support: `pg_rebac_planner`

Status: design, not built. Roadmap item 2 ([`roadmap.md`](roadmap.md)).

## The problem

Since 0.3 a reader's set is the rows of its holders, computed once per query as an InitPlan. The planner cannot see
an InitPlan's value while planning, so it estimates every reader at about ten times the average holder, and a reader
who is broad through a large grant node gets the exact plan from `container in (select * from rebac.readable_set(p))`
([`planner.md`](../planner.md#grant-nodes-03)). `rebac.fenced_ids` routes around it by counting first. A query written
in the bare `in` form has no such router.

## The extension

`pg_rebac_planner` is a separate, optional C extension. A superuser installs it with `create extension
pg_rebac_planner` after `rebac.sql`. Its install script attaches a planner support function to `rebac.holders_of`
and the two `rebac.readable_set` functions (`alter function ... support`, which only a superuser may run). For a call
whose principal is a constant or a parameter, the support function answers the planner's row-count request with that
reader's real breadth: its own `n_containers` plus those of the grant nodes it holds, read from
`rebac.principal_state` and `rebac.node_member`. The planner then picks the exact or the HNSW plan itself.

It changes estimates only. Which rows a read returns, and every check `rebac.checked` makes, are the same with it and
without it.

## What stays the same

- `rebac.sql` never references the extension. The pure-SQL install, on any Postgres 14 or later, is unchanged.
- Re-running `rebac.sql` to upgrade keeps the attachment. `create or replace function` resets a function's support
  function to none, so the extension installs an event trigger on `ddl_command_end` that re-attaches it whenever one
  of those functions is created or replaced; its function is `security definer`, owned by the superuser who installed
  the extension, since attaching needs superuser. The extension's test suite re-runs `rebac.sql` and checks the
  attachment survives.
- `rebac.status()` gains a column saying whether planner support is attached.

## Where it is available

Only where a superuser can install a C extension: self-hosted Postgres. Amazon RDS, Cloud SQL and Supabase allow
only the extensions they ship, so it is not available there; use `rebac.fenced_ids` for vector legs, as without it.
