# Roadmap after 0.3

In order. Each item keeps the rule every shape must keep ([`schema-depth.md`](schema-depth.md)): a revocation is in
force at commit, and nothing fails open while it is built.

1. **Multi-hop arrows (nested folders).** A container under a folder under a space. The closure walks a bounded
   number of hops; every arrow tuple on the path is mirrored into `rebac.edge` by its own resource type; removing a
   hop narrows every holder of every container below it through a recursive walk of `edge`, under the tenant lock
   held exclusively, as a one-hop arrow does today. Moving a folder re-projects the holders of everything under it.
   Design: [`schema-depth.md`](schema-depth.md#arrows-more-than-one-hop-away-workspace-org-member).
2. **Planner support for the bare `in` form (optional).** A separate C extension that gives the planner each
   reader's real breadth, so `container in (select * from rebac.readable_set(p))` chooses the exact or the HNSW plan
   on its own. The pure-SQL install does not change. Design: [`planner-support.md`](planner-support.md).
3. **Arrow targets as grant nodes, and promotion by size.** A space or workspace that many principals read through
   an arrow is stored once, as a grant subject set is today ([`projection-scale.md`](projection-scale.md)). A
   principal's own rows are promoted into a node when the same set recurs across enough principals.
4. **Incremental sweeps.** Watch is exact, so the sweep becomes the backstop it was meant to be: it re-projects the
   principals whose lookups are oldest, a slice per interval, instead of everyone each time. At 100,000 principals a
   full sweep takes about 80 minutes against the 15-minute default today (`projection-scale.md`).
5. **Intersection, then exclusion.** Intersection needs narrowing to skip its still-held rule for intersected
   relations; exclusion needs a polarity per relation, so that adding a tuple to the subtracted side narrows.
   Design: [`schema-depth.md`](schema-depth.md).
