#!/usr/bin/env bash
# Seed, wait until rebacd has relayed the relationships to SpiceDB and projected both people, then run the demo.
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

psql "$DB" -v ON_ERROR_STOP=1 -q -P footer=off -f /quickstart/demo.sql
