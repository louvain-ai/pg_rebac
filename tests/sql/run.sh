#!/usr/bin/env bash
# Run the SQL suite: each test file in a fresh database with rebac.sql installed.
#   REBAC_TEST_URL=postgres://user:pass@host:port   (a server; databases are created and dropped)
#   tests/sql/run.sh [glob]                          (only the files matching it, e.g. '09_*')
#   REBAC_TEST_AS_OWNER=1 tests/sql/run.sh           (install and run as a database owner that is NOT a superuser, as
#                                                      on hosted Postgres; a superuser only creates the extensions)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
base="${REBAC_TEST_URL:-postgres://postgres:rebac@127.0.0.1:55433}"
export PGOPTIONS="-c client_min_messages=warning"
failed=0
owner="${REBAC_TEST_AS_OWNER:-}"
role="rebac_t_owner_$$"
if [ -n "$owner" ]; then
  psql "$base/postgres" -qAt -c "create role $role login nosuperuser password 'owner'" >/dev/null
  # The roles 06 and 12 switch to, created by a superuser and granted to the owner, as an administrator would.
  for r in rebac_test_app rebac_t_app rebac_t_trusted rebac_t_untrusted; do
    psql "$base/postgres" -qAt -c "do \$\$ begin if not exists (select 1 from pg_roles where rolname = '$r') then create role $r; end if; end \$\$" \
      -c "grant $r to $role" >/dev/null
  done
  trap 'psql "$base/postgres" -qAt -c "drop role if exists $role" >/dev/null' EXIT
fi
for f in "$here"/${1:-[0-9]*}.sql; do
  name="$(basename "$f" .sql)"
  db="rebac_t_${name//[^a-z0-9_]/_}_$$"
  url="$base/$db"
  if [ -n "$owner" ]; then
    psql "$base/postgres" -qAt -c "create database $db owner $role" >/dev/null
    # Neither extension is trusted, so a superuser creates them, as a hosted provider's console would.
    psql "$base/$db" -qAt -c "create extension if not exists vector" -c "create extension if not exists dblink" >/dev/null 2>&1 || true
    url="postgres://$role:owner@${base#*@}/$db"
  else
    psql "$base/postgres" -qAt -c "create database $db" >/dev/null
  fi
  extra=""
  [ -n "$owner" ] && extra="owner"
  if psql -v ON_ERROR_STOP=1 -qAt "$url" -f "$root/sql/rebac.sql" -f "$here/_helpers.sql" \
       -c "set t.dblink_password = '$extra'" -f "$f" >/dev/null 2>"$here/.$name.err"; then
    echo "ok   $name"
  else
    echo "FAIL $name"; sed 's/^/     /' "$here/.$name.err"; failed=1
  fi
  rm -f "$here/.$name.err"
  psql "$base/postgres" -qAt -c "drop database $db with (force)" >/dev/null
done
exit $failed
