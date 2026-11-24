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
- **wildcards** (`user:*`) on a relation of the projected type. A wildcard grant refreshes
  every principal the tenant knows; its removal narrows every holder of the container at commit, like an arrow's.

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
| `direct` | `container#member` | refreshes the subject | narrows the subject at commit |
| `arrow` | `container#parent`, `container#workspace` | mirrored into `rebac.edge`; refreshes the parent's readers | narrows every holder of the child |
| `via` | `workspace#admin` | refreshes the subject | narrows the containers whose arrow points at it, found through `rebac.edge` |
| `set` | `group#member` | refreshes the member | narrows the member from what the set was granted |

Grants to a subject set are mirrored in `rebac.set_grant`, so a membership's removal narrows at commit with SpiceDB
down.

The stored closure is checked against the live schema each time the daemon starts. A difference refuses the start
until you accept it ([rebacd](/docs/rebacd/#the-schema-at-start)). `rebacd verify --closure` checks it on demand.

## Refused, and why

The rule underneath: narrowing on a delete is sound only if removing a tuple can never add access. Anything that
breaks that, or that cannot be evaluated from tuples alone, is refused when the daemon starts, with every reason
listed. A schema changed to such a construct while the daemon runs fails every read closed (`RB503`) until it is
supported again ([schema changes while running](/docs/rebacd/#schema-changes-while-running)). A refusal is not a claim that the schema is wrong, only that pg_rebac cannot project it soundly yet.

| Construct | Reason |
|---|---|
| Exclusion (`a - b`) | Not monotone: removing a tuple can grant. |
| Intersection (`a & b`) | Not supported. |
| Intersection arrow (`.all()`) | Not supported. |
| Caveats (`with some_caveat`) | A caveated relationship cannot be evaluated locally. |
| Expiring relationships (`with expiration`) | Cannot be evaluated locally. |
| Wildcards (`user:*`) on a relation reached through an arrow, or inside a subject set | Not supported. |
| Nested subject sets (`group#member` holding `group#member`) | A set's members must be plain object types. |
| A subject set naming a permission | It must name a relation of a defined type. |
| An arrow more than one hop from the projected type | Not supported. |

Other permissions on the same type, such as `edit`, are not part of the closure and are not checked.
