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
# 3. As on a hosted provider: the extensions it ships created beside pg_rebac (pg_stat_statements needs no preload to be
# created), then the release SQL re-run over the installed one (an upgrade in place) as the same owner.
for ext in pg_stat_statements pgcrypto pg_trgm citext '"uuid-ossp"' btree_gin; do
  psql "$base/$db" -qAt -c "create extension if not exists $ext" >/dev/null 2>&1 || echo "     (no $ext here)"
done
psql -v ON_ERROR_STOP=1 -qAt "$owner_url" -f "$root/sql/rebac.sql" >/dev/null
echo "ok   re-install over itself beside the usual hosted extensions"
# 4. The daemon as a role of its own, with privileges on schema rebac only (operations docs: the rebacd role).
daemon="rebacd_t_$$"
psql "$base/postgres" -qAt -c "create role $daemon login nosuperuser password 'daemon'" >/dev/null
trap 'cleanup; psql "$base/postgres" -qAt -c "drop owned by '"$daemon"' cascade" -c "drop role if exists '"$daemon"'" >/dev/null 2>&1 || true' EXIT
psql -v ON_ERROR_STOP=1 -qAt "$owner_url" >/dev/null <<SQL
grant usage on schema rebac to $daemon;
grant select, insert, update, delete on all tables in schema rebac to $daemon;
grant usage, select on all sequences in schema rebac to $daemon;
grant execute on all functions in schema rebac to $daemon;
SQL
daemon_url="postgres://$daemon:daemon@$hostpart/$db"
psql -v ON_ERROR_STOP=1 -qAt "$daemon_url" >/dev/null <<'SQL'
select rebac.heartbeat('*');
select rebac.watch_required(true);
select pg_try_advisory_lock(1919050596, 1);
select rebac.assert_leader(pg_backend_pid());
select rebac.watch_restarted('z0');
select rebac.project('acme', 'ana', array['handbook'], 'z0', 'live', now());
select * from rebac.claim_relay(10, 10, 10);
select * from rebac.claim_refresh(10, 1000);
select * from rebac.apply_watch(array['delete'], array['container:acme/handbook'], array['member'],
                                array['user:acme/ana'], 'z1');
select * from rebac.sweep(0, 0, true);
select * from rebac.status();
SQL
echo "ok   the daemon's calls as a role with privileges on schema rebac only"
