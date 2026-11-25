import { readFileSync } from "node:fs";

/** The pg_rebac SQL (schema `rebac`), as shipped in this package. */
export const sql: string = readFileSync(new URL("../sql/pg_rebac.sql", import.meta.url), "utf8");

/**
 * Install pg_rebac by handing the SQL to any client's simple-query function (no extended protocol: the file holds
 * many statements). No driver is bundled.
 *
 *   await migrate((t) => client.query(t));      // pg
 *   await migrate((t) => sql.unsafe(t));        // postgres.js
 */
export async function migrate(run: (text: string) => Promise<unknown>): Promise<void> {
  await run(sql);
}
