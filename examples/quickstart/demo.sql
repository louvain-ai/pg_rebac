-- The demo: one query, two people, and what filtering after retrieval would have done instead.
\set QUIET on
\pset footer off
set rebac.tenant = 'acme';

\echo
\echo '1. The same search, "salary pay review", top 5, fenced inside the query:'
\echo
\echo '   as alice (handbook + payroll)'
select p.container, p.title
from passage p
where p.tsv @@ websearch_to_tsquery('english', 'salary or pay or review')
  and p.container in (select * from rebac.readable_set('alice'))
order by ts_rank(p.tsv, websearch_to_tsquery('english', 'salary or pay or review')) desc, p.id
limit 5;

\echo '   as bob (handbook + deals)'
select p.container, p.title
from passage p
where p.tsv @@ websearch_to_tsquery('english', 'salary or pay or review')
  and p.container in (select * from rebac.readable_set('bob'))
order by ts_rank(p.tsv, websearch_to_tsquery('english', 'salary or pay or review')) desc, p.id
limit 5;

\echo '2. Filtering AFTER retrieval instead: take the top 5 for everyone, then drop what bob may not read.'
\echo '   Containers bob cannot read (board, payroll) take 4 of the top 5 places. bob asked for 5 rows and gets back:'
\echo
select count(*) as rows_left_for_bob
from (
  select p.container
  from passage p
  where p.tsv @@ websearch_to_tsquery('english', 'salary or pay or review')
  order by ts_rank(p.tsv, websearch_to_tsquery('english', 'salary or pay or review')) desc, p.id
  limit 5
) top5
where top5.container in (select * from rebac.readable_set('bob'));

\echo '3. A revocation is in force when its transaction commits, before rebacd has relayed it:'
\echo
begin;
set local rebac.tenant = 'acme';
select rebac.write('delete', 'container:acme/deals', 'member', 'user:acme/bob') \g /dev/null
commit;
select coalesce(string_agg(c, ', ' order by c), '(nothing)') as bob_reads_now
from rebac.readable_set('bob') c;

\echo '4. A principal nobody has asked about is an error (RB404), never an empty set:'
\echo
\set ON_ERROR_STOP off
\set VERBOSITY terse
select * from rebac.readable_set('mallory');
\set ON_ERROR_STOP on
