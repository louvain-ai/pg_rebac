#!/usr/bin/env bash
# The whole release, in order, by the owner, after the release is approved:
#   scripts/release.sh <version> [--dry-run]
# Each publishing step asks first. --dry-run runs every step's dry-run form (no tag, no push, no publish).
# Needs: git remote `origin` (push access), cargo logged in to crates.io, npm logged in with publish rights to pg-rebac.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
version="${1:?usage: release.sh <version> [--dry-run]}"
dry=0; [[ "${2:-}" == "--dry-run" ]] && dry=1
tag="v$version"

confirm() { # confirm <what>
  if [ "$dry" = 1 ]; then echo "[dry-run] $1"; return 0; fi
  read -r -p "$1 -- proceed? [y/N] " a; [[ "$a" == "y" || "$a" == "Y" ]] || { echo "stopped"; exit 1; }
}

cargo_v="$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name=="rebacd") | .version')"
npm_v="$(jq -r .version packages/npm/package.json)"
meta_v="$(jq -r .version META.json)"
for v in "$cargo_v" "$npm_v" "$meta_v"; do
  [ "$v" = "$version" ] || { echo "version mismatch: Cargo $cargo_v, npm $npm_v, META.json $meta_v, wanted $version" >&2; exit 1; }
done
[ -z "$(git status --porcelain)" ] || { echo "working tree is not clean" >&2; exit 1; }
if [ "$dry" = 0 ]; then
  git fetch origin main
  [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || { echo "HEAD is not origin/main: merge the release branch and push first" >&2; exit 1; }
fi

echo "== 1. tag and push (starts the GitHub Release and the ghcr image workflows)"
confirm "git tag $tag && git push origin $tag"
[ "$dry" = 1 ] || { git tag -a "$tag" -m "pg_rebac $version" && git push origin "$tag"; }

echo "== 2. crates.io: rebac-schema"
if [ "$dry" = 1 ]; then
  cargo publish --dry-run -p rebac-schema --allow-dirty
else
  confirm "cargo publish -p rebac-schema"; cargo publish -p rebac-schema
  echo "waiting for the crates.io index..."
  for _ in $(seq 1 30); do
    cargo search rebac-schema --limit 1 2>/dev/null | grep -q "\"$version\"" && break; sleep 10
  done
fi

echo "== 3. crates.io: rebacd"
if [ "$dry" = 1 ]; then
  cargo publish --dry-run -p rebacd --allow-dirty || echo "[dry-run] rebacd cannot resolve rebac-schema until it is on the registry (expected)"
else
  confirm "cargo publish -p rebacd"; cargo publish -p rebacd
fi

echo "== 4. npm: pg-rebac"
(cd packages/npm && npm install --no-audit --no-fund --no-package-lock >/dev/null)
if [ "$dry" = 1 ]; then
  (cd packages/npm && npm publish --dry-run --access public)
else
  confirm "npm publish --access public (packages/npm)"; (cd packages/npm && npm publish --access public)
fi

echo "== 5. PGXN: build the zip (the upload is manual)"
make dist
echo "Upload $root/dist/pg_rebac-$version.zip at https://manager.pgxn.org/"
