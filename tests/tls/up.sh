#!/usr/bin/env bash
# A Postgres that speaks TLS, for crates/rebacd/tests/tls.rs: a throwaway CA, a server certificate naming ONLY
# `localhost` (so 127.0.0.1 is a name mismatch), and a pgvector container on 127.0.0.1:55434.
#   tests/tls/up.sh      prints the REBAC_TLS_CA path to export
#   tests/tls/up.sh down
set -euo pipefail
NAME=rebac-tls-pg
DIR="$(cd "$(dirname "$0")" && pwd)/.certs"
if [ "${1:-}" = down ]; then docker rm -f "$NAME" >/dev/null 2>&1 || true; rm -rf "$DIR"; exit 0; fi
rm -rf "$DIR" && mkdir -p "$DIR"
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=rebac test CA" \
  -keyout "$DIR/ca.key" -out "$DIR/ca.pem" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" -keyout "$DIR/server.key" -out "$DIR/server.csr" 2>/dev/null
printf 'subjectAltName=DNS:localhost\n' > "$DIR/ext.cnf"
openssl x509 -req -in "$DIR/server.csr" -CA "$DIR/ca.pem" -CAkey "$DIR/ca.key" -CAcreateserial -days 2 \
  -extfile "$DIR/ext.cnf" -out "$DIR/server.crt" 2>/dev/null
docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" -p 127.0.0.1:55434:5432 -e POSTGRES_PASSWORD=rebac -v "$DIR:/certs:ro" \
  --entrypoint bash pgvector/pgvector:pg17 -c '
    cp /certs/server.crt /certs/server.key /tmp/ && chown postgres /tmp/server.* && chmod 600 /tmp/server.key
    exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/tmp/server.crt -c ssl_key_file=/tmp/server.key' >/dev/null
for _ in $(seq 1 60); do docker exec "$NAME" pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1 && break; sleep 1; done
echo "$DIR/ca.pem"
