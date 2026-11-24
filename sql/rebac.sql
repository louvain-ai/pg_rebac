-- pg_rebac: permission-aware SQL for Postgres, pure-SQL mode.
--
-- Install with `psql -v ON_ERROR_STOP=1 -f sql/rebac.sql`. Needs nothing beyond plain SQL and PL/pgSQL, so it runs on
-- any Postgres 14 or later, hosted or not. Everything lives in schema `rebac`.
--
-- The model, in one paragraph. SpiceDB is the authority for who may view what. `rebac.readable` is a local,
-- per-tenant, per-principal projection of one permission (by default `container#view`), kept by the `rebacd` daemon.
-- A query fences itself with `container in (select * from rebac.readable_set(:principal))`, which Postgres inlines
-- into an indexed semi-join, so the fence sits inside the same plan as a vector or keyword ordering. Relationship
-- writes made through `rebac.write` are queued in `rebac.outbox` and relayed to SpiceDB in order; a revocation among
-- them is removed from the projection by a deferred trigger before the writing transaction commits.
--
-- Errors (SQLSTATE, condition):
--   RB400 rebac_no_tenant       no `rebac.tenant` setting: a read without a tenant is a bug, not an empty set
--   RB404 rebac_not_projected   this principal was never asked; ask the daemon (POST /lookup) and retry
--   RB503 rebac_unavailable     the projection cannot be trusted now (stale, over its cap, or the store unreachable)
--   RB422 rebac_invalid         a write the fence refuses (a tenant mismatch, a malformed reference)
-- "Never asked", "nothing" and "unavailable" are three different answers, and none of them is an empty set by accident.

create schema if not exists rebac;

-- -------------------------------------------------------------------------------------------------------------------
-- Configuration. One row per key; the daemon reads the same table, so SQL and daemon cannot disagree.
-- -------------------------------------------------------------------------------------------------------------------
create table if not exists rebac.config (
  key   text primary key,
  value text not null
);

insert into rebac.config (key, value) values
  ('resource_type', 'container'),     -- the SpiceDB resource type whose permission is projected
  ('permission', 'view'),             -- the projected permission
  ('subject_type', 'user'),           -- the SpiceDB subject type principals read as
  ('tenant_separator', '/'),          -- object ids are '<tenant><sep><id>'; '' when ids carry no tenant prefix
  ('implicit_subject', 'true'),       -- a principal also reads as '<subject_type>:<tenant><sep><principal>'
  ('max_staleness_ms', '5000'),       -- LIVENESS: reads fail closed (RB503) when the daemon's heartbeat is older; 0 off
  ('max_projection_age_ms', '3600000'), -- FRESHNESS: a principal whose last SpiceDB lookup began longer ago, or never
                                      -- (unknown), fails closed (RB503); 0 off. Must exceed the daemon's sweep interval
  ('empty_ttl_ms', '60000'),          -- how long "can read nothing" is believed
  ('max_containers_per_principal', '100000'),
  ('narrow_lock_timeout_ms', '5000')  -- how long narrowing waits for a principal's lock before failing the commit
on conflict (key) do nothing;

create or replace function rebac.setting(k text) returns text
language sql stable parallel safe as $$
  select value from rebac.config where key = k
$$;

-- -------------------------------------------------------------------------------------------------------------------
-- Tables.
-- -------------------------------------------------------------------------------------------------------------------

-- The projection: one row per (tenant, principal, container) the principal may view.
create table if not exists rebac.readable (
  tenant    text not null,
  principal text not null,
  container text not null,   -- the resource id with the tenant prefix stripped
  primary key (tenant, principal, container)
);
-- The reverse direction: narrowing, and "who can read this container".
create index if not exists readable_container_idx on rebac.readable (tenant, container);

-- A second projection with the same shape, written only by a daemon in shadow mode and compared, never read.
create table if not exists rebac.readable_shadow (like rebac.readable including all);

-- "This principal was asked and can read nothing", with when. Only ever narrows, for at most `empty_ttl_ms`.
create table if not exists rebac.readable_empty (
  tenant     text not null,
  principal  text not null,
  checked_at timestamptz not null default now(),
  primary key (tenant, principal)
);

-- Per-principal state. `retired` reads nothing whatever SpiceDB holds, and no refresh can grow it back.
create table if not exists rebac.principal_state (
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
-- When the SpiceDB lookup behind this principal's set BEGAN: the time up to which every change SpiceDB held is
-- reflected in it (narrowing since only removes). NULL is unknown, and reads fail closed on it (max_projection_age_ms).
alter table rebac.principal_state add column if not exists looked_up_at timestamptz;

-- Principal -> SpiceDB subjects. A principal reads as the union of its subjects (and its implicit one). The
-- application writes this table; only `trusted` rows count.
create table if not exists rebac.subject (
  tenant    text not null,
  principal text not null,
  subject   text not null,   -- a full SpiceDB subject reference, e.g. 'user:acme/slack_U042'
  trusted   boolean not null,
  primary key (tenant, principal, subject)
);
create index if not exists subject_by_subject_idx on rebac.subject (tenant, subject);

-- The relations that can change the projected permission, derived from the SpiceDB schema by the daemon and checked
-- against it at start. kind: 'direct' (a relation on the resource type that grants it), 'arrow' (a relation on the
-- resource type the permission walks through; its tuples are mirrored into rebac.edge), 'via' (a relation on an
-- arrowed type that grants it there; `through` names the arrow), 'set' (a relation whose tuples are memberships of
-- a subject set, `group#member`, that a direct relation grants to).
create table if not exists rebac.view_relation (
  resource_type text not null,
  relation      text not null,
  kind          text not null check (kind in ('direct', 'arrow', 'via', 'set')),
  target_type   text,          -- arrow: the type it points at
  through       text,          -- via: the arrow on the projected type that reaches this relation
  primary key (resource_type, relation)
);

-- THE CLOSURE'S STATE, for the whole deployment (rebac.view_relation is deployment-wide). The daemon re-derives the
-- closure whenever SpiceDB's schema changes. 'invalid' while the live schema's closure is unsupported, or differs from
-- the stored one and the mirrors (edge, set_grant) and projections have not been rebuilt: every read fails closed
-- (RB503). After a rebuild it is 'valid' again, and `changed_at` stays: a principal whose lookup began before it
-- still fails closed until it is re-projected. No row is the state before any daemon has checked: valid.
create table if not exists rebac.closure_state (
  id         boolean primary key default true check (id),
  state      text not null check (state in ('valid', 'invalid')),
  reason     text,
  changed_at timestamptz not null default now()
);

-- Local copy of arrow tuples (child#relation@parent), so narrowing works with SpiceDB and the daemon down.
create table if not exists rebac.edge (
  tenant   text not null,
  child    text not null,   -- stripped id of the projected type
  relation text not null,   -- the arrow relation
  parent   text not null,   -- typed and stripped: 'container:space-7', 'workspace:w1'
  primary key (tenant, child, relation, parent)
);
create index if not exists edge_parent_idx on rebac.edge (tenant, parent, relation);
alter table rebac.view_relation drop constraint if exists view_relation_kind_check;
alter table rebac.view_relation add constraint view_relation_kind_check check (kind in ('direct', 'arrow', 'via', 'set'));

-- Local copy of grants to a SUBJECT SET (container#member@group:eng#member), so a membership's removal narrows with
-- SpiceDB and the daemon down: everyone it named loses what the set was granted.
create table if not exists rebac.set_grant (
  tenant      text not null,
  container   text not null,   -- stripped id of the projected type
  relation    text not null,
  subject_set text not null,   -- typed, stripped, with its relation: 'group:eng#member'
  primary key (tenant, container, relation, subject_set)
);
create index if not exists set_grant_set_idx on rebac.set_grant (tenant, subject_set);



-- The outbox: relationship writes made through rebac.write, in the order they must reach SpiceDB.
create table if not exists rebac.outbox (
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
create index if not exists outbox_unsynced_idx on rebac.outbox (tenant, seq) where synced_at is null;
create index if not exists outbox_relationship_idx on rebac.outbox (resource, relation, subject, seq desc);
create index if not exists outbox_xid_idx on rebac.outbox (committed_xid);

-- The durable refresh queue: principals whose projection a delivered write (or a sweep) may have changed.
create table if not exists rebac.refresh (
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
create index if not exists refresh_due_idx on rebac.refresh (priority, due_at);

-- The daemon's Watch position and liveness. scope '*' is the deployment; a tenant may have its own row.
create table if not exists rebac.watch_state (
  scope          text primary key,
  zedtoken       text,
  heartbeat_at   timestamptz,
  external_writes bigint not null default 0   -- relationship changes seen on Watch that did not come through rebac.write
);
-- When the Watch stream last delivered a message (a change or a checkpoint). Reported, never part of the read bound:
-- Watch is observe-only, so its position says nothing about the projection.
alter table rebac.watch_state add column if not exists watched_at timestamptz;

-- Functions the application registers to run inside a narrowing transaction: fn(tenant text, containers text[]).
create table if not exists rebac.narrow_hook (
  fn regprocedure primary key
);

-- -------------------------------------------------------------------------------------------------------------------
-- Identity helpers.
-- -------------------------------------------------------------------------------------------------------------------

-- The tenant of this transaction. A missing setting is an error, never an empty set.
create or replace function rebac.tenant() returns text
language plpgsql stable parallel safe as $$
declare t text := current_setting('rebac.tenant', true);
begin
  if t is null or t = '' then
    raise exception 'rebac: no tenant is set for this transaction'
      using errcode = 'RB400', hint = 'set local rebac.tenant = ''<tenant>'' before a fenced read or write';
  end if;
  return t;
end $$;

-- '<tenant><sep>' or '' when ids carry no tenant prefix.
create or replace function rebac.prefix(t text) returns text
language sql stable parallel safe as $$
  select case when coalesce(rebac.setting('tenant_separator'), '') = '' then '' else t || rebac.setting('tenant_separator') end
$$;

-- An object id with its tenant prefix removed, or NULL when it does not carry THIS tenant's prefix.
create or replace function rebac.strip(t text, id text) returns text
language sql stable parallel safe as $$
  select case when rebac.prefix(t) = '' then id
              when starts_with(id, rebac.prefix(t)) then substr(id, length(rebac.prefix(t)) + 1)
              else null end
$$;

-- 'type:id' split.
create or replace function rebac.obj_type(ref text) returns text
language sql immutable parallel safe as $$ select split_part(ref, ':', 1) $$;
create or replace function rebac.obj_id(ref text) returns text
language sql immutable parallel safe as $$ select substr(ref, strpos(ref, ':') + 1) $$;

-- 'group:acme/eng#member' as the set_grant table spells it in tenant t: 'group:eng#member'.
create or replace function rebac.set_ref(t text, ref text) returns text
language sql stable parallel safe as $$
  select rebac.obj_type(ref) || ':' || rebac.strip(t, split_part(rebac.obj_id(ref), '#', 1))
         || '#' || split_part(ref, '#', 2)
$$;

-- The SpiceDB subjects a principal reads as. A retired principal reads as nothing.
create or replace function rebac.subjects_of(t text, p text) returns setof text
language sql stable as $$
  select s from (
    select subject as s from rebac.subject where tenant = t and principal = p and trusted
    union
    select rebac.setting('subject_type') || ':' || rebac.prefix(t) || p
    where rebac.setting('implicit_subject') = 'true'
  ) x
  where not exists (select 1 from rebac.principal_state ps
                    where ps.tenant = t and ps.principal = p and ps.state = 'retired')
$$;

-- The principals a subject reference stands for in a tenant: its explicit links, and its implicit principal.
-- A subject whose prefix is not this tenant's stands for nobody here.
create or replace function rebac.principals_of(t text, subj text) returns setof text
language sql stable as $$
  select principal from rebac.subject where tenant = t and subject = subj and trusted
  union
  select rebac.strip(t, rebac.obj_id(subj))
  where rebac.setting('implicit_subject') = 'true'
    and rebac.obj_type(subj) = rebac.setting('subject_type')
    and rebac.strip(t, rebac.obj_id(subj)) is not null
    and strpos(subj, '#') = 0
$$;

-- THE LOCK ORDER, for every writer of the projection: the TENANT first, then the PRINCIPAL.
--   * A refresh, a lazy first read, a retirement and a narrowing that names its principals take the tenant SHARED, then
--     the principal's own lock exclusively (rebac.lock_principal), before asking SpiceDB, until they commit.
--   * A narrowing that removes an arrow tuple or a grant to a subject set names nobody: it narrows every holder of the
--     container. It takes the tenant EXCLUSIVELY (rebac.lock_tenant) before it reads who the holders are, so every
--     refresh of that tenant in flight has committed (and its row is visible to the read), and every refresh after it
--     waits for its commit (and then sees the revocation pending in the outbox).
-- Tenant locks use the two-key advisory space and principal locks the one-key space, so the two never collide.
-- Tenants are always locked in sorted order, then principals in sorted order: no two writers wait on each other.
create or replace function rebac.lock_tenant(t text, exclusive boolean default false) returns void
language plpgsql volatile as $$
begin
  if exclusive then
    perform pg_advisory_xact_lock(1919050595, hashtext(t));
  else
    perform pg_advisory_xact_lock_shared(1919050595, hashtext(t));
  end if;
end $$;

create or replace function rebac.lock_principal(t text, p text) returns void
language plpgsql volatile as $$
begin
  perform rebac.lock_tenant(t, false);
  perform pg_advisory_xact_lock(hashtextextended('rebac:' || t || ':' || p, 0));
end $$;

-- -------------------------------------------------------------------------------------------------------------------
-- The read side.
-- -------------------------------------------------------------------------------------------------------------------

-- May THIS principal be read for, now? True to read its rows; false for a principal that reads nothing (retired, or
-- a fresh "nothing" mark); otherwise it raises. The sets below use it as a pseudo-constant qual, which the planner
-- places in a one-time gating filter, evaluated once and before the scan of rebac.readable.
create or replace function rebac.checked(p text) returns boolean
language plpgsql stable parallel safe as $$
declare
  t text := rebac.tenant();
  bound bigint := coalesce(rebac.setting('max_staleness_ms'), '0')::bigint;
  age bigint := coalesce(rebac.setting('max_projection_age_ms'), '0')::bigint;
  beat timestamptz;
  cl rebac.closure_state;
  st rebac.principal_state;
begin
  if p is null or p = '' then
    raise exception 'rebac: no principal' using errcode = 'RB400';
  end if;
  -- LIVENESS: the daemon is up and can reach SpiceDB and Postgres. It says nothing about how current a set is.
  if bound > 0 then
    select max(heartbeat_at) into beat from rebac.watch_state where scope in ('*', t);
    if beat is null or statement_timestamp() - beat > bound * interval '1 millisecond' then
      raise exception 'rebac: the daemon is not live (no heartbeat within % ms)', bound
        using errcode = 'RB503', hint = 'rebac_stale: check that rebacd is running and can reach SpiceDB';
    end if;
  end if;
  -- THE CLOSURE: a schema change that alters what can grant view makes every set suspect until it is rebuilt.
  select * into cl from rebac.closure_state;
  if found and cl.state = 'invalid' then
    raise exception 'rebac: the SpiceDB schema changed what grants view (%); reads wait for the rebuild',
      coalesce(cl.reason, 'unknown')
      using errcode = 'RB503', hint = 'rebac_closure: rebacd re-derives the closure and rebuilds; see rebac.closure_state';
  end if;
  select * into st from rebac.principal_state where tenant = t and principal = p;
  if found then
    if st.state = 'retired' then return false; end if;
    if st.state = 'unavailable' then
      raise exception 'rebac: the projection of this principal is unavailable (%)', coalesce(st.reason, 'unknown')
        using errcode = 'RB503';
    end if;
    if st.state = 'projected' then
      -- FRESHNESS: the set reflects SpiceDB as of its lookup; one older than the bound, or of unknown age, is not read.
      if age > 0 and (st.looked_up_at is null
                      or statement_timestamp() - st.looked_up_at > age * interval '1 millisecond') then
        raise exception 'rebac: the projection of this principal is older than % ms, or of unknown age', age
          using errcode = 'RB503',
                hint = 'rebac_not_fresh: the daemon''s sweep re-projects everyone; POST /lookup re-projects one now';
      end if;
      if cl.changed_at > '-infinity' and (st.looked_up_at is null or st.looked_up_at < cl.changed_at) then
        raise exception 'rebac: this principal was projected before the schema''s closure last changed'
          using errcode = 'RB503', hint = 'rebac_closure: it is re-projected by the rebuild, the sweep or POST /lookup';
      end if;
      if st.n_containers > 0 then return true; end if;
      if exists (select 1 from rebac.readable_empty e where e.tenant = t and e.principal = p
                   and e.checked_at > statement_timestamp()
                       - coalesce(rebac.setting('empty_ttl_ms'), '0')::bigint * interval '1 millisecond') then
        return false;
      end if;
    end if;
  end if;
  raise exception 'rebac: this principal has not been projected'
    using errcode = 'RB404', hint = 'ask the daemon (POST /lookup) to project it, then retry';
end $$;

-- THE SET. Inlined by the planner when used in FROM, so
--     where p.container in (select * from rebac.readable_set(:principal))
-- is a semi-join on rebac.readable's primary key. The principal is compared as the caller's own value, so the
-- planner estimates its breadth from ordinary statistics; the checks run once, in a gating filter. A scalar function
-- with a sub-select is never inlined, which is why the planner-shaped form is a set and not a boolean.
-- GUARANTEE: no row is ever returned without rebac.checked having passed. An answer that is empty for another reason
-- (nothing to fence) may return empty without evaluating it.
create or replace function rebac.readable_set(p text) returns setof text
language sql stable parallel safe rows 1000 as $$
  select r.container from rebac.readable r
  where (select rebac.checked(p)) and r.tenant = (select rebac.tenant()) and r.principal = p
$$;

-- The set, narrowed by a list the caller resolved: an intersection, so the list can only narrow, never widen.
create or replace function rebac.readable_set(p text, within text[]) returns setof text
language sql stable parallel safe rows 1000 as $$
  select r.container from rebac.readable r
  where (select rebac.checked(p)) and r.tenant = (select rebac.tenant()) and r.principal = p
    and r.container = any(within)
$$;

-- One check, one container: the principal is checked first, always. Evaluated row by row, so use readable_set to
-- filter a scan.
create or replace function rebac.readable_by(p text, c text) returns boolean
language plpgsql stable parallel safe as $$
begin
  if not rebac.checked(p) then return false; end if;
  return exists (select 1 from rebac.readable r where r.tenant = rebac.tenant() and r.principal = p and r.container = c);
end $$;

-- How many containers a principal reads (checked like a read), for choosing the exact path or the ANN path: below
-- a few thousand candidate rows the exact path is both faster and complete (docs/planner.md).
create or replace function rebac.breadth(p text) returns integer
language sql stable as $$
  select case when rebac.checked(p) then
    (select n_containers from rebac.principal_state where tenant = rebac.tenant() and principal = p) else 0 end
$$;

-- The set's version, for caches keyed on it.
create or replace function rebac.version(p text) returns bigint
language sql stable as $$
  select coalesce((select version from rebac.principal_state where tenant = rebac.tenant() and principal = p), 0)
$$;

-- -------------------------------------------------------------------------------------------------------------------
-- Projection writers (the daemon's entry points). Each takes the tenant explicitly; none clears another tenant.
-- -------------------------------------------------------------------------------------------------------------------

-- The containers a pending (undelivered) revocation names for this principal, which no projection may include:
-- SpiceDB still holds the grant until the relay delivers the delete. Read BEFORE SpiceDB is asked; a delete delivered
-- after this read is also absent from the lookup that follows.
create or replace function rebac.pending_revoked(t text, p text) returns setof text
language sql stable as $$
  with recursive
  subj as (select rebac.subjects_of(t, p) as s),
  net as (  -- latest undelivered op per relationship in this tenant
    select distinct on (resource, relation, subject) op, resource, relation, subject
    from rebac.outbox where tenant = t and synced_at is null
    order by resource, relation, subject, seq desc
  ),
  deleted as (select * from net where op = 'delete'),
  seed as (
    -- a direct view relation of this principal's subjects
    select rebac.strip(t, rebac.obj_id(d.resource)) as c
    from deleted d join rebac.view_relation v
      on v.kind = 'direct' and v.relation = d.relation and v.resource_type = rebac.obj_type(d.resource)
    where d.subject in (select s from subj) and v.resource_type = rebac.setting('resource_type')
    union
    -- a via relation (e.g. workspace#admin): every container whose edge reaches it
    select e.child
    from deleted d join rebac.view_relation v
      on v.kind = 'via' and v.relation = d.relation and v.resource_type = rebac.obj_type(d.resource)
    join rebac.edge e on e.tenant = t and e.relation = v.through
      and e.parent = rebac.obj_type(d.resource) || ':' || rebac.strip(t, rebac.obj_id(d.resource))
    where d.subject in (select s from subj)
    union
    -- an arrow tuple, or a grant to a subject set, being removed holds its container for everyone until delivered
    select rebac.strip(t, rebac.obj_id(d.resource))
    from deleted d join rebac.view_relation v
      on v.relation = d.relation and v.resource_type = rebac.obj_type(d.resource)
     and (v.kind = 'arrow' or (v.kind = 'direct' and strpos(d.subject, '#') > 0))
    union
    -- a membership of a subject set: whatever the set was granted
    select g.container
    from deleted d join rebac.view_relation v
      on v.kind = 'set' and v.relation = d.relation and v.resource_type = rebac.obj_type(d.resource)
    join rebac.set_grant g on g.tenant = t
      and g.subject_set = rebac.obj_type(d.resource) || ':' || rebac.strip(t, rebac.obj_id(d.resource)) || '#' || d.relation
    where d.subject in (select s from subj)
  ),
  closure as (  -- and everything under those through self-arrows (a space's shared files)
    select c from seed where c is not null
    union
    select e.child from closure k
    join rebac.edge e on e.tenant = t and e.parent = rebac.setting('resource_type') || ':' || k.c
    join rebac.view_relation v on v.kind = 'arrow' and v.relation = e.relation
      and v.target_type = rebac.setting('resource_type')
  )
  select c from closure
$$;

-- Replace a principal's projection with `containers`, minus pending revocations, at `token`. The caller holds the
-- principal's lock (rebac.lock_principal) from BEFORE its SpiceDB lookup. `target` is 'live' or 'shadow'.
-- `looked_up_at` is when that lookup BEGAN (the daemon reads it from this database's clock); a caller that passes
-- none asserts the set is current now.
drop function if exists rebac.project(text, text, text[], text, text);
create or replace function rebac.project(t text, p text, containers text[], token text, target text default 'live',
                                         looked_up_at timestamptz default now())
returns integer
language plpgsql volatile as $$
declare
  cap integer := coalesce(rebac.setting('max_containers_per_principal'), '100000')::integer;
  final text[];
  n integer;
begin
  if target not in ('live', 'shadow') then
    raise exception 'rebac.project: target must be live or shadow' using errcode = 'RB422';
  end if;
  if target = 'shadow' then
    if exists (select 1 from rebac.principal_state where tenant = t and principal = p and state = 'retired') then
      delete from rebac.readable_shadow where tenant = t and principal = p;
      return 0;
    end if;
    select coalesce(array_agg(distinct c), '{}') into final
    from unnest(containers) c
    where c is not null and c not in (select rebac.pending_revoked(t, p));
    delete from rebac.readable_shadow r where r.tenant = t and r.principal = p
      and not exists (select 1 from unnest(final) f(c) where f.c = r.container);
    insert into rebac.readable_shadow (tenant, principal, container)
    select t, p, c from unnest(final) c
    on conflict do nothing;
    return cardinality(final);
  end if;

  if exists (select 1 from rebac.principal_state where tenant = t and principal = p and state = 'retired') then
    delete from rebac.readable where tenant = t and principal = p;
    return 0;
  end if;
  select coalesce(array_agg(distinct c), '{}') into final
  from unnest(containers) c
  where c is not null and c not in (select rebac.pending_revoked(t, p));
  n := cardinality(final);
  if n > cap then
    delete from rebac.readable where tenant = t and principal = p;
    -- Never truncated: a truncated set is a wrong fence. Unavailable until someone raises the cap.
    insert into rebac.principal_state (tenant, principal, state, reason, zedtoken, projected_at, n_containers, version)
    values (t, p, 'unavailable', 'over_cap', token, now(), 0, 1)
    on conflict (tenant, principal) do update
      set state = 'unavailable', reason = 'over_cap', zedtoken = excluded.zedtoken, projected_at = now(),
          n_containers = 0, version = rebac.principal_state.version + 1;
    return 0;
  end if;
  -- A DIFF, not a rewrite: only the containers that left are deleted and only the new ones inserted, so a refresh
  -- after a small change writes a small amount (no dead tuple, no index entry, no WAL for what stayed).
  delete from rebac.readable r where r.tenant = t and r.principal = p
    and not exists (select 1 from unnest(final) f(c) where f.c = r.container);
  if n > 0 then
    insert into rebac.readable (tenant, principal, container) select t, p, c from unnest(final) c
    on conflict do nothing;
    delete from rebac.readable_empty where tenant = t and principal = p;
  else
    insert into rebac.readable_empty (tenant, principal, checked_at) values (t, p, now())
    on conflict (tenant, principal) do update set checked_at = now();
  end if;
  insert into rebac.principal_state (tenant, principal, state, reason, zedtoken, projected_at, n_containers, version,
                                     looked_up_at)
  values (t, p, 'projected', null, token, now(), n, 1, project.looked_up_at)
  on conflict (tenant, principal) do update
    set state = 'projected', reason = null, zedtoken = excluded.zedtoken, projected_at = now(),
        n_containers = excluded.n_containers, version = rebac.principal_state.version + 1,
        looked_up_at = excluded.looked_up_at;
  return n;
end $$;

-- Mark a principal as unreadable for now (the store could not be asked on a first read). Never an empty set.
create or replace function rebac.mark_unavailable(t text, p text, why text) returns void
language sql volatile as $$
  insert into rebac.principal_state (tenant, principal, state, reason)
  values (t, p, 'unavailable', why)
  on conflict (tenant, principal) do update set state = 'unavailable', reason = excluded.reason
  where rebac.principal_state.state <> 'retired'
$$;

-- Retire a principal: it reads nothing from now on, whatever SpiceDB holds, until rebac.unretire.
create or replace function rebac.retire(t text, p text) returns void
language plpgsql volatile as $$
begin
  perform rebac.lock_principal(t, p);
  delete from rebac.readable where tenant = t and principal = p;
  delete from rebac.readable_empty where tenant = t and principal = p;
  insert into rebac.principal_state (tenant, principal, state, reason, version)
  values (t, p, 'retired', null, 1)
  on conflict (tenant, principal) do update
    set state = 'retired', reason = null, n_containers = 0, version = rebac.principal_state.version + 1;
end $$;

create or replace function rebac.unretire(t text, p text) returns void
language sql volatile as $$
  update rebac.principal_state set state = 'pending', version = version + 1
  where tenant = t and principal = p and state = 'retired';
  insert into rebac.refresh (tenant, principal, priority) values (t, p, 0)
  on conflict (tenant, principal) do update set priority = least(rebac.refresh.priority, 0), due_at = now()
$$;

-- -------------------------------------------------------------------------------------------------------------------
-- The write side: rebac.write and revocation-first narrowing.
-- -------------------------------------------------------------------------------------------------------------------

-- Queue one relationship write in the caller's transaction. Arrow tuples are mirrored into rebac.edge at once, so
-- narrowing never needs SpiceDB. A reference whose id does not carry this tenant's prefix is refused.
create or replace function rebac.write(op text, resource text, relation text, subject text) returns bigint
language plpgsql volatile as $$
declare
  t text := rebac.tenant();
  new_seq bigint;
  v rebac.view_relation;
  child text;
  parent text;
begin
  if op not in ('touch', 'delete') then
    raise exception 'rebac.write: op must be touch or delete' using errcode = 'RB422';
  end if;
  if strpos(resource, ':') = 0 or strpos(subject, ':') = 0 or relation = '' or strpos(resource, '#') > 0 then
    raise exception 'rebac.write: references are type:id (a subject may add #relation, a resource may not)'
      using errcode = 'RB422';
  end if;
  if rebac.strip(t, rebac.obj_id(resource)) is null
     or rebac.strip(t, split_part(rebac.obj_id(subject), '#', 1)) is null then
    raise exception 'rebac.write: a reference does not carry this tenant''s prefix' using errcode = 'RB422';
  end if;
  -- A revocation is narrowed at commit by deleting rows that a refresh may have committed a moment earlier. Under
  -- REPEATABLE READ or SERIALIZABLE that delete reads the transaction's first snapshot and cannot see such a row, so
  -- it would commit un-narrowed: refused, never weakened.
  if op = 'delete' and current_setting('transaction_isolation') <> 'read committed'
     and exists (select 1 from rebac.view_relation vr
                 where vr.resource_type = rebac.obj_type(resource) and vr.relation = write.relation) then
    raise exception 'rebac.write: a revocation must commit under READ COMMITTED (this transaction is %)',
      current_setting('transaction_isolation')
      using errcode = 'RB422', hint = 'write revocations in a READ COMMITTED transaction (the Postgres default)';
  end if;
  insert into rebac.outbox (tenant, op, resource, relation, subject)
  values (t, op, resource, relation, subject) returning outbox.seq into new_seq;

  select * into v from rebac.view_relation vr
  where vr.resource_type = rebac.obj_type(resource) and vr.relation = write.relation and vr.kind = 'arrow';
  if found and rebac.obj_type(resource) = rebac.setting('resource_type') then
    child := rebac.strip(t, rebac.obj_id(resource));
    parent := rebac.obj_type(subject) || ':' || rebac.strip(t, rebac.obj_id(subject));
    if op = 'touch' then
      insert into rebac.edge (tenant, child, relation, parent) values (t, child, write.relation, parent)
      on conflict do nothing;
    end if;
    -- A delete keeps the edge until commit-time narrowing has used it; narrow_pending removes it.
  end if;
  if op = 'touch' and strpos(subject, '#') > 0 and rebac.obj_type(resource) = rebac.setting('resource_type')
     and exists (select 1 from rebac.view_relation vr where vr.resource_type = rebac.obj_type(resource)
                   and vr.relation = write.relation and vr.kind = 'direct') then
    insert into rebac.set_grant (tenant, container, relation, subject_set)
    values (t, rebac.strip(t, rebac.obj_id(resource)), write.relation, rebac.set_ref(t, subject))
    on conflict do nothing;
  end if;
  return new_seq;
end $$;

-- Narrow every revocation among these outbox rows (net per relationship, latest seq wins), under the lock order above.
-- Returns the number of projection rows removed. The commit trigger calls it with this transaction's rows and
-- `hold_tenants` true; the relay calls it again with the rows it claims and `hold_tenants` false, which is a no-op when
-- the writing transaction already narrowed, and the safety net when it did not. (The relay's rows are committed, so a
-- refresh reads them as pending and needs no tenant lock to be kept from regrowing them; and the relay holds its
-- transaction across SpiceDB writes, which must not stall every refresh of a tenant.)
drop function if exists rebac.narrow_seqs(bigint[]);
create or replace function rebac.narrow_seqs(seqs bigint[], hold_tenants boolean default true) returns integer
language plpgsql volatile as $$
declare
  removed integer := 0;
  rec_p record;
  rec_h record;
  root text := rebac.setting('resource_type');
  prior text := current_setting('lock_timeout');
begin
  if seqs is null or cardinality(seqs) = 0 then return 0; end if;
  if current_setting('transaction_isolation') <> 'read committed' and exists (
       select 1 from rebac.outbox o join rebac.view_relation v
         on v.resource_type = rebac.obj_type(o.resource) and v.relation = o.relation
       where o.seq = any(seqs) and o.op = 'delete') then
    raise exception 'rebac: a revocation must be narrowed under READ COMMITTED (this transaction is %)',
      current_setting('transaction_isolation') using errcode = 'RB422';
  end if;
  create temp table if not exists rebac_narrow_pairs (tenant text, principal text, container text)
    on commit drop;
  truncate rebac_narrow_pairs;
  create temp table if not exists rebac_narrow_holders (tenant text, container text) on commit drop;
  truncate rebac_narrow_holders;

  -- An arrow, or a grant to a subject set, removed: everyone who holds the container loses it. Who holds it is read
  -- only AFTER the tenant is held exclusively (below).
  insert into rebac_narrow_holders (tenant, container)
  with net as (
    select distinct on (resource, relation, subject) tenant, op, resource, relation, subject
    from rebac.outbox where seq = any(seqs)
    order by resource, relation, subject, seq desc
  )
  select distinct d.tenant, rebac.strip(d.tenant, rebac.obj_id(d.resource))
  from net d join rebac.view_relation v
    on v.resource_type = rebac.obj_type(d.resource) and v.relation = d.relation
   and (v.kind = 'arrow' or (v.kind = 'direct' and strpos(d.subject, '#') > 0))
  where d.op = 'delete' and v.resource_type = root;

  perform set_config('lock_timeout', rebac.setting('narrow_lock_timeout_ms') || 'ms', true);
  if hold_tenants then
    for rec_p in select distinct tenant from rebac_narrow_holders order by tenant loop
      perform rebac.lock_tenant(rec_p.tenant, true);
    end loop;
  end if;
  perform set_config('lock_timeout', prior, true);

  with net as (
    select distinct on (resource, relation, subject) tenant, op, resource, relation, subject
    from rebac.outbox where seq = any(seqs)
    order by resource, relation, subject, seq desc
  ),
  del as (select * from net where op = 'delete'),
  -- a direct view relation whose subject holds no OTHER view relation by the outbox's net state (a demotion
  -- from editor to member is not a revocation)
  direct as (
    select d.tenant, d.resource, d.subject
    from del d join rebac.view_relation v
      on v.kind = 'direct' and v.resource_type = rebac.obj_type(d.resource) and v.relation = d.relation
    where v.resource_type = root
      and not exists (
        select 1 from rebac.view_relation o
        cross join lateral (
          select x.op from rebac.outbox x
          where x.resource = d.resource and x.relation = o.relation and x.subject = d.subject
          order by x.seq desc limit 1) latest
        where o.kind = 'direct' and o.resource_type = v.resource_type and o.relation <> d.relation
          and latest.op = 'touch')
  ),
  via as (
    select d.tenant, d.subject, e.child as container
    from del d join rebac.view_relation v
      on v.kind = 'via' and v.resource_type = rebac.obj_type(d.resource) and v.relation = d.relation
    join rebac.edge e on e.tenant = d.tenant and e.relation = v.through
      and e.parent = rebac.obj_type(d.resource) || ':' || rebac.strip(d.tenant, rebac.obj_id(d.resource))
  ),
  membership as (  -- a member leaves a subject set: they lose what the set was granted
    select d.tenant, d.subject, g.container
    from del d join rebac.view_relation v
      on v.kind = 'set' and v.resource_type = rebac.obj_type(d.resource) and v.relation = d.relation
    join rebac.set_grant g on g.tenant = d.tenant
      and g.subject_set = rebac.obj_type(d.resource) || ':' || rebac.strip(d.tenant, rebac.obj_id(d.resource)) || '#' || d.relation
  )
  insert into rebac_narrow_pairs (tenant, principal, container)
  select x.tenant, p, rebac.strip(x.tenant, rebac.obj_id(x.resource))
  from direct x cross join lateral rebac.principals_of(x.tenant, x.subject) p
  union
  select x.tenant, p, x.container
  from via x cross join lateral rebac.principals_of(x.tenant, x.subject) p
  union
  select x.tenant, p, x.container
  from membership x cross join lateral rebac.principals_of(x.tenant, x.subject) p
  union
  -- a new statement, so a new snapshot: it sees every refresh that committed while the tenant lock was awaited
  select r.tenant, r.principal, r.container
  from rebac_narrow_holders a join rebac.readable r on r.tenant = a.tenant and r.container = a.container;

  -- Everything under a lost container through self-arrows goes with it (a space's shared files).
  with recursive closure(tenant, principal, container) as (
    select tenant, principal, container from rebac_narrow_pairs
    union
    select k.tenant, k.principal, e.child
    from closure k
    join rebac.edge e on e.tenant = k.tenant and e.parent = root || ':' || k.container
    join rebac.view_relation v on v.kind = 'arrow' and v.relation = e.relation and v.target_type = root
  )
  insert into rebac_narrow_pairs select * from closure
  except select * from rebac_narrow_pairs;

  -- Removed arrows leave the edge table now that narrowing has used them.
  delete from rebac.edge e using (
    select distinct on (resource, relation, subject) tenant, op, resource, relation, subject
    from rebac.outbox where seq = any(seqs)
    order by resource, relation, subject, seq desc
  ) n
  where n.op = 'delete' and e.tenant = n.tenant and e.relation = n.relation
    and e.child = rebac.strip(n.tenant, rebac.obj_id(n.resource))
    and e.parent = rebac.obj_type(n.subject) || ':' || rebac.strip(n.tenant, rebac.obj_id(n.subject));

  -- Removed grants to a subject set leave the set_grant table likewise.
  delete from rebac.set_grant g using (
    select distinct on (resource, relation, subject) tenant, op, resource, relation, subject
    from rebac.outbox where seq = any(seqs)
    order by resource, relation, subject, seq desc
  ) n
  where n.op = 'delete' and strpos(n.subject, '#') > 0 and g.tenant = n.tenant and g.relation = n.relation
    and g.container = rebac.strip(n.tenant, rebac.obj_id(n.resource)) and g.subject_set = rebac.set_ref(n.tenant, n.subject);

  if not exists (select 1 from rebac_narrow_pairs) then return 0; end if;

  -- A lookup that began before this revocation may be in flight under the principal's lock: wait for it, so it
  -- cannot write the grant back afterwards. Sorted, and bounded, so this cannot deadlock against the daemon.
  perform set_config('lock_timeout', rebac.setting('narrow_lock_timeout_ms') || 'ms', true);
  for rec_p in select distinct tenant, principal from rebac_narrow_pairs order by tenant, principal loop
    if hold_tenants then
      perform rebac.lock_principal(rec_p.tenant, rec_p.principal);
    else
      -- The relay's re-narrowing only removes rows, so it takes no tenant lock at all: it must never hold up an
      -- arrow or set-grant revocation while its transaction waits on SpiceDB.
      perform pg_advisory_xact_lock(hashtextextended('rebac:' || rec_p.tenant || ':' || rec_p.principal, 0));
    end if;
  end loop;
  perform set_config('lock_timeout', prior, true);

  delete from rebac.readable r using rebac_narrow_pairs n
  where r.tenant = n.tenant and r.principal = n.principal and r.container = n.container;
  get diagnostics removed = row_count;

  update rebac.principal_state s set version = s.version + 1,
    n_containers = (select count(*) from rebac.readable r where r.tenant = s.tenant and r.principal = s.principal)
  from (select distinct tenant, principal from rebac_narrow_pairs) n
  where s.tenant = n.tenant and s.principal = n.principal;
  -- Narrowed to nothing is a known answer, "nothing", not "never asked".
  insert into rebac.readable_empty (tenant, principal, checked_at)
  select s.tenant, s.principal, now() from rebac.principal_state s
  join (select distinct tenant, principal from rebac_narrow_pairs) n
    on s.tenant = n.tenant and s.principal = n.principal
  where s.n_containers = 0 and s.state = 'projected'
  on conflict (tenant, principal) do update set checked_at = now();

  for rec_h in select fn from rebac.narrow_hook loop
    for rec_p in select tenant, array_agg(distinct container) as containers from rebac_narrow_pairs group by tenant loop
      execute format('select %s($1, $2)', rec_h.fn::regproc) using rec_p.tenant, rec_p.containers;
    end loop;
  end loop;
  perform pg_notify('rebac_narrowed', '');
  return removed;
end $$;

-- Narrow every revocation THIS transaction queued, once. Called by the deferred trigger at commit; calling it earlier
-- is allowed, and the trigger then skips what is done.
create or replace function rebac.narrow_pending() returns integer
language plpgsql volatile as $$
declare
  upto bigint := coalesce(nullif(current_setting('rebac._narrowed_upto', true), ''), '0')::bigint;
  seqs bigint[];
begin
  select array_agg(seq order by seq) into seqs
  from rebac.outbox where committed_xid = pg_current_xact_id() and seq > upto;
  if seqs is null then return 0; end if;
  perform set_config('rebac._narrowed_upto', seqs[cardinality(seqs)]::text, true);
  return rebac.narrow_seqs(seqs);
end $$;

create or replace function rebac.narrow_trigger() returns trigger
language plpgsql as $$
begin
  if new.seq > coalesce(nullif(current_setting('rebac._narrowed_upto', true), ''), '0')::bigint then
    perform rebac.narrow_pending();
  end if;
  return null;
end $$;

-- The trigger makes the promise structural: a caller cannot forget to narrow.
drop trigger if exists rebac_outbox_narrow on rebac.outbox;
create constraint trigger rebac_outbox_narrow
  after insert on rebac.outbox deferrable initially deferred
  for each row execute function rebac.narrow_trigger();

-- Wake the relay when a transaction commits outbox rows.
create or replace function rebac.notify_outbox() returns trigger
language plpgsql as $$
begin
  perform pg_notify('rebac_outbox', '');
  return null;
end $$;
drop trigger if exists rebac_outbox_notify on rebac.outbox;
create trigger rebac_outbox_notify after insert on rebac.outbox
  for each statement execute function rebac.notify_outbox();

-- -------------------------------------------------------------------------------------------------------------------
-- Status and RLS.
-- -------------------------------------------------------------------------------------------------------------------

-- What an operator watches. heartbeat_age_ms and stale are LIVENESS; oldest_lookup_ms and unfresh_principals are
-- FRESHNESS (the read bound); relay_lag_ms and watch_age_ms are reported only (a slow relay only delays grants, and
-- Watch is observe-only).
drop function if exists rebac.status();
create or replace function rebac.status() returns table (
  heartbeat_age_ms bigint, stale boolean, pending_outbox bigint, quarantined bigint, refresh_queue bigint,
  external_writes bigint, unavailable_principals bigint, relay_lag_ms bigint, watch_age_ms bigint,
  oldest_lookup_ms bigint, unfresh_principals bigint)
language sql stable as $$
  select
    (extract(epoch from (statement_timestamp() - (select max(heartbeat_at) from rebac.watch_state))) * 1000)::bigint,
    coalesce((select max(heartbeat_at) from rebac.watch_state) is null
      or statement_timestamp() - (select max(heartbeat_at) from rebac.watch_state)
         > rebac.setting('max_staleness_ms')::bigint * interval '1 millisecond', true)
      and rebac.setting('max_staleness_ms')::bigint > 0,
    (select count(*) from rebac.outbox where synced_at is null),
    (select count(*) from rebac.outbox where error is not null),
    (select count(*) from rebac.refresh),
    (select coalesce(sum(external_writes), 0)::bigint from rebac.watch_state),
    (select count(*) from rebac.principal_state where state = 'unavailable'),
    (select (extract(epoch from (statement_timestamp() - min(created_at))) * 1000)::bigint
     from rebac.outbox where synced_at is null),
    (select (extract(epoch from (statement_timestamp() - max(watched_at))) * 1000)::bigint from rebac.watch_state),
    (select (extract(epoch from (statement_timestamp() - min(looked_up_at))) * 1000)::bigint
     from rebac.principal_state where state = 'projected'),
    (select count(*) from rebac.principal_state where state = 'projected'
       and coalesce(rebac.setting('max_projection_age_ms'), '0')::bigint > 0
       and (looked_up_at is null or statement_timestamp() - looked_up_at
            > rebac.setting('max_projection_age_ms')::bigint * interval '1 millisecond'))
$$;

-- FRESHNESS PER TENANT: the oldest lookup among its projected principals is the time through which the WHOLE tenant
-- reflects SpiceDB (a sweep's completion, in effect: it re-projects everyone). Counts only.
create or replace function rebac.freshness() returns table (
  tenant text, principals bigint, oldest_lookup_at timestamptz, unknown bigint, unfresh bigint, refresh_queue bigint,
  relay_oldest_pending timestamptz)
language sql stable as $$
  select s.tenant, count(*), min(s.looked_up_at), count(*) filter (where s.looked_up_at is null),
    count(*) filter (where coalesce(rebac.setting('max_projection_age_ms'), '0')::bigint > 0
      and (s.looked_up_at is null or statement_timestamp() - s.looked_up_at
           > rebac.setting('max_projection_age_ms')::bigint * interval '1 millisecond')),
    (select count(*) from rebac.refresh r where r.tenant = s.tenant),
    (select min(o.created_at) from rebac.outbox o where o.tenant = s.tenant and o.synced_at is null)
  from rebac.principal_state s where s.state = 'projected'
  group by s.tenant
$$;

-- THE PRINCIPAL, BOUND BY A TRUSTED ROLE. `rebac.principal` is an ordinary setting: any role that can run SQL can set
-- it to anyone. Where a role that is NOT trusted can query a fenced table (an analyst, a BI tool, SQL an agent wrote),
-- bind the principal instead with rebac.set_principal, which only roles granted EXECUTE can call, and fence with
-- `principal_source => 'session'`. The binding is held in a table no other role can write, keyed by this backend and
-- this transaction, so it ends with the transaction.
create unlogged table if not exists rebac.session_principal (
  pid       integer not null,
  xact      xid8 not null,
  tenant    text not null,
  principal text not null,
  primary key (pid, xact)
);
revoke all on rebac.session_principal from public;

create or replace function rebac.set_principal(t text, p text) returns void
language plpgsql volatile security definer set search_path = pg_catalog, pg_temp as $$
begin
  if t is null or t = '' or p is null or p = '' then
    raise exception 'rebac.set_principal: a tenant and a principal are required' using errcode = 'RB400';
  end if;
  delete from rebac.session_principal where pid = pg_backend_pid();
  insert into rebac.session_principal (pid, xact, tenant, principal)
  values (pg_backend_pid(), pg_current_xact_id(), t, p);
  perform set_config('rebac.tenant', t, true);
end $$;
revoke all on function rebac.set_principal(text, text) from public;

-- The bound principal's readable set, for a policy. Runs as its owner, so the querying role needs no access to
-- rebac.readable or rebac.readable_set (and must not have it: with it, it could read anyone's set directly).
-- Not inlined (a SECURITY DEFINER function never is): the policy evaluates it once per query, as a hashed subplan.
create or replace function rebac.session_readable_set() returns setof text
language plpgsql volatile security definer set search_path = pg_catalog, pg_temp as $$
declare b rebac.session_principal;
begin
  select * into b from rebac.session_principal
  where pid = pg_backend_pid() and xact = pg_current_xact_id_if_assigned();
  if not found then
    raise exception 'rebac: no principal is bound to this transaction' using errcode = 'RB400',
      hint = 'a trusted role calls rebac.set_principal(tenant, principal) first';
  end if;
  if coalesce(current_setting('rebac.tenant', true), '') <> b.tenant then
    raise exception 'rebac: rebac.tenant was changed after the principal was bound' using errcode = 'RB422';
  end if;
  return query select * from rebac.readable_set(b.principal);
end $$;
revoke all on function rebac.session_readable_set() from public;

-- Generate a row-level-security policy that fences reads of `tbl` on `container_column`.
--   * RESTRICTIVE: it is ANDed with every other policy on the table, so no permissive policy (an application's own,
--     added before or after) can widen it. Postgres returns nothing through restrictive policies alone, so when the
--     table has no permissive SELECT policy, a permissive `rebac_base` (true) is added beside it; with permissive
--     policies of your own, rows must pass one of them AND the fence.
--   * principal_source 'setting' (the default) reads the principal from `principal_setting` (rebac.principal): for
--     a role you trust to set it truthfully. 'session' reads the one bound by rebac.set_principal.
-- Refuses a table the current role owns (an owner bypasses RLS) unless force => true, which adds FORCE ROW LEVEL
-- SECURITY. SELECT only: writes to `tbl` are governed by your own policies and grants.
drop function if exists rebac.enable_rls(regclass, text, text, boolean);
create or replace function rebac.enable_rls(tbl regclass, container_column text,
                                            principal_setting text default 'rebac.principal',
                                            force boolean default false,
                                            principal_source text default 'setting')
returns void
language plpgsql volatile as $$
declare owner_name name;
begin
  if principal_source not in ('setting', 'session') then
    raise exception 'rebac.enable_rls: principal_source is setting or session' using errcode = 'RB422';
  end if;
  select pg_get_userbyid(relowner) into owner_name from pg_class where oid = tbl::regclass;
  if owner_name = current_user and not force then
    raise exception 'rebac.enable_rls: % is owned by %, which bypasses row-level security', tbl, owner_name
      using errcode = 'RB422', hint = 'pass force => true to add FORCE ROW LEVEL SECURITY';
  end if;
  execute format('alter table %s enable row level security', tbl);
  if force then execute format('alter table %s force row level security', tbl); end if;
  execute format('drop policy if exists rebac_read on %s', tbl);
  if principal_source = 'setting' then
    execute format(
      'create policy rebac_read on %s as restrictive for select using (%I in (select * from rebac.readable_set(current_setting(%L))))',
      tbl, container_column, principal_setting);
  else
    execute format(
      'create policy rebac_read on %s as restrictive for select using (%I in (select * from rebac.session_readable_set()))',
      tbl, container_column);
  end if;
  if not exists (select 1 from pg_policy where polrelid = tbl and polpermissive and polcmd in ('r', '*')
                   and polname <> 'rebac_base') then
    execute format('drop policy if exists rebac_base on %s', tbl);
    execute format('create policy rebac_base on %s as permissive for select using (true)', tbl);
  end if;
end $$;

-- -------------------------------------------------------------------------------------------------------------------
-- The daemon's queues. Every daemon step is a SQL function, so its semantics are tested here and the daemon only
-- moves data between these functions and SpiceDB.
-- -------------------------------------------------------------------------------------------------------------------

alter table rebac.refresh add column if not exists generation bigint not null default 0;

-- Resources whose readers a delivered arrow tuple may have changed (a file hung under a space, a container moved to
-- a workspace). The daemon asks SpiceDB who can now view each one (LookupSubjects) and refreshes them.
create table if not exists rebac.refresh_resource (
  tenant      text not null,
  resource    text not null,   -- a full SpiceDB reference, 'container:acme/file-1'
  zedtoken    text,
  due_at      timestamptz not null default now(),
  attempts    integer not null default 0,
  leased_until timestamptz,
  generation  bigint not null default 0,
  primary key (tenant, resource)
);

-- One sweep per interval for the whole deployment, whichever replica claims it.
create table if not exists rebac.sweep_claim (
  id      boolean primary key default true check (id),
  last_at timestamptz not null
);

create or replace function rebac.enqueue_refresh(t text, p text, pri smallint, token text) returns void
language sql volatile as $$
  insert into rebac.refresh (tenant, principal, priority, zedtoken, due_at, generation)
  values (t, p, pri, token, now(), 1)
  on conflict (tenant, principal) do update
    set priority = least(rebac.refresh.priority, excluded.priority),
        zedtoken = coalesce(excluded.zedtoken, rebac.refresh.zedtoken),
        due_at = least(rebac.refresh.due_at, now()),
        generation = rebac.refresh.generation + 1
$$;

-- CLAIM A RELAY PASS: per tenant, revocations first, commit order within each, at most `per_tenant` from any one,
-- and never a row whose relationship has an earlier row still undelivered, so no two relays reorder one
-- relationship. The tenant walk is a loose index scan (one probe per tenant with pending work), and each tenant
-- offers only its oldest `win` rows, so a flood in one tenant costs the others nothing.
drop function if exists rebac.claim_relay(integer, integer, integer);
create or replace function rebac.claim_relay(lim integer, per_tenant integer, win integer)
returns table (seq bigint, tenant text, op text, resource text, relation text, subject text, age_ms bigint)
language plpgsql volatile as $$
begin
  return query
  with candidates as (
    with recursive tenants(t) as (
      (select o.tenant from rebac.outbox o where o.synced_at is null order by o.tenant limit 1)
      union all
      select (select o.tenant from rebac.outbox o where o.synced_at is null and o.tenant > tenants.t
              order by o.tenant limit 1)
      from tenants where tenants.t is not null
    )
    select c.seq, c.revoke_last, c.rank
    from tenants cross join lateral (
      select e.seq, e.revoke_last, row_number() over (order by e.revoke_last, e.seq) as rank
      from (
        select w.seq, (w.op <> 'delete') as revoke_last
        from (select o.seq, o.op, o.resource, o.relation, o.subject from rebac.outbox o
              where o.tenant = tenants.t and o.synced_at is null order by o.seq limit win) w
        where not exists (select 1 from rebac.outbox p
                          where p.resource = w.resource and p.relation = w.relation and p.subject = w.subject
                            and p.seq < w.seq and p.synced_at is null)
        order by (w.op <> 'delete'), w.seq
        limit per_tenant
      ) e
    ) c
    where tenants.t is not null
    order by c.revoke_last, c.rank, c.seq
    limit lim
  ), locked as (
    -- Locked here and re-checked under the lock: another relay may have claimed or delivered a candidate since.
    select o.seq, o.tenant, o.op, o.resource, o.relation, o.subject, o.created_at
    from rebac.outbox o
    where o.seq in (select candidates.seq from candidates) and o.synced_at is null
      and not exists (select 1 from rebac.outbox p
                      where p.resource = o.resource and p.relation = o.relation and p.subject = o.subject
                        and p.seq < o.seq and p.synced_at is null)
    for update of o skip locked
  )
  select l.seq, l.tenant, l.op, l.resource, l.relation, l.subject,
         (extract(epoch from (now() - l.created_at)) * 1000)::bigint
  from locked l join candidates c on c.seq = l.seq
  order by c.revoke_last, c.rank, c.seq;
end $$;

-- Record a delivery: rows delivered (with the token SpiceDB wrote them at), rows SpiceDB refused on their own
-- (quarantined: kept, with the reason, never deleted), and the refreshes the delivered rows require, all in the
-- caller's transaction, so a delivery recorded is a refresh recorded.
create or replace function rebac.record_delivery(delivered bigint[], token text, refused bigint[], reasons text[])
returns integer
language plpgsql volatile as $$
declare queued integer := 0; n integer;
begin
  update rebac.outbox set synced_at = now(), zedtoken = token where seq = any(delivered);
  update rebac.outbox o set synced_at = now(), error = r.reason
  from unnest(refused, reasons) as r(seq, reason) where o.seq = r.seq;

  -- Direct and via relations name their subject: refresh the principals it stands for. A revocation first.
  with rows as (
    select o.tenant, o.op, o.subject
    from rebac.outbox o join rebac.view_relation v
      on v.resource_type = rebac.obj_type(o.resource) and v.relation = o.relation and v.kind in ('direct', 'via', 'set')
    where o.seq = any(delivered)
  ), targets as (
    select distinct r.tenant, p.principal, min(case when r.op = 'delete' then 0 else 1 end)::smallint as pri
    from rows r cross join lateral rebac.principals_of(r.tenant, r.subject) as p(principal)
    group by r.tenant, p.principal
  )
  select count(*) into n from targets t, lateral (select rebac.enqueue_refresh(t.tenant, t.principal, t.pri, token)) x;
  queued := queued + n;

  -- Arrow tuples and grants to a subject set name no reader: queue their resource for a LookupSubjects.
  insert into rebac.refresh_resource (tenant, resource, zedtoken, due_at, generation)
  select distinct o.tenant, o.resource, token, now(), 1
  from rebac.outbox o join rebac.view_relation v
    on v.resource_type = rebac.obj_type(o.resource) and v.relation = o.relation
   and (v.kind = 'arrow' or (v.kind = 'direct' and strpos(o.subject, '#') > 0))
  where o.seq = any(delivered)
  on conflict (tenant, resource) do update
    set zedtoken = excluded.zedtoken, due_at = least(rebac.refresh_resource.due_at, now()),
        generation = rebac.refresh_resource.generation + 1;
  get diagnostics n = row_count;
  return queued + n;
end $$;

-- Claim due refreshes under a lease; a lease that runs out is claimable again (a daemon that died mid-refresh).
create or replace function rebac.claim_refresh(lim integer, lease_ms integer)
returns table (tenant text, principal text, zedtoken text, attempts integer, generation bigint)
language sql volatile as $$
  update rebac.refresh r set leased_until = now() + lease_ms * interval '1 millisecond'
  from (select x.tenant, x.principal from rebac.refresh x
        where x.due_at <= now() and (x.leased_until is null or x.leased_until < now())
        order by x.priority, x.due_at limit lim for update skip locked) c
  where r.tenant = c.tenant and r.principal = c.principal
  returning r.tenant, r.principal, r.zedtoken, r.attempts, r.generation
$$;

-- Done: removed, unless it was enqueued again while it ran (then it stays, due now).
create or replace function rebac.complete_refresh(t text, p text, gen bigint) returns void
language sql volatile as $$
  delete from rebac.refresh where tenant = t and principal = p and generation = gen;
  update rebac.refresh set leased_until = null where tenant = t and principal = p
$$;

-- Failed: retried on a doubling ladder, capped at five minutes.
create or replace function rebac.fail_refresh(t text, p text, err text) returns void
language sql volatile as $$
  update rebac.refresh set attempts = attempts + 1, leased_until = null, last_error = left(err, 500),
    due_at = now() + least(power(2, least(attempts, 12)) * interval '1 second', interval '5 minutes')
  where tenant = t and principal = p
$$;

create or replace function rebac.claim_refresh_resource(lim integer, lease_ms integer)
returns table (tenant text, resource text, zedtoken text, generation bigint)
language sql volatile as $$
  update rebac.refresh_resource r set leased_until = now() + lease_ms * interval '1 millisecond'
  from (select x.tenant, x.resource from rebac.refresh_resource x
        where x.due_at <= now() and (x.leased_until is null or x.leased_until < now())
        order by x.due_at limit lim for update skip locked) c
  where r.tenant = c.tenant and r.resource = c.resource
  returning r.tenant, r.resource, r.zedtoken, r.generation
$$;

-- The subjects SpiceDB says can view a resource now: refresh the principals they stand for, and the resource is done.
create or replace function rebac.complete_refresh_resource(t text, res text, gen bigint, subjects text[], token text)
returns integer
language plpgsql volatile as $$
declare n integer;
begin
  select count(*) into n from (
    select distinct p.principal from unnest(subjects) s cross join lateral rebac.principals_of(t, s) as p(principal)
  ) x, lateral (select rebac.enqueue_refresh(t, x.principal, 1::smallint, token)) y;
  delete from rebac.refresh_resource where tenant = t and resource = res and generation = gen;
  update rebac.refresh_resource set leased_until = null where tenant = t and resource = res;
  return n;
end $$;

create or replace function rebac.fail_refresh_resource(t text, res text) returns void
language sql volatile as $$
  update rebac.refresh_resource set attempts = attempts + 1, leased_until = null,
    due_at = now() + least(power(2, least(attempts, 12)) * interval '1 second', interval '5 minutes')
  where tenant = t and resource = res
$$;

-- THE SLOW BACKSTOP. Claimed by one replica per interval. Re-sends each recent revocation ONCE, after it has settled
-- (a grant a frozen relay landed late is withdrawn by that one re-send, because the relay's ordering delivers it
-- after every earlier row for its relationship), and re-projects every principal at sweep priority.
create or replace function rebac.sweep(interval_ms bigint, settle_ms bigint, requeue boolean default true)
returns table (requeued bigint, principals bigint)
language plpgsql volatile as $$
declare r bigint := 0; p bigint := 0; settle bigint := least(settle_ms, interval_ms);
begin
  insert into rebac.sweep_claim (id, last_at) values (true, now())
  on conflict (id) do update set last_at = now()
  where rebac.sweep_claim.last_at <= now() - interval_ms * interval '1 millisecond';
  if not found then return query select 0::bigint, 0::bigint; return; end if;
  if requeue then
    with rq as (
      update rebac.outbox o set synced_at = null, zedtoken = null, requeued_at = now()
      where o.op = 'delete' and o.error is null and o.requeued_at is null
        and o.synced_at > now() - 2 * interval_ms * interval '1 millisecond'
        and o.synced_at <= now() - settle * interval '1 millisecond'
        and exists (select 1 from rebac.view_relation v
                    where v.resource_type = rebac.obj_type(o.resource) and v.relation = o.relation)
        and not exists (select 1 from rebac.outbox n where n.resource = o.resource and n.relation = o.relation
                          and n.subject = o.subject and n.seq > o.seq)
      returning 1)
    select count(*) into r from rq;
  end if;
  with everyone as (
    select tenant, principal from rebac.principal_state where state <> 'retired'
    union select tenant, principal from rebac.subject
  ), q as (
    insert into rebac.refresh (tenant, principal, priority, due_at, generation)
    select tenant, principal, 2, now(), 1 from everyone
    on conflict (tenant, principal) do update set generation = rebac.refresh.generation + 1,
      due_at = least(rebac.refresh.due_at, now())
    returning 1)
  select count(*) into p from q;
  return query select r, p;
end $$;

-- Liveness: the daemon beats this while it can reach SpiceDB and Postgres; reads fail closed when it stops.
create or replace function rebac.heartbeat(sc text default '*') returns void
language sql volatile as $$
  insert into rebac.watch_state (scope, heartbeat_at) values (sc, now())
  on conflict (scope) do update set heartbeat_at = now()
$$;

-- The daemon's closure transitions. INVALIDATE: reads fail closed from now on. ACCEPT: the rebuild is done; the
-- principals projected before `changed_at` still fail closed one by one until each is re-projected.
create or replace function rebac.invalidate_closure(why text) returns void
language sql volatile as $$
  insert into rebac.closure_state (id, state, reason, changed_at) values (true, 'invalid', why, now())
  on conflict (id) do update set state = 'invalid', reason = excluded.reason, changed_at = now()
$$;

create or replace function rebac.accept_closure() returns void
language sql volatile as $$
  insert into rebac.closure_state (id, state, reason, changed_at) values (true, 'valid', null, '-infinity')
  on conflict (id) do update set state = 'valid', reason = null
$$;
