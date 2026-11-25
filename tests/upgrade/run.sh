#!/usr/bin/env bash
# THE UPGRADE FROM 0.2.0: install the released SQL, project principals as 0.2 did (every grant expanded per
# principal), then re-run this checkout's sql/rebac.sql twice and check that, before any refresh, every read is the
# same and every revocation is still in force at commit.
#   REBAC_TEST_URL=postgres://user:pass@host:port tests/upgrade/run.sh [from-ref]   (default v0.2.0)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
base="${REBAC_TEST_URL:-postgres://postgres:rebac@127.0.0.1:55433}"
from="${1:-v0.2.0}"
export PGOPTIONS="-c client_min_messages=warning"
db="rebac_t_upgrade_$$"
old="$(mktemp)"
trap 'rm -f "$old"; psql "$base/postgres" -qAt -c "drop database if exists $db with (force)" >/dev/null' EXIT
git -C "$root" show "$from:sql/rebac.sql" > "$old"
psql "$base/postgres" -qAt -c "create database $db" >/dev/null
psql -v ON_ERROR_STOP=1 -qAt "$base/$db" -f "$old" -f "$root/tests/sql/_helpers.sql" -f "$here/before.sql" >/dev/null
psql -v ON_ERROR_STOP=1 -qAt "$base/$db" -f "$root/sql/rebac.sql" -f "$root/sql/rebac.sql" -f "$root/tests/sql/_helpers.sql" \
  -f "$here/after.sql" >/dev/null
echo "ok   upgrade from $from"
