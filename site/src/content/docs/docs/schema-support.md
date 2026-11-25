---
title: SpiceDB schema support
description: Which SpiceDB schemas pg_rebac projects, how it derives the closure, and what it refuses and why.
---

pg_rebac projects one permission of one resource type, `container#view` by default. The daemon reads the live
SpiceDB schema and works out which relations can change that permission: the closure. It stores them in
`rebac.view_relation`, which tells `rebac.write` what to narrow and the relay what to refresh.

## Supported

The projected permission may be any union (`+`) of:

- **direct relations** (`member`, `editor`);
- **a self-arrow** (`parent->view`), a container under another container;
- **an arrow to another type and a relation there** (`workspace->admin`);
- **subject sets one level deep** (`group#member`, where `group#member` holds plain object types);
- **wildcards** (`user:*`) on a relation of the projected type. A wildcard grant is stored once, under the
  wildcard's grant node, which every principal with a subject of that type reads through; its removal narrows the
  node, and every holder of the container, at commit.

A schema of the shape the tests are built on:

```text
definition user {}

definition group {
  relation member: user
}

definition workspace {
  relation admin: user
}

definition container {
  relation workspace: workspace
  relation parent: container
  relation member: user | group#member
  relation editor: user
  permission view = member + editor + workspace->admin + parent->view
}
```

## The closure

`rebacd closure` prints what the live schema derives. Each relation gets a kind, and the kind decides what a write
does:

| Kind | Example | A touch | A delete |
|---|---|---|---|
| `direct` | `container#member` | refreshes the subject (a set's or a wildcard's grant node, for a grant to one) | narrows the subject at commit |
| `arrow` | `container#parent`, `container#workspace` | mirrored into `rebac.edge`; refreshes the parent's readers and the grant nodes that view it | narrows every holder of the child, grant nodes included |
| `via` | `workspace#admin` | refreshes the subject | narrows the containers whose arrow points at it, found through `rebac.edge` |
| `set` | `group#member` | refreshes the member | removes the membership, and the member's own copies of what the set holds |

With grant nodes on (`node_grants`, the default) a grant to a subject set or a wildcard is stored once, under its
node, and read by every member at query time ([the readable set](/docs/concepts/readable-set/)). Grants to a subject
set, through a direct or a via relation, are mirrored in `rebac.set_grant`, so a membership's removal narrows at
commit with SpiceDB down.

The stored closure is checked against the live schema each time the daemon starts. A difference refuses the start
until you accept it ([rebacd](/docs/rebacd/#the-schema-at-start)). `rebacd verify --closure` checks it on demand.

## Refused, and why

The rule underneath: narrowing on a delete is sound only if removing a tuple can never add access. Anything that breaks
that, or that cannot be evaluated from tuples alone, is refused when the daemon starts, with every reason listed. A
schema changed to such a construct while the daemon runs fails every read closed (`RB503`) until it is supported again
([schema changes while running](/docs/rebacd/#schema-changes-while-running)). A refusal is not a claim that the schema
is wrong, only that pg_rebac cannot project it soundly.

| Construct | Reason |
|---|---|
| Exclusion (`a - b`) | Not monotone: removing a tuple from `b` grants, and adding one revokes, which no delete-driven narrowing sees. |
| Intersection (`a & b`) | Narrowing at commit keeps a container that another relation the outbox shows granted still holds. That is exact for a union and would keep a container whose other branch was just revoked. |
| Intersection arrow (`.all()`) | Not monotone: removing one of its tuples leaves fewer targets that must all hold, which can grant. |
| Caveats (`with some_caveat`) | A caveated relationship cannot be evaluated locally. |
| Expiring relationships (`with expiration`) | Cannot be evaluated locally. |
| Wildcards (`user:*`) on a relation reached through an arrow, or inside a subject set | Its removal would have to narrow every holder of every container under the other object; the narrowing reads one hop. |
| Nested subject sets (`group#member` holding `group#member`) | A set's members must be plain object types. |
| A subject set naming a permission | It must name a relation of a defined type. |
| An arrow more than one hop from the projected type | Removing its far tuple would have to narrow every container under it; `rebac.edge` mirrors one hop. |

Other permissions on the same type, such as `edit`, are not part of the closure and are not checked.
