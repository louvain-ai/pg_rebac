-- Shared by both layouts: the model's memberships and node grants, materialised. -v principals=N
\set tenant '''tenant-0001'''
drop table if exists bench.member, bench.nc;
create unlogged table bench.member as
  select i, n.node from generate_series(0, :principals - 1) i cross join lateral bench.nodes_of(i) n(node);
create unlogged table bench.nc as select distinct node, container from bench.node_containers(:principals);
create index on bench.nc (node);
analyze bench.member, bench.nc;
update rebac.config set value = '0' where key in ('max_staleness_ms', 'max_projection_age_ms');
-- Built in bulk, then indexed as rebac.sql defines them.
alter table rebac.readable drop constraint readable_pkey;
drop index rebac.readable_container_idx;
