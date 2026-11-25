-- The demo: one vector search, two people, what filtering after retrieval would have done instead, and a revocation.
\set QUIET on
\pset footer off
set rebac.tenant = 'acme';
-- The embedding of the query "salary pay review", in the seed's three dimensions (pay, sales, policy).
\set q '''[1, 0.05, 0.2]'''

\echo
\echo '1. The same vector search, top 5 nearest to "salary pay review", fenced inside the query:'
\echo
\echo '   as alice (reads handbook and payroll)'
-- rebac.fenced_ids picks the path for the reader's breadth: below exact_below_rows candidate rows (every reader
-- here) an exact sort, above it the fenced HNSW scan. Either way it returns k rows the reader may read, or raises.
select p.container, p.title
from rebac.fenced_ids('alice', 'passage', :q::vector, 5) f
join passage p on p.id::text = f.id
order by f.distance;

\echo '   as bob (reads handbook and deals)'
select p.container, p.title
from rebac.fenced_ids('bob', 'passage', :q::vector, 5) f
join passage p on p.id::text = f.id
order by f.distance;

\echo '2. Filtering AFTER retrieval instead: take the top 5 for everyone, then drop what bob may not read.'
\echo '   Passages bob cannot read (board) take 4 of the 5 places. bob asked for 5 rows and gets back:'
\echo
select count(*) as rows_left_for_bob
from (select p.container from passage p order by p.embedding <=> :q limit 5) top5
where top5.container in (select * from rebac.readable_set('bob'));

\echo '3. Revoke bob from deals. The revocation is in force when its transaction commits, before rebacd relays it:'
\echo
begin;
set local rebac.tenant = 'acme';
select rebac.write('delete', 'container:acme/deals', 'member', 'user:acme/bob') \g /dev/null
commit;
\echo '   bob, the same search again'
select p.container, p.title
from rebac.fenced_ids('bob', 'passage', :q::vector, 5) f
join passage p on p.id::text = f.id
order by f.distance;

\echo '4. A principal nobody has asked about is an error (RB404), never an empty set:'
\echo
-- The demo catches the error to print it as one line; an application sees it raised (SQLSTATE RB404).
do $$
begin
  perform * from rebac.readable_set('mallory');
  perform set_config('demo.mallory', 'returned a set (unexpected)', false);
exception when sqlstate 'RB404' then
  perform set_config('demo.mallory', sqlstate || ' "' || replace(sqlerrm, 'rebac: ', '') || '"', false);
end $$;
select current_setting('demo.mallory') as mallory \gset
\echo '   mallory →' :mallory
\echo
\echo 'You just saw one vector search return different rows per person, fenced inside the query, and a revocation'
\echo 'in force at commit. Stop it and forget everything:'
-- How this run is stopped: the served file and a clone's compose file each say (QUICKSTART_DOWN).
\getenv down QUICKSTART_DOWN
\echo ' ' :down
\echo 'Docs: https://pgrebac.com/docs/getting-started/'
