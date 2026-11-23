-- pg_fence: permission-aware SQL for Postgres, pure-SQL mode.
--
-- Install with `psql -v ON_ERROR_STOP=1 -f sql/fence.sql`. Needs nothing beyond plain SQL and PL/pgSQL, so it runs on
-- any Postgres 14 or later, hosted or not. Everything lives in schema `fence`.
--
-- The model, in one paragraph. SpiceDB is the authority for who may view what. `fence.readable` is a local,
-- per-tenant, per-principal projection of one permission (by default `container#view`), kept by the `fenced` daemon.
-- A query fences itself with `container in (select * from fence.readable_set(:principal))`, which Postgres inlines
-- into an indexed semi-join, so the fence sits inside the same plan as a vector or keyword ordering. Relationship
-- writes made through `fence.write` are queued in `fence.outbox` and relayed to SpiceDB in order; a revocation among
-- them is removed from the projection by a deferred trigger before the writing transaction commits.
--
-- Errors (SQLSTATE, condition):
--   FN400 fence_no_tenant       no `fence.tenant` setting: a read without a tenant is a bug, not an empty set
--   FN404 fence_not_projected   this principal was never asked; ask the daemon (POST /lookup) and retry
--   FN503 fence_unavailable     the projection cannot be trusted now (stale, over its cap, or the store unreachable)
--   FN422 fence_invalid         a write the fence refuses (a tenant mismatch, a malformed reference)
-- "Never asked", "nothing" and "unavailable" are three different answers, and none of them is an empty set by accident.

create schema if not exists fence;

-- -------------------------------------------------------------------------------------------------------------------
-- Configuration. One row per key; the daemon reads the same table, so SQL and daemon cannot disagree.
-- -------------------------------------------------------------------------------------------------------------------
create table if not exists fence.config (
  key   text primary key,
  value text not null
);

insert into fence.config (key, value) values
  ('resource_type', 'container'),     -- the SpiceDB resource type whose permission is projected
  ('permission', 'view'),             -- the projected permission
  ('subject_type', 'user'),           -- the SpiceDB subject type principals read as
  ('tenant_separator', '/'),          -- object ids are '<tenant><sep><id>'; '' when ids carry no tenant prefix
  ('implicit_subject', 'true'),       -- a principal also reads as '<subject_type>:<tenant><sep><principal>'
  ('max_staleness_ms', '5000'),       -- reads fail closed (FN503) when the daemon's heartbeat is older; 0 turns it off
  ('empty_ttl_ms', '60000'),          -- how long "can read nothing" is believed
  ('max_containers_per_principal', '100000'),
  ('narrow_lock_timeout_ms', '5000')  -- how long narrowing waits for a principal's lock before failing the commit
on conflict (key) do nothing;

create or replace function fence.setting(k text) returns text
language sql stable parallel safe as $$
  select value from fence.config where key = k
$$;

-- -------------------------------------------------------------------------------------------------------------------
-- Tables.
-- -------------------------------------------------------------------------------------------------------------------

-- The projection: one row per (tenant, principal, container) the principal may view.
create table if not exists fence.readable (
  tenant    text not null,
  principal text not null,
  container text not null,   -- the resource id with the tenant prefix stripped
  primary key (tenant, principal, container)
);
-- The reverse direction: narrowing, and "who can read this container".
create index if not exists readable_container_idx on fence.readable (tenant, container);

-- A second projection with the same shape, written only by a daemon in shadow mode and compared, never read.
create table if not exists fence.readable_shadow (like fence.readable including all);

-- "This principal was asked and can read nothing", with when. Only ever narrows, for at most `empty_ttl_ms`.
create table if not exists fence.readable_empty (
  tenant     text not null,
  principal  text not null,
  checked_at timestamptz not null default now(),
  primary key (tenant, principal)
);

-- Per-principal state. `retired` reads nothing whatever SpiceDB holds, and no refresh can grow it back.
create table if not exists fence.principal_state (
  tenant       text not null,
  principal    text not null,
  state        text not null check (state in ('projected', 'pending', 'unavailable', 'retired')),
  reason       text,
  zedtoken     text,
  projected_at timestamptz,
  n_containers integer not null default 0,
  version      bigint not null default 0,   -- moves on every change to the set: a cache key for the application
  primary key (tenant, principal)
);

-- Principal -> SpiceDB subjects. A principal reads as the union of its subjects (and its implicit one). The
-- application writes this table; only `trusted` rows count.
create table if not exists fence.subject (
  tenant    text not null,
  principal text not null,
  subject   text not null,   -- a full SpiceDB subject reference, e.g. 'user:acme/slack_U042'
  trusted   boolean not null,
  primary key (tenant, principal, subject)
);
create index if not exists subject_by_subject_idx on fence.subject (tenant, subject);

-- The relations that can change the projected permission, derived from the SpiceDB schema by the daemon and checked
-- against it at start. kind: 'direct' (a relation on the resource type that grants it), 'arrow' (a relation on the
-- resource type the permission walks through; its tuples are mirrored into fence.edge), 'via' (a relation on an
-- arrowed type that grants it there; `through` names the arrow).
create table if not exists fence.view_relation (
  resource_type text not null,
  relation      text not null,
  kind          text not null check (kind in ('direct', 'arrow', 'via')),
  target_type   text,          -- arrow: the type it points at
  through       text,          -- via: the arrow on the projected type that reaches this relation
  primary key (resource_type, relation)
);

-- Local copy of arrow tuples (child#relation@parent), so narrowing works with SpiceDB and the daemon down.
create table if not exists fence.edge (
  tenant   text not null,
  child    text not null,   -- stripped id of the projected type
  relation text not null,   -- the arrow relation
  parent   text not null,   -- typed and stripped: 'container:space-7', 'workspace:w1'
  primary key (tenant, child, relation, parent)
);
create index if not exists edge_parent_idx on fence.edge (tenant, parent, relation);

-- The outbox: relationship writes made through fence.write, in the order they must reach SpiceDB.
create table if not exists fence.outbox (
  seq           bigserial primary key,
  tenant        text not null,
  op            text not null check (op in ('touch', 'delete')),
  resource      text not null,
  relation      text not null,
  subject       text not null,
  committed_xid xid8 not null default pg_current_xact_id(),
  created_at    timestamptz not null default now(),
  synced_at     timestamptz,
  zedtoken      text,
  error         text,
  requeued_at   timestamptz
);
create index if not exists outbox_unsynced_idx on fence.outbox (tenant, seq) where synced_at is null;
create index if not exists outbox_relationship_idx on fence.outbox (resource, relation, subject, seq desc);
create index if not exists outbox_xid_idx on fence.outbox (committed_xid);

-- The durable refresh queue: principals whose projection a delivered write (or a sweep) may have changed.
create table if not exists fence.refresh (
  tenant      text not null,
  principal   text not null,
  priority    smallint not null default 1,   -- 0 = revocation-driven, 1 = grant, 2 = sweep
  zedtoken    text,                          -- the lookup must be at least as fresh as this
  due_at      timestamptz not null default now(),
  attempts    integer not null default 0,
  leased_until timestamptz,
  last_error  text,
  primary key (tenant, principal)
);
create index if not exists refresh_due_idx on fence.refresh (priority, due_at);

-- The daemon's Watch position and liveness. scope '*' is the deployment; a tenant may have its own row.
create table if not exists fence.watch_state (
  scope          text primary key,
  zedtoken       text,
  heartbeat_at   timestamptz,
  external_writes bigint not null default 0   -- relationship changes seen on Watch that did not come through fence.write
);

-- Functions the application registers to run inside a narrowing transaction: fn(tenant text, containers text[]).
create table if not exists fence.narrow_hook (
  fn regprocedure primary key
);

-- -------------------------------------------------------------------------------------------------------------------
-- Identity helpers.
-- -------------------------------------------------------------------------------------------------------------------

-- The tenant of this transaction. A missing setting is an error, never an empty set.
create or replace function fence.tenant() returns text
language plpgsql stable parallel safe as $$
declare t text := current_setting('fence.tenant', true);
begin
  if t is null or t = '' then
    raise exception 'fence: no tenant is set for this transaction'
      using errcode = 'FN400', hint = 'set local fence.tenant = ''<tenant>'' before a fenced read or write';
  end if;
  return t;
end $$;

-- '<tenant><sep>' or '' when ids carry no tenant prefix.
create or replace function fence.prefix(t text) returns text
language sql stable parallel safe as $$
  select case when coalesce(fence.setting('tenant_separator'), '') = '' then '' else t || fence.setting('tenant_separator') end
$$;

-- An object id with its tenant prefix removed, or NULL when it does not carry THIS tenant's prefix.
create or replace function fence.strip(t text, id text) returns text
language sql stable parallel safe as $$
  select case when fence.prefix(t) = '' then id
              when starts_with(id, fence.prefix(t)) then substr(id, length(fence.prefix(t)) + 1)
              else null end
$$;

-- 'type:id' split.
create or replace function fence.obj_type(ref text) returns text
language sql immutable parallel safe as $$ select split_part(ref, ':', 1) $$;
create or replace function fence.obj_id(ref text) returns text
language sql immutable parallel safe as $$ select substr(ref, strpos(ref, ':') + 1) $$;

-- The SpiceDB subjects a principal reads as. A retired principal reads as nothing.
create or replace function fence.subjects_of(t text, p text) returns setof text
language sql stable as $$
  select s from (
    select subject as s from fence.subject where tenant = t and principal = p and trusted
    union
    select fence.setting('subject_type') || ':' || fence.prefix(t) || p
    where fence.setting('implicit_subject') = 'true'
  ) x
  where not exists (select 1 from fence.principal_state ps
                    where ps.tenant = t and ps.principal = p and ps.state = 'retired')
$$;

-- The principals a subject reference stands for in a tenant: its explicit links, and its implicit principal.
-- A subject whose prefix is not this tenant's stands for nobody here.
create or replace function fence.principals_of(t text, subj text) returns setof text
language sql stable as $$
  select principal from fence.subject where tenant = t and subject = subj and trusted
  union
  select fence.strip(t, fence.obj_id(subj))
  where fence.setting('implicit_subject') = 'true'
    and fence.obj_type(subj) = fence.setting('subject_type')
    and fence.strip(t, fence.obj_id(subj)) is not null
    and strpos(subj, '#') = 0
$$;

-- One writer per (tenant, principal): a refresh, a lazy first read and a narrowing all take it before asking SpiceDB.
create or replace function fence.lock_principal(t text, p text) returns void
language sql volatile as $$
  select pg_advisory_xact_lock(hashtextextended('fence:' || t || ':' || p, 0))
$$;

-- -------------------------------------------------------------------------------------------------------------------
-- The read side.
-- -------------------------------------------------------------------------------------------------------------------

-- May THIS principal be read for, now? True to read its rows; false for a principal that reads nothing (retired, or
-- a fresh "nothing" mark); otherwise it raises. The sets below use it as a pseudo-constant qual, which the planner
-- places in a one-time gating filter, evaluated once and before the scan of fence.readable.
create or replace function fence.checked(p text) returns boolean
language plpgsql stable parallel safe as $$
declare
  t text := fence.tenant();
  bound bigint := coalesce(fence.setting('max_staleness_ms'), '0')::bigint;
  beat timestamptz;
  st fence.principal_state;
begin
  if p is null or p = '' then
    raise exception 'fence: no principal' using errcode = 'FN400';
  end if;
  if bound > 0 then
    select max(heartbeat_at) into beat from fence.watch_state where scope in ('*', t);
    if beat is null or statement_timestamp() - beat > bound * interval '1 millisecond' then
      raise exception 'fence: the projection is stale (no daemon heartbeat within % ms)', bound
        using errcode = 'FN503', hint = 'fence_stale: check that fenced is running and can reach SpiceDB';
    end if;
  end if;
  select * into st from fence.principal_state where tenant = t and principal = p;
  if found then
    if st.state = 'retired' then return false; end if;
    if st.state = 'unavailable' then
      raise exception 'fence: the projection of this principal is unavailable (%)', coalesce(st.reason, 'unknown')
        using errcode = 'FN503';
    end if;
    if st.state = 'projected' then
      if st.n_containers > 0 then return true; end if;
      if exists (select 1 from fence.readable_empty e where e.tenant = t and e.principal = p
                   and e.checked_at > statement_timestamp()
                       - coalesce(fence.setting('empty_ttl_ms'), '0')::bigint * interval '1 millisecond') then
        return false;
      end if;
    end if;
  end if;
  raise exception 'fence: this principal has not been projected'
    using errcode = 'FN404', hint = 'ask the daemon (POST /lookup) to project it, then retry';
end $$;

-- THE SET. Inlined by the planner when used in FROM, so
--     where p.container in (select * from fence.readable_set(:principal))
-- is a semi-join on fence.readable's primary key. The principal is compared as the caller's own value, so the
-- planner estimates its breadth from ordinary statistics; the checks run once, in a gating filter. A scalar function
-- with a sub-select is never inlined, which is why the planner-shaped form is a set and not a boolean.
-- GUARANTEE: no row is ever returned without fence.checked having passed. An answer that is empty for another reason
-- (nothing to fence) may return empty without evaluating it.
create or replace function fence.readable_set(p text) returns setof text
language sql stable parallel safe rows 1000 as $$
  select r.container from fence.readable r
  where (select fence.checked(p)) and r.tenant = (select fence.tenant()) and r.principal = p
$$;

-- The set, narrowed by a list the caller resolved: an intersection, so the list can only narrow, never widen.
create or replace function fence.readable_set(p text, within text[]) returns setof text
language sql stable parallel safe rows 1000 as $$
  select r.container from fence.readable r
  where (select fence.checked(p)) and r.tenant = (select fence.tenant()) and r.principal = p
    and r.container = any(within)
$$;

-- One check, one container: the principal is checked first, always. Evaluated row by row, so use readable_set to
-- filter a scan.
create or replace function fence.readable_by(p text, c text) returns boolean
language plpgsql stable parallel safe as $$
begin
  if not fence.checked(p) then return false; end if;
  return exists (select 1 from fence.readable r where r.tenant = fence.tenant() and r.principal = p and r.container = c);
end $$;

-- How many containers a principal reads (checked like a read), for choosing the exact path or the ANN path: below
-- a few thousand candidate rows the exact path is both faster and complete (docs/planner.md).
create or replace function fence.breadth(p text) returns integer
language sql stable as $$
  select case when fence.checked(p) then
    (select n_containers from fence.principal_state where tenant = fence.tenant() and principal = p) else 0 end
$$;

-- The set's version, for caches keyed on it.
create or replace function fence.version(p text) returns bigint
language sql stable as $$
  select coalesce((select version from fence.principal_state where tenant = fence.tenant() and principal = p), 0)
$$;

-- -------------------------------------------------------------------------------------------------------------------
-- Projection writers (the daemon's entry points). Each takes the tenant explicitly; none clears another tenant.
-- -------------------------------------------------------------------------------------------------------------------

-- The containers a pending (undelivered) revocation names for this principal, which no projection may include:
-- SpiceDB still holds the grant until the relay delivers the delete. Read BEFORE SpiceDB is asked; a delete delivered
-- after this read is also absent from the lookup that follows.
create or replace function fence.pending_revoked(t text, p text) returns setof text
language sql stable as $$
  with recursive
  subj as (select fence.subjects_of(t, p) as s),
  net as (  -- latest undelivered op per relationship in this tenant
    select distinct on (resource, relation, subject) op, resource, relation, subject
    from fence.outbox where tenant = t and synced_at is null
    order by resource, relation, subject, seq desc
  ),
  deleted as (select * from net where op = 'delete'),
  seed as (
    -- a direct view relation of this principal's subjects
    select fence.strip(t, fence.obj_id(d.resource)) as c
    from deleted d join fence.view_relation v
      on v.kind = 'direct' and v.relation = d.relation and v.resource_type = fence.obj_type(d.resource)
    where d.subject in (select s from subj) and v.resource_type = fence.setting('resource_type')
    union
    -- a via relation (e.g. workspace#admin): every container whose edge reaches it
    select e.child
    from deleted d join fence.view_relation v
      on v.kind = 'via' and v.relation = d.relation and v.resource_type = fence.obj_type(d.resource)
    join fence.edge e on e.tenant = t and e.relation = v.through
      and e.parent = fence.obj_type(d.resource) || ':' || fence.strip(t, fence.obj_id(d.resource))
    where d.subject in (select s from subj)
    union
    -- an arrow tuple being removed holds its child for everyone until it is delivered
    select fence.strip(t, fence.obj_id(d.resource))
    from deleted d join fence.view_relation v
      on v.kind = 'arrow' and v.relation = d.relation and v.resource_type = fence.obj_type(d.resource)
  ),
  closure as (  -- and everything under those through self-arrows (a space's shared files)
    select c from seed where c is not null
    union
    select e.child from closure k
    join fence.edge e on e.tenant = t and e.parent = fence.setting('resource_type') || ':' || k.c
    join fence.view_relation v on v.kind = 'arrow' and v.relation = e.relation
      and v.target_type = fence.setting('resource_type')
  )
  select c from closure
$$;

-- Replace a principal's projection with `containers`, minus pending revocations, at `token`. The caller holds the
-- principal's lock (fence.lock_principal) from BEFORE its SpiceDB lookup. `target` is 'live' or 'shadow'.
create or replace function fence.project(t text, p text, containers text[], token text, target text default 'live')
returns integer
language plpgsql volatile as $$
declare
  cap integer := coalesce(fence.setting('max_containers_per_principal'), '100000')::integer;
  final text[];
  n integer;
begin
  if target not in ('live', 'shadow') then
    raise exception 'fence.project: target must be live or shadow' using errcode = 'FN422';
  end if;
  if target = 'shadow' then
    delete from fence.readable_shadow where tenant = t and principal = p;
    if exists (select 1 from fence.principal_state where tenant = t and principal = p and state = 'retired') then
      return 0;
    end if;
    insert into fence.readable_shadow (tenant, principal, container)
    select t, p, c from unnest(containers) c
    where c is not null and c not in (select fence.pending_revoked(t, p))
    on conflict do nothing;
    get diagnostics n = row_count;
    return n;
  end if;

  if exists (select 1 from fence.principal_state where tenant = t and principal = p and state = 'retired') then
    delete from fence.readable where tenant = t and principal = p;
    return 0;
  end if;
  select coalesce(array_agg(distinct c), '{}') into final
  from unnest(containers) c
  where c is not null and c not in (select fence.pending_revoked(t, p));
  n := cardinality(final);
  delete from fence.readable where tenant = t and principal = p;
  if n > cap then
    -- Never truncated: a truncated set is a wrong fence. Unavailable until someone raises the cap.
    insert into fence.principal_state (tenant, principal, state, reason, zedtoken, projected_at, n_containers, version)
    values (t, p, 'unavailable', 'over_cap', token, now(), 0, 1)
    on conflict (tenant, principal) do update
      set state = 'unavailable', reason = 'over_cap', zedtoken = excluded.zedtoken, projected_at = now(),
          n_containers = 0, version = fence.principal_state.version + 1;
    return 0;
  end if;
  if n > 0 then
    insert into fence.readable (tenant, principal, container) select t, p, c from unnest(final) c;
    delete from fence.readable_empty where tenant = t and principal = p;
  else
    insert into fence.readable_empty (tenant, principal, checked_at) values (t, p, now())
    on conflict (tenant, principal) do update set checked_at = now();
  end if;
  insert into fence.principal_state (tenant, principal, state, reason, zedtoken, projected_at, n_containers, version)
  values (t, p, 'projected', null, token, now(), n, 1)
  on conflict (tenant, principal) do update
    set state = 'projected', reason = null, zedtoken = excluded.zedtoken, projected_at = now(),
        n_containers = excluded.n_containers, version = fence.principal_state.version + 1;
  return n;
end $$;

-- Mark a principal as unreadable for now (the store could not be asked on a first read). Never an empty set.
create or replace function fence.mark_unavailable(t text, p text, why text) returns void
language sql volatile as $$
  insert into fence.principal_state (tenant, principal, state, reason)
  values (t, p, 'unavailable', why)
  on conflict (tenant, principal) do update set state = 'unavailable', reason = excluded.reason
  where fence.principal_state.state <> 'retired'
$$;

-- Retire a principal: it reads nothing from now on, whatever SpiceDB holds, until fence.unretire.
create or replace function fence.retire(t text, p text) returns void
language plpgsql volatile as $$
begin
  perform fence.lock_principal(t, p);
  delete from fence.readable where tenant = t and principal = p;
  delete from fence.readable_empty where tenant = t and principal = p;
  insert into fence.principal_state (tenant, principal, state, reason, version)
  values (t, p, 'retired', null, 1)
  on conflict (tenant, principal) do update
    set state = 'retired', reason = null, n_containers = 0, version = fence.principal_state.version + 1;
end $$;

create or replace function fence.unretire(t text, p text) returns void
language sql volatile as $$
  update fence.principal_state set state = 'pending', version = version + 1
  where tenant = t and principal = p and state = 'retired';
  insert into fence.refresh (tenant, principal, priority) values (t, p, 0)
  on conflict (tenant, principal) do update set priority = least(fence.refresh.priority, 0), due_at = now()
$$;

-- -------------------------------------------------------------------------------------------------------------------
-- The write side: fence.write and revocation-first narrowing.
-- -------------------------------------------------------------------------------------------------------------------

-- Queue one relationship write in the caller's transaction. Arrow tuples are mirrored into fence.edge at once, so
-- narrowing never needs SpiceDB. A reference whose id does not carry this tenant's prefix is refused.
create or replace function fence.write(op text, resource text, relation text, subject text) returns bigint
language plpgsql volatile as $$
declare
  t text := fence.tenant();
  new_seq bigint;
  v fence.view_relation;
  child text;
  parent text;
begin
  if op not in ('touch', 'delete') then
    raise exception 'fence.write: op must be touch or delete' using errcode = 'FN422';
  end if;
  if strpos(resource, ':') = 0 or strpos(subject, ':') = 0 or relation = '' then
    raise exception 'fence.write: references are type:id' using errcode = 'FN422';
  end if;
  if fence.strip(t, fence.obj_id(resource)) is null
     or fence.strip(t, split_part(fence.obj_id(subject), '#', 1)) is null then
    raise exception 'fence.write: a reference does not carry this tenant''s prefix' using errcode = 'FN422';
  end if;
  insert into fence.outbox (tenant, op, resource, relation, subject)
  values (t, op, resource, relation, subject) returning outbox.seq into new_seq;

  select * into v from fence.view_relation vr
  where vr.resource_type = fence.obj_type(resource) and vr.relation = write.relation and vr.kind = 'arrow';
  if found and fence.obj_type(resource) = fence.setting('resource_type') then
    child := fence.strip(t, fence.obj_id(resource));
    parent := fence.obj_type(subject) || ':' || fence.strip(t, fence.obj_id(subject));
    if op = 'touch' then
      insert into fence.edge (tenant, child, relation, parent) values (t, child, write.relation, parent)
      on conflict do nothing;
    end if;
    -- A delete keeps the edge until commit-time narrowing has used it; narrow_pending removes it.
  end if;
  return new_seq;
end $$;

-- Narrow every revocation THIS transaction queued (net per relationship, latest seq wins), once, under the affected
-- principals' locks taken in sorted order. Returns the number of projection rows removed. Called by the deferred
-- trigger at commit; calling it earlier is allowed and the trigger then skips what is done.
create or replace function fence.narrow_pending() returns integer
language plpgsql volatile as $$
declare
  me xid8 := pg_current_xact_id();
  upto bigint := coalesce(nullif(current_setting('fence._narrowed_upto', true), ''), '0')::bigint;
  top bigint;
  removed integer := 0;
  rec_p record;
  rec_h record;
  root text := fence.setting('resource_type');
  prior text := current_setting('lock_timeout');
begin
  select max(seq) into top from fence.outbox where committed_xid = me;
  if top is null or top <= upto then return 0; end if;

  create temp table if not exists fence_narrow_pairs (tenant text, principal text, container text)
    on commit drop;
  truncate fence_narrow_pairs;

  with net as (
    select distinct on (resource, relation, subject) tenant, op, resource, relation, subject
    from fence.outbox where committed_xid = me and seq > upto
    order by resource, relation, subject, seq desc
  ),
  del as (select * from net where op = 'delete'),
  -- a direct view relation whose subject holds no OTHER view relation by the outbox's net state (a demotion
  -- from editor to member is not a revocation)
  direct as (
    select d.tenant, d.resource, d.subject
    from del d join fence.view_relation v
      on v.kind = 'direct' and v.resource_type = fence.obj_type(d.resource) and v.relation = d.relation
    where v.resource_type = root
      and not exists (
        select 1 from fence.view_relation o
        cross join lateral (
          select x.op from fence.outbox x
          where x.resource = d.resource and x.relation = o.relation and x.subject = d.subject
          order by x.seq desc limit 1) latest
        where o.kind = 'direct' and o.resource_type = v.resource_type and o.relation <> d.relation
          and latest.op = 'touch')
  ),
  via as (
    select d.tenant, d.subject, e.child as container
    from del d join fence.view_relation v
      on v.kind = 'via' and v.resource_type = fence.obj_type(d.resource) and v.relation = d.relation
    join fence.edge e on e.tenant = d.tenant and e.relation = v.through
      and e.parent = fence.obj_type(d.resource) || ':' || fence.strip(d.tenant, fence.obj_id(d.resource))
  ),
  arrow as (  -- an arrow removed: everyone who holds its child loses it
    select d.tenant, fence.strip(d.tenant, fence.obj_id(d.resource)) as container
    from del d join fence.view_relation v
      on v.kind = 'arrow' and v.resource_type = fence.obj_type(d.resource) and v.relation = d.relation
    where v.resource_type = root
  )
  insert into fence_narrow_pairs (tenant, principal, container)
  select x.tenant, p, fence.strip(x.tenant, fence.obj_id(x.resource))
  from direct x cross join lateral fence.principals_of(x.tenant, x.subject) p
  union
  select x.tenant, p, x.container
  from via x cross join lateral fence.principals_of(x.tenant, x.subject) p
  union
  select r.tenant, r.principal, r.container
  from arrow a join fence.readable r on r.tenant = a.tenant and r.container = a.container;

  -- Everything under a lost container through self-arrows goes with it (a space's shared files).
  with recursive closure(tenant, principal, container) as (
    select tenant, principal, container from fence_narrow_pairs
    union
    select k.tenant, k.principal, e.child
    from closure k
    join fence.edge e on e.tenant = k.tenant and e.parent = root || ':' || k.container
    join fence.view_relation v on v.kind = 'arrow' and v.relation = e.relation and v.target_type = root
  )
  insert into fence_narrow_pairs select * from closure
  except select * from fence_narrow_pairs;

  -- Removed arrows leave the edge table now that narrowing has used them.
  delete from fence.edge e using (
    select distinct on (resource, relation, subject) tenant, op, resource, relation, subject
    from fence.outbox where committed_xid = me and seq > upto
    order by resource, relation, subject, seq desc
  ) n
  where n.op = 'delete' and e.tenant = n.tenant and e.relation = n.relation
    and e.child = fence.strip(n.tenant, fence.obj_id(n.resource))
    and e.parent = fence.obj_type(n.subject) || ':' || fence.strip(n.tenant, fence.obj_id(n.subject));

  perform set_config('fence._narrowed_upto', top::text, true);
  if not exists (select 1 from fence_narrow_pairs) then return 0; end if;

  -- A lookup that began before this revocation may be in flight under the principal's lock: wait for it, so it
  -- cannot write the grant back afterwards. Sorted, and bounded, so this cannot deadlock against the daemon.
  perform set_config('lock_timeout', fence.setting('narrow_lock_timeout_ms') || 'ms', true);
  for rec_p in select distinct tenant, principal from fence_narrow_pairs order by tenant, principal loop
    perform fence.lock_principal(rec_p.tenant, rec_p.principal);
  end loop;
  perform set_config('lock_timeout', prior, true);

  delete from fence.readable r using fence_narrow_pairs n
  where r.tenant = n.tenant and r.principal = n.principal and r.container = n.container;
  get diagnostics removed = row_count;

  update fence.principal_state s set version = s.version + 1,
    n_containers = (select count(*) from fence.readable r where r.tenant = s.tenant and r.principal = s.principal)
  from (select distinct tenant, principal from fence_narrow_pairs) n
  where s.tenant = n.tenant and s.principal = n.principal;
  -- Narrowed to nothing is a known answer, "nothing", not "never asked".
  insert into fence.readable_empty (tenant, principal, checked_at)
  select s.tenant, s.principal, now() from fence.principal_state s
  join (select distinct tenant, principal from fence_narrow_pairs) n
    on s.tenant = n.tenant and s.principal = n.principal
  where s.n_containers = 0 and s.state = 'projected'
  on conflict (tenant, principal) do update set checked_at = now();

  for rec_h in select fn from fence.narrow_hook loop
    for rec_p in select tenant, array_agg(distinct container) as containers from fence_narrow_pairs group by tenant loop
      execute format('select %s($1, $2)', rec_h.fn::regproc) using rec_p.tenant, rec_p.containers;
    end loop;
  end loop;
  perform pg_notify('fence_narrowed', '');
  return removed;
end $$;

create or replace function fence.narrow_trigger() returns trigger
language plpgsql as $$
begin
  if new.seq > coalesce(nullif(current_setting('fence._narrowed_upto', true), ''), '0')::bigint then
    perform fence.narrow_pending();
  end if;
  return null;
end $$;

-- The trigger makes the promise structural: a caller cannot forget to narrow.
drop trigger if exists fence_outbox_narrow on fence.outbox;
create constraint trigger fence_outbox_narrow
  after insert on fence.outbox deferrable initially deferred
  for each row execute function fence.narrow_trigger();

-- Wake the relay when a transaction commits outbox rows.
create or replace function fence.notify_outbox() returns trigger
language plpgsql as $$
begin
  perform pg_notify('fence_outbox', '');
  return null;
end $$;
drop trigger if exists fence_outbox_notify on fence.outbox;
create trigger fence_outbox_notify after insert on fence.outbox
  for each statement execute function fence.notify_outbox();

-- -------------------------------------------------------------------------------------------------------------------
-- Status and RLS.
-- -------------------------------------------------------------------------------------------------------------------

create or replace function fence.status() returns table (
  heartbeat_age_ms bigint, stale boolean, pending_outbox bigint, quarantined bigint, refresh_queue bigint,
  external_writes bigint, unavailable_principals bigint)
language sql stable as $$
  select
    (extract(epoch from (statement_timestamp() - (select max(heartbeat_at) from fence.watch_state))) * 1000)::bigint,
    coalesce((select max(heartbeat_at) from fence.watch_state) is null
      or statement_timestamp() - (select max(heartbeat_at) from fence.watch_state)
         > fence.setting('max_staleness_ms')::bigint * interval '1 millisecond', true)
      and fence.setting('max_staleness_ms')::bigint > 0,
    (select count(*) from fence.outbox where synced_at is null),
    (select count(*) from fence.outbox where error is not null),
    (select count(*) from fence.refresh),
    (select coalesce(sum(external_writes), 0)::bigint from fence.watch_state),
    (select count(*) from fence.principal_state where state = 'unavailable')
$$;

-- Generate a row-level-security policy that fences reads of `tbl` on `container_column`, the principal read from a
-- transaction-local setting. Refuses a table the current role owns (an owner bypasses RLS) unless force => true,
-- which adds FORCE ROW LEVEL SECURITY.
create or replace function fence.enable_rls(tbl regclass, container_column text,
                                            principal_setting text default 'fence.principal',
                                            force boolean default false)
returns void
language plpgsql volatile as $$
declare owner_name name;
begin
  select pg_get_userbyid(relowner) into owner_name from pg_class where oid = tbl;
  if owner_name = current_user and not force then
    raise exception 'fence.enable_rls: % is owned by %, which bypasses row-level security', tbl, owner_name
      using errcode = 'FN422', hint = 'pass force => true to add FORCE ROW LEVEL SECURITY';
  end if;
  execute format('alter table %s enable row level security', tbl);
  if force then execute format('alter table %s force row level security', tbl); end if;
  execute format('drop policy if exists fence_read on %s', tbl);
  execute format(
    'create policy fence_read on %s for select using (%I in (select * from fence.readable_set(current_setting(%L))))',
    tbl, container_column, principal_setting);
end $$;
