#!/usr/bin/env bash
# rebac.sql installs as a database owner that is NOT a superuser (hosted Postgres: Neon, RDS, Cloud SQL), with and
# without pgvector, and rebac.fenced_ids then runs as that role. The rest of the suite runs as a superuser.
#   REBAC_TEST_URL=postgres://postgres:pass@host:port tests/sql/install_as_owner.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
base="${REBAC_TEST_URL:-postgres://postgres:rebac@127.0.0.1:55433}"
export PGOPTIONS="-c client_min_messages=warning"
db="rebac_t_owner_$$"
role="rebac_owner_$$"
hostpart="${base#*@}"
owner_url="postgres://$role:owner@$hostpart/$db"
cleanup() {
  psql "$base/postgres" -qAt -c "drop database if exists $db with (force)" -c "drop role if exists $role" >/dev/null
}
trap cleanup EXIT
psql "$base/postgres" -qAt -c "create role $role login nosuperuser password 'owner'" -c "create database $db owner $role" >/dev/null
# 1. No pgvector in the database: the install must still complete.
psql -v ON_ERROR_STOP=1 -qAt "$owner_url" -f "$root/sql/rebac.sql" >/dev/null
psql -qAt "$owner_url" -c "select 'rebac.project' :: regproc" >/dev/null
echo "ok   install as a non-superuser owner, no pgvector"
# 2. pgvector added by a superuser (it is not a trusted extension), in a fresh session: re-install, then search.
if psql "$base/$db" -qAt -c "create extension if not exists vector" >/dev/null 2>&1; then
  psql -v ON_ERROR_STOP=1 -qAt "$owner_url" -f "$root/sql/rebac.sql" >/dev/null
  psql -v ON_ERROR_STOP=1 -qAt "$owner_url" -f "$here/_helpers.sql" -f "$here/14_fenced_ids.sql" >/dev/null
  echo "ok   re-install and rebac.fenced_ids as a non-superuser owner, with pgvector"
else
  echo "SKIP pgvector is not available on this server"
fi
