#!/usr/bin/env bash
# Generate the versioned SQL artefact from sql/rebac.sql: dist/pg_rebac--<version>.sql.
#   scripts/release-sql.sh 0.1.0 [outfile]
# The artefact is rebac.sql under a version banner; nothing else is changed. It is generated at release time
# and never committed (dist/ is ignored), so it cannot go stale.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
version="${1:?usage: release-sql.sh <version> [outfile]}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.]+)?$ ]] || { echo "not a version: $version" >&2; exit 2; }
out="${2:-$root/dist/pg_rebac--$version.sql}"
mkdir -p "$(dirname "$out")"
{
  echo "-- pg_rebac $version (generated from sql/rebac.sql by scripts/release-sql.sh; do not edit)."
  echo "-- Install: psql -v ON_ERROR_STOP=1 -f $(basename "$out")"
  echo
  cat "$root/sql/rebac.sql"
} > "$out"
echo "$out"
