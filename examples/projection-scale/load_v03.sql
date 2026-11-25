-- 0.3's layout: each group's grant once, under its node; memberships; a principal's own shares minus what its nodes
-- hold (what rebac.project writes with node_grants on). -v principals=N
\ir load_common.sql
drop index if exists rebac.node_member_node_idx;
alter table rebac.node_member drop constraint node_member_pkey;
insert into rebac.readable (tenant, principal, container)
select :tenant, '#' || node, bench.cid(container) from bench.nc;
insert into rebac.node_member (tenant, principal, node)
select :tenant, bench.pid(i), '#' || node from bench.member;
insert into rebac.readable (tenant, principal, container)
select :tenant, bench.pid(i), bench.cid(c)
from generate_series(0, :principals - 1) i cross join lateral bench.direct_of(i) c
where not exists (select 1 from bench.member m join bench.nc using (node) where m.i = i and nc.container = c);
alter table rebac.readable add primary key (tenant, principal, container);
create index readable_container_idx on rebac.readable (tenant, container);
alter table rebac.node_member add primary key (tenant, principal, node);
create index node_member_node_idx on rebac.node_member (tenant, node);
insert into rebac.principal_state (tenant, principal, state, n_containers, version, projected_at, looked_up_at)
select :tenant, bench.pid(i), 'projected', 0, 1, now(), now() from generate_series(0, :principals - 1) i;
update rebac.principal_state s set n_containers = c.n
from (select principal, count(*) n from rebac.readable group by principal) c where c.principal = s.principal;
insert into rebac.principal_state (tenant, principal, state, n_containers, version, projected_at, looked_up_at)
select :tenant, '#' || node, 'projected', count(*), 1, now(), now() from bench.nc group by node;
vacuum analyze rebac.readable, rebac.principal_state, rebac.node_member;
