#!/usr/bin/env bash
# Checks a quickstart run's output (CI): alice and bob saw different rows, bob kept one row of five when filtered after
# retrieval, the revocation removed deals from bob's search, and mallory was RB404. Each step's output starts at its
# `-- N.` comment line, the SQL the demo prints before running it.
set -euo pipefail
out="$1"
fail() { echo "quickstart output: $1" >&2; exit 1; }
# A step's lines, from its comment to the next step's.
section() { sed -n "/^-- $1\./,/^-- $2\./p" "$out"; }
alice="$(section 1 2 | sed '/^-- The same search as bob/q')"
bob="$(section 1 2 | sed -n '/^-- The same search as bob/,$p')"
after="$(section 2 3)"
revoked="$(section 3 4)"
mallory="$(sed -n '/^-- 4\./,$p' "$out")"
grep -q 'payroll   | Salary bands 2026' <<< "$alice" || fail "alice did not see payroll"
grep -q 'deals     | Commission plan' <<< "$bob" || fail "bob did not see deals"
grep -q 'payroll' <<< "$bob" && fail "bob saw payroll"
grep -qx '(1 row)' <<< "$after" || fail "filtering after retrieval did not leave bob 1 row"
grep -q 'deals     |' <<< "$revoked" && fail "the revocation did not remove deals from bob's search"
grep -q 'handbook  | Annual pay review' <<< "$revoked" || fail "bob's search after the revocation is empty"
grep -q '^ERROR:  RB404: rebac: this principal has not been projected' <<< "$mallory" || fail "mallory was not RB404"
echo "quickstart output: as expected"
