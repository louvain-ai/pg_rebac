# Intersection, deeper arrows, exclusion and `.all()`: what narrowing would need

[`schema-support`](../../site/src/content/docs/docs/schema-support.md) lists what the closure accepts: unions and
direct relations, a self-arrow, an arrow one hop to another type, subject sets one level deep, and
wildcards on the projected type. This note is the design for the shapes it refuses for a structural
reason, and what each would cost. None of it is built.

## The rule every shape must keep

A revocation through `rebac.write` is in force at commit (G2). The commit can only narrow what it can name from the
mirrors it holds (`rebac.edge`, `rebac.set_grant`, the outbox), so a shape is accepted only when:

1. removing a tuple can never grant (otherwise a delete would have to widen someone), and
2. every tuple whose removal can narrow is mirrored, so the commit can find who held what through it.

Unions meet both. The shapes below break one or the other, or a third rule that narrowing itself assumes.

## Intersection (`a & b`)

Monotone, and every tuple is mirrored, so rules 1 and 2 hold. What breaks is narrowing's "still held" rule: a
removal keeps a container that another closure relation the outbox shows granted to the same principal still holds
(`tests/sql/04_narrowing.sql`). For `a + b` that is exact. For `a & b`, removing `b` must take the container away even
though `a` still holds.

Design: the closure marks the relations under an intersection, and narrowing skips the still-held check for a removal
of such a relation (narrowing too far is the safe direction; the refresh that follows restores what SpiceDB still
grants). The change is in `narrow_seqs`, which the projection-scale work is rewriting, so it lands after it.

## Arrows more than one hop away (`workspace->org->member`)

Monotone, so rule 1 holds. Rule 2 fails: removing `workspace:w#org@org:o` narrows every holder of every container
under `w`, but `rebac.edge` mirrors only the projected type's own arrow tuples (container to workspace), not the
workspace-to-org hop.

Design:

- Mirror every arrow tuple on the path into `rebac.edge`, keyed by its resource type. The closure already walks the
  path, so it can emit a row per hop (`kind = 'arrow'` with `resource_type` set to the hop's type).
- A removed hop's narrowing is the closure of `edge` below it: a recursive CTE from the removed object down to the
  containers, then the holders of those containers. That is the same "narrow every holder" path an arrow tuple takes
  today, under the tenant lock held exclusively.
- Cost: one recursive walk per removed hop tuple, bounded by the containers under it. The tenant-exclusive lock is
  held for that walk, so a deep hierarchy's removal waits on every refresh in flight, as an arrow removal does now.
- The projection-scale design's node tables make this cheaper: a hop target becomes a node, and removing a hop deletes
  node rows rather than enumerating holders.

## Exclusion (`a - b`)

Rule 1 fails both ways: removing a tuple from `b` grants, and ADDING one to `b` revokes. Narrowing today runs only on
deletes. Accepting exclusion means:

- The closure tags each relation with a polarity. A relation under the subtracted side is negative.
- `rebac.write('touch', …)` on a negative relation narrows exactly as a delete of a positive one does, at commit, with
  the same lock order and the same pending rule (G3: a pending negative touch is a pending revocation).
- `rebac.write('delete', …)` on a negative relation is a grant: lazy, queued for a refresh.
- Watch applies the same polarity, and the sweep's re-send of recent revocations includes negative touches.
- Nesting flips polarity again (`a - (b - c)`), so polarity is computed per path, and a relation reached by paths of
  both polarities is refused.

The SQL change is concentrated in `narrow_seqs` and `pending_revoked`, which read `op = 'delete'` today and would read
"the op that narrows for this relation's polarity". That is a contained change, but it touches the code the
projection-scale work is rewriting, so it follows that work.

## `.all()` (intersection arrows)

`t.all(p)` grants when every target of `t` grants `p`. Removing a `t` tuple to a target that lacked `p` leaves only
targets that have it, which grants: rule 1 fails, and unlike exclusion the grant depends on the other targets' state,
not on the tuple alone. The commit would have to re-evaluate the permission, not narrow from tuples. Refused, with
no plan to accept it in the projection; Materialize refuses it on the materialised path for the same reason.

## Caveats and expiring relationships

A caveat's outcome depends on context supplied at check time, and an expiry on the clock, so neither is a property of
the tuple a projection can store. Both stay refused at the closure. On Watch, a TOUCH carrying either is applied as a
removal (it narrows, and the refresh that follows asks SpiceDB), which is the safe reading of a shape the closure has
not accepted yet.
