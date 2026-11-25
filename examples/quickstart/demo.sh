#!/usr/bin/env bash
# Seed, wait until rebacd has relayed the relationships to SpiceDB and projected both people, then run the demo: one
# vector search as two people, what filtering after retrieval would have done instead, a revocation and an unknown
# principal. Each step prints the SQL it runs, then psql's result; the printed text is the string psql is given.
set -euo pipefail
q() { psql "$DB" -v ON_ERROR_STOP=1 -qAt "$@"; }

q -f /quickstart/seed.sql >/dev/null

echo "Waiting for rebacd to relay the relationships to SpiceDB and project alice and bob..."
# Ready means the reads the demo makes succeed: both people projected, and the daemon live (heartbeating).
want="alice:handbook,payroll bob:deals,handbook"
ready=""
for _ in $(seq 1 90); do
  ready=$(q -c "set rebac.tenant = 'acme'" -c "select string_agg(who || ':' || cs, ' ' order by who) from (
                  select who, (select string_agg(c, ',' order by c) from rebac.readable_set(who) c) as cs
                  from unnest(array['alice', 'bob']) who) x" 2>/dev/null) || ready=""
  [ "$ready" = "$want" ] && break
  sleep 1
done
if [ "$ready" != "$want" ]; then
  echo "rebacd did not project alice and bob within 90 s; see the rebacd service's logs (docker compose ... logs rebacd)" >&2
  exit 1
fi

# Colour only on a terminal, and never with NO_COLOR set (https://no-color.org): comments dim, keywords bold, the
# rebac call in cyan.
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  show() {
    sed -E -e $'/^--/{s/.*/\e[2m&\e[0m/;b\n}' \
      -e $'s/\\b(select|from|join|on|where|in|order by|limit|set|begin|commit)\\b/\e[1m&\e[0m/g' \
      -e $'s/rebac\\.[a-z_]+\\([^)]*\\)/\e[1;36m&\e[0m/g'
  }
else
  show() { cat; }
fi

# Every step is its own session, and opens with the tenant it reads in.
tenant="set rebac.tenant = 'acme';"
# One step: print the SQL on stdin, then run that same string. Extra arguments go to psql.
step() {
  local sql
  sql="$(cat)"
  printf '%s\n' "$sql" | show
  psql "$DB" -X -q -v ON_ERROR_STOP=1 -c "$tenant" -c "$sql" "$@"
}

# The query "salary pay review", embedded in the seed's three dimensions (pay, sales, policy).
v="'[1,0.05,0.2]'::vector"
search() {
  cat <<SQL
select p.container, p.title
from rebac.fenced_ids('$1', 'passage', $v, 5) f
join passage p on p.id::text = f.id
order by f.distance;
SQL
}

echo
printf '%s\n' "-- Each step below runs in its own session, which first sets its tenant:" "$tenant" | show
echo
step <<SQL
-- 1. One vector search, fenced inside the query: the 5 nearest passages alice may read.
$(search alice)
SQL
step <<SQL
-- The same search as bob: different rows, still 5.
$(search bob)
SQL
step <<SQL
-- 2. Filtering after retrieval instead: the top 5 for everyone, then drop what bob may not read.
--    Board passages, which bob cannot read, took 4 of the 5 places.
select p.container, p.title
from (select container, title from passage
      order by embedding <=> $v limit 5) p
where p.container in (select * from rebac.readable_set('bob'));
SQL
# rebac.write returns the outbox sequence number it wrote, which says nothing here.
step -o /dev/null <<SQL
-- 3. Revoke bob from deals. The revocation is in force when this transaction commits,
--    before rebacd has relayed it to SpiceDB.
begin;
select rebac.write('delete', 'container:acme/deals', 'member', 'user:acme/bob');
commit;
SQL
echo
step <<SQL
-- bob, the same search straight after the commit: no deals.
$(search bob)
SQL
# The error is the result: verbose names its SQLSTATE; the source location in pl_exec.c is left out.
if out="$(step -v VERBOSITY=verbose -v SHOW_CONTEXT=never 2>&1 <<SQL
-- 4. A principal nobody has asked about is an error, never an empty set.
select * from rebac.readable_set('mallory');
SQL
)"; then
  printf '%s\n' "$out"
  echo "mallory's read returned a set; it must be RB404" >&2
  exit 1
fi
grep -v '^LOCATION:' <<< "$out"
grep -q '^ERROR:  RB404:' <<< "$out" || { echo "mallory's read failed, but not with RB404" >&2; exit 1; }

echo
echo 'You just saw one vector search return different rows per person, fenced inside the query, and a revocation'
echo 'in force at commit. Stop it and forget everything:'
# How this run is stopped: the served file and a clone's compose file each say (QUICKSTART_DOWN).
echo "  $QUICKSTART_DOWN"
echo 'Docs: https://pgrebac.com/docs/getting-started/'
