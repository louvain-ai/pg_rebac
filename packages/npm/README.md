# @louvain/pg-rebac

The [pg_rebac](https://pgrebac.com) SQL, bundled for your migrations. No database driver dependency.

```ts
import pg from "pg";
import { migrate } from "@louvain/pg-rebac";

const client = new pg.Client(process.env.DATABASE_URL);
await client.connect();
await migrate((text) => client.query(text)); // postgres.js: (text) => sql.unsafe(text)
```

`sql` is the same text as a string; `@louvain/pg-rebac/pg_rebac.sql` is the file. You still run the
[`rebacd`](https://github.com/louvain-ai/pg_rebac#readme) daemon (`ghcr.io/louvain-ai/rebacd`) beside Postgres and SpiceDB.

Licence: Apache-2.0. Source: <https://github.com/louvain-ai/pg_rebac>.
