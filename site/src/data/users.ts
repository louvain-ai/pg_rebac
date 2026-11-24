// Who runs pg_rebac. Add an entry when another project or company does; `status` says how far, honestly.
export interface User {
  name: string;
  href: string;
  /** One line on what it is and what pg_rebac does for it. */
  line: string;
  /** How it is used today: never stronger than what is true. */
  status: string;
  mark: "louvain";
}

export const users: User[] = [
  {
    name: "Louvain",
    href: "https://louvain.ai",
    line: "pg_rebac is the permission layer behind Louvain, company memory for AI that only shows each person what they're allowed to see.",
    status: "Running in shadow: projected and compared against SpiceDB, with no reads switched to it yet.",
    mark: "louvain",
  },
];
