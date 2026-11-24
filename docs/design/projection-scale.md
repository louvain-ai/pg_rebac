# Projection scale: one row per (principal, container), and a group-level design

Status: a design, not built. Nothing in 0.2 behaves as this page describes; [`guarantees.md`](../guarantees.md) is
what the code does.

## What a row costs

`rebac.readable` stores one row per (tenant, principal, container) the principal may view, with two B-trees: the
primary key, which the fence probes, and `(tenant, container)`, which narrowing uses. Measured on Postgres 17 with
871,961 rows (the planner study's projection, freshly loaded, no bloat):

| ids | heap | indexes | per row |
|---|---|---|---|
| short (`acme`, ~5-character ids) | 44 MB | 55 MB | 114 B |
| UUID-length (11-character tenant, 36-character ids) | 102 MB | 141 MB | 279 B |

A second measurement of 2 million rows with UUID-text ids, inserted principal by principal, came to 300 B/row as
inserted and 259 B/row after `REINDEX`. In it the `(tenant, container)` index was only 8 to 10 B/row, because B-tree
deduplication collapses 100 readers per container; it grows as readers per container fall. Size grows linearly in
rows. Updates add dead tuples on top until vacuum runs, so budget more than this. These are estimates from measured
row widths, not a run at the scale below.

## 100,000 people and 10,000,000 documents

The number of rows is people times the containers each one reads, so the answer depends on what a container is:

| container | containers a reader reads (mean) | rows | size at 279 B/row |
|---|---|---|---|
| a space or folder (10M documents in 50,000 containers) | 1,000 | 1.0 × 10⁸ | 28 GB |
| a document, sharing mostly per team | 10,000 (0.1%) | 1.0 × 10⁹ | 279 GB |
| a document, 1% of documents shared company-wide | 100,000+ | ≥ 1.0 × 10¹⁰ | ≥ 2.8 TB |

By docs per container, at 100,000 principals and 260 to 300 B/row:

| containers | docs per container | breadth | per reader | rows | size |
|---|---|---|---|---|---|
| 100k | 100 | 1% | 1,000 | 10⁸ | 26 to 30 GB |
| 100k | 100 | 5% | 5,000 | 5 × 10⁸ | 130 to 150 GB |
| 1M | 10 | 1% | 10,000 | 10⁹ | 260 to 300 GB |
| 1M | 10 | 5% | 50,000 | 5 × 10⁹ | 1.3 to 1.5 TB |
| 10M | 1 | 1% | 100,000 | 10¹⁰ | 2.6 to 3 TB, and at the cap |
| 10M | 1 | 5% | 500,000 | | over the cap: every read RB503 |

A single company-wide container costs only 100,000 rows (about 30 MB). What costs is the number of containers each
reader reads. The first rows work on one Postgres. A per-document container at 1% breadth does not:

- every company-wide document costs 100,000 rows;
- every reader is over `max_containers_per_principal` (100,000), so it fails closed with `over_cap`, which is
  correct but useless;
- a set grant's removal deletes 100,000 rows under the tenant lock, and a refresh rewrites a reader's 100,000
  containers.

## The sweep is the other bound

Before storage runs out, re-projection does. The sweep re-projects every principal every 15 minutes. At 100,000
principals and `refresh_concurrency` 4, each LookupResources plus diff must average at most 36 ms to fit the interval,
or 144 ms to fit the one-hour freshness bound. The churn study's database-side refresh of a 5,000-container set took
about 37 ms before the SpiceDB call. At 10,000 containers per reader, a sweep streams 10⁹ rows from SpiceDB every 15
minutes. With Watch applying revocations (G10), the sweep is a backstop, so its interval can be raised along with
`max_projection_age_ms`. That still re-reads everything per person. The design below also cuts this: a group's grant
is looked up once per group, not once per member.

## Where the size comes from

The blow-up comes from grants to groups and to everyone being multiplied out per person. Per-person grants are
cheap. A company-wide share expanded per reader is what costs.

## The design: store a group's grant once, expand it at read time

Split the projection by where a grant comes from:

- `readable_direct (tenant, principal, container)`: what a principal reads through relations that name them. This is
  today's table, restricted to direct grants.
- `node_readable (tenant, node, container)`: what a GRANT NODE reads. A node is a subject set (`group:eng#member`),
  the target of an arrow (`workspace:w1#admin`), or a wildcard (`user:*`). It is looked up once per node with
  LookupResources on the node as subject, and it holds the containers under it through self-arrows.
- `node_member (tenant, node, principal)`: who stands for a node. For a set this is a membership mirror (today's
  `set` relations). For an arrow target it is the via relation's subjects. For a wildcard it is every principal of
  the tenant, implicitly, with no rows.

The fence keeps its shape, one inlinable set function:

```sql
select container from rebac.readable_direct where tenant = t and principal = p
union
select n.container from rebac.node_readable n
where n.tenant = t and n.node = any(rebac.nodes_of(t, p))   -- p's nodes: a few dozen, computed once
```

A principal's nodes are a small array, read once per query in the same one-time gate as `rebac.checked`. The ANN
path then probes two primary keys per candidate row instead of one. The exact path takes the union as its array.
Neither leaves the plan shape the planner study measured.

### What it does to narrowing (G2)

It gets simpler:

- A membership removed deletes one `node_member` row. Every container the group was granted is gone for that person
  at once, and nobody's container rows are touched.
- A set grant, arrow tuple or wildcard removed deletes one `node_readable` row per container. No holders are
  enumerated, so the tenant-exclusive lock that today waits for every refresh in flight is no longer needed for these
  removals. Each such removal then touches O(containers), not O(holders).
- A direct revocation stays as it is today.

### Sizes for the scenarios above

The company-wide case becomes 100,000 `node_readable` rows (1% of 10M documents) plus no membership rows (a
wildcard), or 100,000 rows if expressed as an all-company group: about 50 MB in place of terabytes. Per-team sharing
becomes the teams' containers once each plus about 20 memberships per person. People's direct grants are unchanged.

### What must be decided before building it

1. **Deduplication of direct rows.** LookupResources for a principal returns everything it reads, without saying
   by which path. Either store it all as direct (simple, but it keeps the blow-up for anyone who reads a company-wide
   container), or subtract the containers covered by the principal's nodes. Subtracting is sound: a container held
   both directly and through a group, whose group grant is later removed, is narrowed too far and restored by the
   refresh that the removal queues, which is the direction a fence may err. Subtracting is the plan.
2. **Freshness (G7) per node.** A principal's set is as old as the oldest of its own lookup and its nodes' lookups.
   `rebac.checked` would read the minimum over its nodes, still one indexed read.
3. **The cap.** `max_containers_per_principal` would bound direct rows only. A node gets its own cap, and the
   readable set's size is never materialised.
4. **Which nodes to promote.** Small groups are cheaper expanded per person. Keep expanding a node into
   `readable_direct` until it has more than N members times M containers (a setting), then promote it.
5. **RLS and `readable_by`** keep working unchanged on top of the new `readable_set`.

### Not addressed

Nested groups (a set whose members are sets) and arrows more than one hop away are still refused by the closure.
This design makes them cheaper to add later, as nodes whose members are nodes, but does not add them.
