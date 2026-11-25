-- 0.2.0's layout: every grant expanded per principal (what rebac.project wrote in 0.2.0). -v principals=N
\ir load_common.sql
insert into rebac.readable (tenant, principal, container)
select :tenant, bench.pid(x.i), bench.cid(x.c) from (
  select m.i, nc.container as c from bench.member m join bench.nc using (node)
  union
  select i, c from generate_series(0, :principals - 1) i cross join lateral bench.direct_of(i) c
) x;
alter table rebac.readable add primary key (tenant, principal, container);
create index readable_container_idx on rebac.readable (tenant, container);
insert into rebac.principal_state (tenant, principal, state, n_containers, version, projected_at, looked_up_at)
select tenant, principal, 'projected', count(*), 1, now(), now() from rebac.readable group by tenant, principal;
vacuum analyze rebac.readable, rebac.principal_state;
