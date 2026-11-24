# Security

## Reporting a vulnerability

Please report a vulnerability privately, never in a public issue or pull request. Use this repository's private
vulnerability reporting ("Security" tab, then "Report a vulnerability"), which reaches the maintainers only.

Include what you found, the commit, how to reproduce it, and what an attacker could do with it. We will acknowledge the
report, keep you informed while we work on it, and credit you in the release that fixes it unless you ask us not to.

## Scope

In scope: `sql/rebac.sql`, the `rebacd` daemon and the image the `Dockerfile` builds. In particular, any way for a
read through `rebac.readable_set`, `rebac.readable_by` or a policy from `rebac.enable_rls` to return, count or rank a
row its principal may not view according to SpiceDB, beyond the staleness bound the configuration states; a read in one
tenant that sees another's rows; a revocation written through `rebac.write` that is not in force when its transaction
commits; or an error that becomes an empty set or rows.

Out of scope: SpiceDB itself, a schema shape pg_rebac documents as refused, and the quickstart's throwaway credentials
(`examples/quickstart/`), which guard nothing and listen on no host port.
