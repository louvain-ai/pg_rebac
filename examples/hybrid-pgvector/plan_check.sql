-- Asserts that a fenced vector leg uses the HNSW index, at a narrow and a broad reader. Run after setup.sql.
set rebac.tenant = 'acme';
set hnsw.iterative_scan = relaxed_order;
create temp table plan_lines (reader text, line text);
do $$
declare who text; r record;
begin
  foreach who in array array['b005', 'b030', 'b100'] loop
    for r in execute format(
      'explain (costs off) select id from passage where tenant = %L and container in (select * from rebac.readable_set(%L)) '
      'order by embedding <=> (select embedding from passage where id = 7) limit 20', 'acme', who) loop
      insert into plan_lines values (who, r."QUERY PLAN");
    end loop;
  end loop;
end $$;
do $$
declare who text;
begin
  foreach who in array array['b005', 'b030', 'b100'] loop
    if not exists (select 1 from plan_lines where plan_lines.reader = who and line like '%passage_embedding_idx%') then
      raise exception 'FAIL: the fenced vector leg for % does not use the HNSW index', who;
    end if;
    if not exists (select 1 from plan_lines where plan_lines.reader = who and line like '%readable_pkey%') then
      raise exception 'FAIL: the fence for % is not a primary-key probe', who;
    end if;
  end loop;
end $$;
\echo ok: the fenced vector leg uses passage_embedding_idx with a readable_pkey probe at 5%, 30% and 100% breadth
