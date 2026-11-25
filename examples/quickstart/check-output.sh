#!/usr/bin/env bash
# Checks a quickstart run's output (CI): alice and bob saw different rows, bob kept one row of five when filtered after
# retrieval, the revocation removed deals from bob's search, and mallory was RB404.
set -euo pipefail
out="$1"
fail() { echo "quickstart output: $1" >&2; exit 1; }
# bob's first search holds deals rows; his search after the revocation (section 3 to 4) holds none.
grep -q 'payroll   | Salary bands 2026' "$out" || fail "alice did not see payroll"
sed -n '/as bob/,/^2\./p' "$out" | grep -q 'deals     | Commission plan' || fail "bob did not see deals"
sed -n '/as bob/,/^2\./p' "$out" | grep -q 'payroll' && fail "bob saw payroll"
sed -n '/rows_left_for_bob/,/^3\./p' "$out" | grep -qx ' *1' || fail "filtering after retrieval did not leave bob 1 row"
sed -n '/^3\./,/^4\./p' "$out" | grep -q 'deals     |' && fail "the revocation did not remove deals from bob's search"
sed -n '/^3\./,/^4\./p' "$out" | grep -q 'handbook  | Annual pay review' || fail "bob's search after the revocation is empty"
grep -q 'mallory → RB404 "this principal has not been projected"' "$out" || fail "mallory was not RB404"
echo "quickstart output: as expected"
