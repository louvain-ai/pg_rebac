# Refresh churn

How much a refresh writes when a large tenant's memberships change a little. `churn.sql` builds one tenant of
`principals` × `width` projection rows, then re-projects `per_round` principals per round with `churn` of their
containers replaced, and reports time, WAL bytes and rows inserted and deleted in `rebac.readable` per round.

```sh
psql $DB/postgres -c 'create database rebac_churn'
psql -v ON_ERROR_STOP=1 $DB/rebac_churn -f sql/rebac.sql
psql $DB/rebac_churn -v principals=1000 -v width=5000 -v per_round=200 -v churn=0.01 -v rounds=5 \
  -f examples/project-churn/churn.sql
```

The results, before and after `rebac.project` became a diff, are on the site's performance page.
