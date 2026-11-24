---
title: Revocations and consistency
description: What is in force at commit, what is eventually consistent, the staleness bound, and the guarantees with their limits.
---

## Revocations are in force at commit

Write relationships with `rebac.write` in your own transaction:

```sql
begin;
set local rebac.tenant = 'acme';
select rebac.write('delete', 'container:acme/space-7', 'member', 'user:acme/priya');
-- ... the rest of your transaction ...
commit;   -- priya no longer reads space-7, whether or not SpiceDB or rebacd is up
```

`rebac.write(op, resource, relation, subject)` takes `touch` or `delete` and queues the write in `rebac.outbox`. A
deferred constraint trigger narrows the projection before the transaction commits, so no caller can forget to.

What the trigger does, in order:

1. It takes the net operation per relationship this transaction queued (revoke then grant nets to nothing).
2. It skips a delete whose subject still holds view through another relation, by the outbox's net state. A
   demotion from editor to member is not a revocation.
3. It resolves subjects to principals and follows mirrored arrow edges, so a container's children go with it.
4. It takes the locks in a fixed order, bounded by `narrow_lock_timeout_ms` (5,000 by default): the tenant
   exclusively when the removal names no principal (below), then each principal in sorted order. If a lock cannot
   be taken in time, the COMMIT fails. It never commits an unnarrowed revocation.
5. It deletes the rows and bumps each principal's `rebac.version`.

Functions registered in `rebac.narrow_hook` run inside the same transaction with the tenant and the containers that
were narrowed.

### Exactly what is covered

At commit, exactly, for:

- direct relations (`container#member`);
- relations reached through an arrow (`workspace->admin`);
- memberships of subject sets (`group#member@user:…`).

Two removals narrow everyone who holds a container: an arrow tuple (a file leaving its folder) and a grant to a
subject set. They name no principal, so the trigger holds the tenant exclusively and only then reads who the holders
are. Every refresh holds its tenant shared from before its SpiceDB call until it commits. A refresh already in
flight therefore commits first and is narrowed; one that starts later waits and then subtracts the revocation as
pending. Such a revocation waits for the tenant's refreshes in flight, within `narrow_lock_timeout_ms`.

### READ COMMITTED only

The commit-time delete must see a refresh that committed a moment earlier. Under REPEATABLE READ or SERIALIZABLE it
reads the transaction's first snapshot and cannot, so `rebac.write` refuses a view revocation in such a transaction
with `RB422`. Grants are accepted at any isolation level. READ COMMITTED is the Postgres default.

Arrow, via and set narrowing needs the local edge and set-grant tables to be complete: every such write goes
through `rebac.write`, or the tenant has been backfilled.

## Grants are eventually consistent

A grant shows up after the relay delivers it to SpiceDB and the principal is refreshed. Until then the reader is
briefly narrower, which is the one direction a fence may err.

## Consistency of a lookup

By default each refresh asks SpiceDB `at_least_as_fresh` the token SpiceDB returned for the delivery that queued
it. A first read, and anything without a token, is looked up fully consistent. Set `consistency =
"fully_consistent"` in `rebac.toml` to use full consistency for every lookup.

A refresh holds the principal's lock from before its SpiceDB call to its write, and subtracts revocations still
pending in the outbox. So a lookup that began before a revocation cannot write the grant back.

## The staleness bound

The daemon heartbeats only while it can reach both SpiceDB and Postgres. Once the newest heartbeat is older than
`max_staleness_ms` (5,000 by default), every read fails closed with `RB503`. Revocations through the outbox never
wait on it; the bound exists so grants and external writes cannot fall arbitrarily far behind unnoticed.

`max_staleness_ms = 0` turns it off, for a deployment that projects from a static source.

## Guarantees

| | Guarantee |
|---|---|
| G1 | Every row a query fenced by `rebac.readable_set` returns lies in a container the projection holds for that tenant and principal, at the query's snapshot. No row is returned unless the principal's check passed. |
| G2 | A view revocation written through `rebac.write` in a READ COMMITTED transaction is absent from the projection when that transaction commits, as scoped above, including against a refresh that races it. |
| G3 | No refresh can restore a container whose revocation is pending in the outbox, or anything for a retired principal. |
| G4 | A refresh queued by a delivery is looked up at least as fresh as the token that delivery was written at. |
| G5 | Every key carries the tenant. A reference with another tenant's prefix is refused by `rebac.write`, maps to nobody, and is dropped from a lookup. |
| G6 | An unknown principal, a missing tenant, a stale daemon, an unavailable lookup and an over-cap set each raise. None is an empty set, or a wider one. |

## Not guaranteed

- **Writes not made through `rebac.write`**, by another service or the `zed` CLI, are counted on Watch and narrowed
  by the sweep only. G2 does not cover them.
- **The still-held rule** reads the outbox's net state, so it is exact only when every view relation is written
  through `rebac.write`.
- **Caveats, expiring relationships and exclusion** are refused when the daemon starts, not evaluated.
- **A read replica** holds G2 only once its replay has passed the commit.
