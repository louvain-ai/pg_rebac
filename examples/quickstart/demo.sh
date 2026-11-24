#!/usr/bin/env bash
# Seed, wait until rebacd has relayed the relationships to SpiceDB and projected both people, then run the demo.
set -euo pipefail
q() { psql "$DB" -v ON_ERROR_STOP=1 -qAt "$@"; }

q -f /quickstart/seed.sql >/dev/null

echo "Waiting for rebacd to relay the relationships to SpiceDB and project alice and bob..."
for _ in $(seq 1 60); do
  ready=$(q -c "select string_agg(principal || ':' || containers, ' ' order by principal) from (
                  select principal, string_agg(container, ',' order by container) as containers
                  from rebac.readable where tenant = 'acme' and principal in ('alice', 'bob') group by principal) x")
  [ "$ready" = "alice:handbook,payroll bob:deals,handbook" ] && break
  sleep 1
done
if [ "$ready" != "alice:handbook,payroll bob:deals,handbook" ]; then
  echo "rebacd did not project alice and bob within 60 s; see the rebacd service's logs (docker compose ... logs rebacd)" >&2
  exit 1
fi

psql "$DB" -v ON_ERROR_STOP=1 -q -P footer=off -f /quickstart/demo.sql
