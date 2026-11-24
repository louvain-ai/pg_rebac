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
    line: "Company memory for AI that only shows each person what they're allowed to see. pg_rebac is being proven there as its permission fence.",
    status: "Running in shadow in production: Louvain's permissions are mirrored into it and compared daily with a projection rebuilt from SpiceDB. No reads are served from it yet.",
    mark: "louvain",
  },
];
