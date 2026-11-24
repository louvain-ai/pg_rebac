---
title: The readable set
description: One read primitive, inlined by the planner, with three distinct answers.
---

`rebac.readable` is a local, per-tenant, per-principal projection of one SpiceDB permission, `container#view` by
default. One row is one container a principal may view. Its primary key is `(tenant, principal, container)`, and
a second index on `(tenant, container)` serves narrowing.

SpiceDB stays the authority. The projection is a cache of it, minus pending revocations, and is never a record.

## The functions

| Function | Returns | Use it for |
|---|---|---|
| `rebac.readable_set(p)` | `setof text` | Fencing a scan: `container in (select * from rebac.readable_set(p))` |
| `rebac.readable_set(p, within)` | `setof text` | The same, intersected with a list you resolved. It can only narrow. |
| `rebac.readable_by(p, c)` | `boolean` | One lookup of one container. Evaluated per row, so not for scans. |
| `rebac.breadth(p)` | `integer` | How many containers `p` reads, to choose a [path](/docs/concepts/breadth/). |
| `rebac.version(p)` | `bigint` | A version that moves whenever `p`'s set changes, for cache keys. |

The tenant always comes from `set local rebac.tenant = '…'`.

## Why a set and not a boolean

Postgres never inlines a scalar SQL function whose body has a sub-select, so a boolean predicate written as
`exists (...)` runs once per row. A set-returning SQL function used in `from` is inlined. Measured with
`EXPLAIN (VERBOSE)`, `container in (select * from rebac.readable_set(p))` becomes a semi-join on `readable_pkey`.

The principal's checks (tenant, liveness, the set's freshness, state) run once, as a pseudo-constant qual the planner places in a
one-time gating filter, before `rebac.readable` is scanned. The principal is compared as the caller's own value,
so the planner can use its statistics.

```text
 Limit
   InitPlan 1
     ->  Result
   InitPlan 2
     ->  Result
   ->  Result
         One-Time Filter: (InitPlan 1).col1
         ->  Nested Loop
               ->  Index Scan using passage_embedding_idx on passage
                     Order By: (embedding <=> '[1,0,0]'::vector)
               ->  Index Only Scan using readable_pkey on readable r
                     Index Cond: ((tenant = (InitPlan 2).col1) AND (principal = 'ana'::text)
                                  AND (container = passage.container))
```

The test suite asserts this shape: the fenced vector leg uses the HNSW index with a `readable_pkey` probe.

## Three answers, never an accidental empty set

| Answer | What it means | What to do |
|---|---|---|
| `RB404` `rebac_not_projected` | This principal was never asked. | Call the daemon's `POST /lookup`, then retry. |
| An empty set | Asked, and can read nothing. Believed for `empty_ttl_ms` (60,000 by default). | Nothing: it is the answer. |
| `RB503` `rebac_unavailable` | The projection cannot be trusted now: the daemon is not live, the set is too old or of unknown age, it is over its cap, or SpiceDB could not be asked. | Fail the read. |

Two more codes complete the set. `RB400` is a missing tenant or principal; a read without one is a bug. `RB422` is
a write the fence refuses, such as a reference that does not carry the tenant's prefix.

A principal over `max_containers_per_principal` (100,000 by default) is marked unavailable with reason `over_cap`.
Its set is never truncated, because a truncated set is a wrong fence.

## Principals and subjects

The principal is your application's notion of a reader. By default it reads as the SpiceDB subject
`user:<tenant>/<principal>`. Rows in `rebac.subject` add more subjects, such as a linked chat identity, and only rows
marked `trusted` count. A principal reads as the union of its subjects.

`rebac.retire(tenant, principal)` makes a principal read nothing, whatever SpiceDB holds, and no refresh can grow it
back. `rebac.unretire` queues it for a fresh projection.

## Row-level security

`rebac.enable_rls(table, container_column)` generates a policy that fences `select` on a table, with the principal
read from a transaction-local setting (`rebac.principal` by default). It refuses a table the current role owns,
since an owner bypasses row-level security, unless you pass `force => true`, which adds
`FORCE ROW LEVEL SECURITY`.
