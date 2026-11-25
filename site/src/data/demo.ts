// The quickstart's first result, as it prints it (public/quickstart.yml): one vector search, top 5 nearest to
// "salary pay review", run as two people. The cover's two-readers figure draws these rows.
export const quickstart = "curl -fsSL https://pgrebac.com/quickstart.yml | docker compose -f - run --rm demo";
/** The same command broken after the pipe, where a shell continues the line, so it reads in a narrow column. */
export const quickstartLines = "curl -fsSL https://pgrebac.com/quickstart.yml |\n  docker compose -f - run --rm demo";

export interface Reader {
  name: string;
  reads: string[];
  rows: { title: string; container: string }[];
}

export const readers: Reader[] = [
  {
    name: "alice",
    reads: ["handbook", "payroll"],
    rows: [
      { title: "Annual pay review", container: "handbook" },
      { title: "Salary bands 2026", container: "payroll" },
      { title: "Pay days", container: "handbook" },
      { title: "Payroll cut-off", container: "payroll" },
      { title: "Overtime pay", container: "payroll" },
    ],
  },
  {
    name: "bob",
    reads: ["handbook", "deals"],
    rows: [
      { title: "Annual pay review", container: "handbook" },
      { title: "Pay days", container: "handbook" },
      { title: "Commission plan", container: "deals" },
      { title: "Reseller pay terms", container: "deals" },
      { title: "Expenses", container: "handbook" },
    ],
  },
];
