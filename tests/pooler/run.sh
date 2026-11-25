#!/usr/bin/env bash
# pg_rebac through PgBouncer in TRANSACTION mode, with ONE server connection, so every client below shares the same
# Postgres backend and anything that leaks across transactions leaks here.
#   REBAC_TEST_URL=postgres://postgres:rebac@127.0.0.1:55433 tests/pooler/run.sh
# It proves:
#   1. a session-level SET of rebac.tenant / rebac.principal LEAKS to the next client on the backend, and SET LOCAL
#      does not: through a transaction pooler, set them with SET LOCAL (or set_config(..., true)) inside the
#      transaction that reads;
#   2. a fenced read, a revocation through rebac.write (narrowed at its commit) and rebac.set_principal's binding all
#      work through the pooler, and the binding ends with its transaction;
#   3. a session advisory lock taken through the pooler outlives the client that took it (it belongs to the server
#      connection), which is why rebacd's leader lock connects to Postgres itself or to a session-mode pooler.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
base="${REBAC_TEST_URL:-postgres://postgres:rebac@127.0.0.1:55433}"
hostport="${base#*@}"
port="${REBAC_POOLER_PORT:-56432}"
db="rebac_pooler_$$"
name="rebac-pgbouncer-$$"
export PGOPTIONS="-c client_min_messages=warning"
cleanup() {
  docker rm -f "$name" >/dev/null 2>&1 || true
  psql "$base/postgres" -qAt -c "drop database if exists $db with (force)" >/dev/null 2>&1 || true
}
trap cleanup EXIT

psql "$base/postgres" -qAt -c "create database $db" >/dev/null
psql -v ON_ERROR_STOP=1 -qAt "$base/$db" -f "$root/sql/rebac.sql" -f "$root/tests/sql/_helpers.sql" >/dev/null
psql -v ON_ERROR_STOP=1 -qAt "$base/$db" >/dev/null <<'SQL'
select t.fixture_relations();
select rebac.project('acme', 'ana', array['handbook', 'payroll'], 't', 'live', now());
create table doc (id int primary key, container text);
insert into doc values (1, 'handbook'), (2, 'payroll'), (3, 'board');
select rebac.enable_rls('doc', 'container', force => true, principal_source => 'session');
do $$ begin
  if not exists (select 1 from pg_roles where rolname = 'rebac_pool_reader') then create role rebac_pool_reader; end if;
end $$;
grant usage on schema rebac to rebac_pool_reader;
grant execute on function rebac.session_readable_set() to rebac_pool_reader;
grant select on doc to rebac_pool_reader;
SQL

# The pooler reaches Postgres on the host's published port; on Linux CI that is the host network.
pg_host="${hostport%:*}"; pg_port="${hostport##*:}"
[ "$pg_host" = "127.0.0.1" ] || [ "$pg_host" = "localhost" ] && pg_host="host.docker.internal"
docker run -d --name "$name" --add-host=host.docker.internal:host-gateway -p "127.0.0.1:$port:5432" \
  -e DB_HOST="$pg_host" -e DB_PORT="$pg_port" -e DB_USER=postgres -e DB_PASSWORD=rebac -e DB_NAME="$db" \
  -e POOL_MODE=transaction -e DEFAULT_POOL_SIZE=1 -e MIN_POOL_SIZE=0 -e MAX_DB_CONNECTIONS=1 \
  -e AUTH_TYPE=scram-sha-256 -e IGNORE_STARTUP_PARAMETERS=extra_float_digits,options \
  edoburu/pgbouncer:v1.26.1-p0 >/dev/null
pool="postgres://postgres:rebac@127.0.0.1:$port/$db"
for _ in $(seq 60); do psql "$pool" -qAt -c "select 1" >/dev/null 2>&1 && break; sleep 0.5; done

q() { PGOPTIONS= psql "$pool" -v ON_ERROR_STOP=1 -qAtX "$@"; }
fail=0
check() { # check <what> <expected> <actual>
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: expected [$2], got [$3]"; fail=1; fi
}

# Every client shares the one backend.
check "one shared backend" "$(q -c 'select pg_backend_pid()')" "$(q -c 'select pg_backend_pid()')"

# 1. A session-level SET leaks to the next client; SET LOCAL does not.
q -c "set rebac.tenant = 'acme'" >/dev/null
check "session SET leaks to the next client (the footgun)" "acme" "$(q -c "select coalesce(current_setting('rebac.tenant', true), '')")"
q -c "reset rebac.tenant" >/dev/null
q -c "begin; set local rebac.tenant = 'acme'; commit;" >/dev/null
check "SET LOCAL ends with its transaction" "" "$(q -c "select coalesce(current_setting('rebac.tenant', true), '')")"

# 2. A fenced read in one transaction, with the tenant set local.
q -c "select t.beat()" >/dev/null
check "a fenced read through the pooler" "handbook,payroll" \
  "$(q -c "begin; set local rebac.tenant = 'acme'; select string_agg(c, ',' order by c) from rebac.readable_set('ana') c; commit;" | sed -n 1p)"

# A revocation through rebac.write is in force at its commit, read by the next client.
q -c "begin; set local rebac.tenant = 'acme'; select rebac.write('delete', 'container:acme/payroll', 'member', 'user:acme/ana'); commit;" >/dev/null
check "a revocation through the pooler is in force at commit" "handbook" \
  "$(q -c "begin; set local rebac.tenant = 'acme'; select string_agg(c, ',' order by c) from rebac.readable_set('ana') c; commit;" | sed -n 1p)"

# set_principal binds for one transaction; the next client on the same backend has no binding.
check "set_principal binds within its transaction" "1" \
  "$(q -c "begin; select rebac.set_principal('acme', 'ana'); set local role rebac_pool_reader; select string_agg(id::text, ',') from doc; commit;" | sed -n 2p)"
out="$(q -c "begin; set local rebac.tenant = 'acme'; set local role rebac_pool_reader; select count(*) from doc; commit;" 2>&1 || true)"
case "$out" in *"no principal is bound"*) echo "ok   the binding does not outlive its transaction";;
  *) echo "FAIL the binding outlived its transaction: $out"; fail=1;; esac

# 3. A session advisory lock taken through the pooler outlives its client.
q -c "select pg_try_advisory_lock(1919050596, 99)" >/dev/null
check "a session advisory lock outlives its client (so rebacd's lock needs a real session)" "t" \
  "$(psql "$base/$db" -qAtX -c "select exists (select 1 from pg_locks where locktype = 'advisory' and classid = 1919050596 and objid = 99)")"

exit $fail
