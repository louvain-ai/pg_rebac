#!/usr/bin/env bash
# The upgrade from a 0.2 release: install its SQL (tests/upgrade/releases/, each exactly the release's sql/rebac.sql),
# project principals as 0.2 did (every grant expanded per principal), then re-run this checkout's sql/rebac.sql twice
# and check that, before any refresh, every read is the same and every revocation is still in force at commit.
#   REBAC_TEST_URL=postgres://user:pass@host:port tests/upgrade/run.sh [version]   (default 0.2.1)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
base="${REBAC_TEST_URL:-postgres://postgres:rebac@127.0.0.1:55433}"
from="${1:-0.2.1}"
export PGOPTIONS="-c client_min_messages=warning"
db="rebac_t_upgrade_$$"
old="$here/releases/rebac-$from.sql"
[ -f "$old" ] || { echo "no released SQL for $from in $here/releases" >&2; exit 2; }
trap 'psql "$base/postgres" -qAt -c "drop database if exists $db with (force)" >/dev/null' EXIT
psql "$base/postgres" -qAt -c "create database $db" >/dev/null
psql -v ON_ERROR_STOP=1 -qAt "$base/$db" -f "$old" -f "$root/tests/sql/_helpers.sql" -f "$here/before.sql" >/dev/null
psql -v ON_ERROR_STOP=1 -qAt "$base/$db" -f "$root/sql/rebac.sql" -f "$root/sql/rebac.sql" -f "$root/tests/sql/_helpers.sql" \
  -f "$here/after.sql" >/dev/null
echo "ok   upgrade from $from"
