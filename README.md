# pg_fence

Permission-aware SQL for Postgres. SpiceDB stays the authority for who may read what; pg_fence keeps a local,
indexed projection of one permission and gives every query the same fence:

```sql
set local fence.tenant = 'acme';
select id from passage
where container in (select * from fence.readable_set('priya'))
order by embedding <=> $1
limit 20;
```

Postgres inlines `fence.readable_set` into a semi-join on the projection's primary key, so the fence sits inside the
same plan as a pgvector ordering, a full-text match or anything else, and is applied before the order and the limit.

Status: slice 1, pre-release. Pure-SQL mode and the `fenced` daemon. See `docs/` for the guarantees.

## Pure-SQL mode

```sh
psql -v ON_ERROR_STOP=1 -f sql/fence.sql
```

It needs nothing beyond SQL and PL/pgSQL, so it installs on any Postgres 14 or later, including hosted ones that
allow no custom extensions.

## Licence

Apache-2.0. See `LICENSE` and `NOTICE`.
