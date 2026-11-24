-- The demo: one vector search, two people, what filtering after retrieval would have done instead, and a revocation.
\set QUIET on
\pset footer off
set rebac.tenant = 'acme';
-- Every fenced HNSW read sets this: without it, pgvector stops after ef_search candidates and the fence returns
-- fewer than k rows.
set hnsw.iterative_scan = relaxed_order;
-- The embedding of the query "salary pay review", in the seed's three dimensions (pay, sales, policy).
\set q '''[1, 0.05, 0.2]'''

\echo
\echo '1. The same vector search, top 5 nearest to "salary pay review", fenced inside the query:'
\echo
\echo '   as alice (reads handbook and payroll)'
select p.container, p.title
from passage p
where p.container in (select * from rebac.readable_set('alice'))
order by p.embedding <=> :q
limit 5;

\echo '   as bob (reads handbook and deals)'
select p.container, p.title
from passage p
where p.container in (select * from rebac.readable_set('bob'))
order by p.embedding <=> :q
limit 5;

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
from passage p
where p.container in (select * from rebac.readable_set('bob'))
order by p.embedding <=> :q
limit 5;

\echo '4. A principal nobody has asked about is an error (RB404), never an empty set:'
\echo
\set ON_ERROR_STOP off
\set VERBOSITY terse
select * from rebac.readable_set('mallory');
\set ON_ERROR_STOP on
