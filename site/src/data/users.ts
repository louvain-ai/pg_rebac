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
    line: "Company memory for AI that only shows each person what they're allowed to see.",
    status: "In production at Louvain: every permission is mirrored into pg_rebac and verified against SpiceDB daily.",
    mark: "louvain",
  },
];
