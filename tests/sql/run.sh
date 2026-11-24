#!/usr/bin/env bash
# Run the SQL suite: each test file in a fresh database with rebac.sql installed.
#   REBAC_TEST_URL=postgres://user:pass@host:port   (a server; databases are created and dropped)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
base="${REBAC_TEST_URL:-postgres://postgres:rebac@127.0.0.1:55433}"
export PGOPTIONS="-c client_min_messages=warning"
failed=0
for f in "$here"/[0-9]*.sql; do
  name="$(basename "$f" .sql)"
  db="rebac_t_${name//[^a-z0-9_]/_}_$$"
  psql "$base/postgres" -qAt -c "create database $db" >/dev/null
  if psql -v ON_ERROR_STOP=1 -qAt "$base/$db" -f "$root/sql/rebac.sql" -f "$here/_helpers.sql" -f "$f" >/dev/null 2>"$here/.$name.err"; then
    echo "ok   $name"
  else
    echo "FAIL $name"; sed 's/^/     /' "$here/.$name.err"; failed=1
  fi
  rm -f "$here/.$name.err"
  psql "$base/postgres" -qAt -c "drop database $db with (force)" >/dev/null
done
exit $failed
