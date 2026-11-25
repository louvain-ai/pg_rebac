#!/usr/bin/env bash
# The manual fallback. A release is normally the merge of release-please's pull request, and publish.yml does the
# rest (CONTRIBUTING.md, "Releasing"). Use this only when that path is unavailable, from the release commit:
#   scripts/release.sh <version> [--dry-run]
# RELEASE_BRANCH=release-0.2 releases a patch from a maintenance branch (default: main).
# Each publishing step asks first and skips what is already published. --dry-run runs every step's dry-run form
# (no tag, no push, no publish).
# Needs: git remote `origin` (push access), cargo logged in to crates.io, npm logged in with publish rights to
# pg-rebac, and gh logged in for the last step.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
version="${1:?usage: release.sh <version> [--dry-run]}"
dry=0; [[ "${2:-}" == "--dry-run" ]] && dry=1
tag="v$version"
branch="${RELEASE_BRANCH:-main}"

confirm() { # confirm <what>
  if [ "$dry" = 1 ]; then echo "[dry-run] $1"; return 0; fi
  read -r -p "$1 -- proceed? [y/N] " a; [[ "$a" == "y" || "$a" == "Y" ]] || { echo "stopped"; exit 1; }
}
on_crates_io() { curl -fsS "https://index.crates.io/re/ba/$1" | jq -e --arg v "$version" 'select(.vers == $v)' >/dev/null; }

cargo_v="$(cargo metadata --locked --no-deps --format-version 1 | jq -r '.packages[] | select(.name=="rebacd") | .version')"
npm_v="$(jq -r .version packages/npm/package.json)"
meta_v="$(jq -r .version META.json)"
for v in "$cargo_v" "$npm_v" "$meta_v"; do
  [ "$v" = "$version" ] || { echo "version mismatch: Cargo $cargo_v, npm $npm_v, META.json $meta_v, wanted $version" >&2; exit 1; }
done
[ -z "$(git status --porcelain)" ] || { echo "working tree is not clean" >&2; exit 1; }
if [ "$dry" = 0 ]; then
  git fetch origin "$branch"
  [ "$(git rev-parse HEAD)" = "$(git rev-parse "origin/$branch")" ] || { echo "HEAD is not origin/$branch: merge the release branch and push first" >&2; exit 1; }
fi

echo "== 1. tag and push"
if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null; then
  echo "$tag is already on origin"
else
  confirm "git tag $tag && git push origin $tag"
  [ "$dry" = 1 ] || { git tag -a "$tag" -m "pg_rebac $version" && git push origin "$tag"; }
fi

echo "== 2. crates.io: rebac-schema, then rebacd (cargo waits for the index between them)"
crates=()
for c in rebac-schema rebacd; do
  if on_crates_io "$c"; then echo "$c $version is on crates.io"; else crates+=(-p "$c"); fi
done
if [ "${#crates[@]}" -gt 0 ]; then
  if [ "$dry" = 1 ]; then
    cargo publish --locked --dry-run --allow-dirty "${crates[@]}"
  else
    confirm "cargo publish ${crates[*]}"; cargo publish --locked "${crates[@]}"
  fi
fi

echo "== 3. npm: pg-rebac"
if npm view "pg-rebac@$version" version >/dev/null 2>&1; then
  echo "pg-rebac $version is on npm"
else
  (cd packages/npm && npm install --no-audit --no-fund --no-package-lock >/dev/null)
  if [ "$dry" = 1 ]; then
    (cd packages/npm && npm publish --dry-run)
  else
    confirm "npm publish (packages/npm)"; (cd packages/npm && npm publish)
  fi
fi

echo "== 4. the image, the GitHub Release's assets and PGXN: publish.yml, which skips what steps 2 and 3 published"
confirm "gh workflow run publish.yml --ref $tag -f tag=$tag -f dry-run=false"
[ "$dry" = 1 ] || gh workflow run publish.yml --ref "$tag" -f tag="$tag" -f dry-run=false
echo "Without GitHub Actions: make dist VERSION=$version, then upload dist/pg_rebac-$version.zip at https://manager.pgxn.org/"
