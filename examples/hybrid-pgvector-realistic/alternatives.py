#!/usr/bin/env python3
"""The two alternatives people ask about, measured on the same data as bench.py (a database built by setup.sql).

    REBAC_BENCH_URL=postgres://postgres:rebac@127.0.0.1:55447/rebac_bench_real python3 alternatives.py [--rls] [--overfetch]

--rls        Row-level security. The same fenced reads, with the fence in a policy instead of the query, four ways:
               query fence   no RLS; `container in (select * from rebac.readable_set(p))` in the query (bench.py)
               rls, set      rebac.enable_rls: a restrictive policy on the same set (principal from a setting)
               rls, per row  a policy calling rebac.readable_by(p, container): pg_rebac's own per-row check,
                             PL/pgSQL, which re-checks the principal (liveness, freshness) on every row
               rls, exists   a policy with the naive inline check: exists (select 1 from rebac.readable r where
                             r.tenant = .. and r.principal = .. and r.container = passage.container)
             Two query shapes: ANN top-20 (hnsw.iterative_scan = relaxed_order) and count(*) (every readable row).
             Reads run as a non-superuser role (`rebac_bench_reader`, created here) because a superuser and a table's
             owner bypass RLS. The policies are dropped again at the end.
--overfetch  Filtering after retrieval: fetch the unfenced top N (N = 20 .. 4000), keep what the reader may view,
             return the first 20. Reports latency and rows/k per reader and N, beside the fenced query.

Latency is pgbench (one transaction = one query, per-transaction log, the first --warmup dropped). Needs psql and
pgbench on PATH and Python 3.12 or later (no packages).
"""
import argparse, glob, os, statistics, subprocess, tempfile
from urllib.parse import urlparse

URL = os.environ.get("REBAC_BENCH_URL", "postgres://postgres:rebac@127.0.0.1:55444/rebac_bench_real")
U = urlparse(URL)
ENV = dict(os.environ, PGPASSWORD=U.password or "", PGHOST=U.hostname, PGPORT=str(U.port or 5432),
           PGUSER=U.username or "postgres", PGDATABASE=U.path.lstrip("/"))
READER = dict(ENV, PGUSER="rebac_bench_reader", PGPASSWORD="rebac_bench_reader")
NAMED = ["b0001", "b001", "b005", "b010", "b020", "b050"]
K = 20
Q = "(select vec from bench_query where id = :qid)"


def psql(sql, env=ENV):
    out = subprocess.run(["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1"], input=sql, capture_output=True, text=True,
                         env=env)
    if out.returncode != 0:
        raise RuntimeError(out.stderr)
    return out.stdout


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(q * (len(xs) - 1))))]


def pgbench(sql, env, opts, tx, warmup):
    """One client, `tx` transactions of `sql`; latencies in ms with the first `warmup` dropped."""
    with tempfile.TemporaryDirectory() as d:
        open(f"{d}/s.sql", "w").write("\\set qid random(1, :nq)\n" + sql + "\n")
        nq = psql("select count(*) from bench_query").strip()
        args = ["pgbench", "-n", "-M", "simple", "-c", "1", "-t", str(tx), "-l", f"--log-prefix={d}/log",
                "-f", f"{d}/s.sql", "-D", f"nq={nq}"]
        out = subprocess.run(args, capture_output=True, text=True, env=dict(env, PGOPTIONS=opts))
        if out.returncode != 0:
            raise RuntimeError(out.stderr)
        lat = []
        for f in glob.glob(f"{d}/log*"):
            lat += [int(l.split()[2]) / 1000.0 for l in list(open(f))[warmup:]]
        return lat


def opts(principal, gucs=""):
    return f"-c rebac.tenant=acme -c rebac.principal={principal} -c jit=off {gucs}".strip()


def reps(sql, env, o, budget_s, lo, hi):
    """How many repetitions fit the time budget, from three probes (slow per-row cells get fewer)."""
    probe = pgbench(sql, env, o, 3, 0)
    return max(lo, min(hi, int(budget_s * 1000 / max(statistics.median(probe), 0.01))))


# --------------------------------------------------------------------------------------------------------- RLS ---

POLICIES = {
    "query fence": None,
    "rls, set": "select rebac.enable_rls('passage', 'container', force => true)",
    "rls, per row": "create policy rebac_bench on passage as permissive for select using "
                    "(rebac.readable_by(current_setting('rebac.principal'), container))",
    "rls, exists": "create policy rebac_bench on passage as permissive for select using (exists (select 1 from "
                   "rebac.readable r where r.tenant = current_setting('rebac.tenant') and r.principal = "
                   "current_setting('rebac.principal') and r.container = passage.container))",
}


def clear_policies():
    psql("drop policy if exists rebac_read on passage; drop policy if exists rebac_base on passage; "
         "drop policy if exists rebac_bench on passage; alter table passage no force row level security; "
         "alter table passage disable row level security;")


def rls(a):
    psql("do $$ begin if not exists (select 1 from pg_roles where rolname = 'rebac_bench_reader') then "
         "create role rebac_bench_reader login password 'rebac_bench_reader' nosuperuser nobypassrls; end if; end $$;"
         "grant select on passage, bench_query to rebac_bench_reader; grant usage on schema rebac to rebac_bench_reader;"
         "grant select on all tables in schema rebac to rebac_bench_reader;"
         "grant execute on all functions in schema rebac to rebac_bench_reader;")
    print(f"# RLS against a fence in the query: single client, warm cache, p50/p99 over the timed queries (n)")
    print(f"{'reader':7} {'passages':>9} | {'policy':13} {'shape':6} {'n':>4} {'p50 ms':>9} {'p99 ms':>9} {'rows':>8}")
    try:
        for name, ddl in POLICIES.items():
            clear_policies()
            if ddl:
                psql("alter table passage enable row level security;" + ddl)
            for p in NAMED:
                fence = "" if ddl else " and container in (select * from rebac.readable_set(current_setting('rebac.principal')))"
                ann = f"select id from passage where tenant = 'acme'{fence} order by embedding <=> {Q} limit {K};"
                cnt = f"select count(*) from passage where tenant = 'acme'{fence};"
                for shape, sql, gucs in (("ann", ann, "-c hnsw.iterative_scan=relaxed_order"), ("count", cnt, "")):
                    o = opts(p, gucs)
                    n = reps(sql, READER, o, a.budget, 5, a.tx)
                    lat = pgbench(sql, READER, o, n + a.warmup, a.warmup)
                    got = psql(f"set rebac.tenant='acme'; set rebac.principal='{p}'; set jit=off; "
                               f"set hnsw.iterative_scan=relaxed_order; "
                               + (f"select count(*) from ({sql.rstrip(';').replace(':qid', '1')}) x;"
                                  if shape == "ann" else sql), READER).strip()
                    total = psql(f"select count(*) from passage where container in (select container from "
                                 f"rebac.readable where tenant='acme' and principal='{p}')").strip()
                    print(f"{p:7} {total:>9} | {name:13} {shape:6} {len(lat):4d} {pct(lat, .5):9.1f} "
                          f"{pct(lat, .99):9.1f} {got:>8}", flush=True)
    finally:
        clear_policies()


# --------------------------------------------------------------------------------------------------- over-fetch ---

def overfetch(a):
    # `order by d + 0`, not `order by d`: the inner rows already arrive in distance order, so `order by d` would let
    # the outer limit stop pulling at the 20th readable row, which is an in-database iterative scan, not an
    # application fetching N rows and filtering them. The extra sort makes every one of the N rows be fetched.
    print(f"# Filtering after retrieval: unfenced HNSW top N (hnsw.ef_search = min(N, 1000), iterative_scan = "
          f"strict_order), then keep the reader's rows, limit {K}. rows/k over {a.truth} queries.")
    print(f"{'reader':7} {'N':>5} | {'p50 ms':>8} {'p99 ms':>8} {'rows/k':>7}")
    for p in NAMED:
        for n in (20, 40, 100, 200, 400, 1000, 2000, 4000):
            inner = f"select id, container, embedding <=> {Q} as d from passage order by embedding <=> {Q} limit {n}"
            sql = (f"select id from ({inner}) c where container in (select container from rebac.readable "
                   f"where tenant = 'acme' and principal = '{p}') order by d + 0 limit {K};")
            gucs = f"-c hnsw.ef_search={min(n, 1000)} -c hnsw.iterative_scan=strict_order"
            lat = pgbench(sql, ENV, opts(p, gucs), a.tx + a.warmup, a.warmup)
            rk = psql(f"load 'vector'; set jit=off; set hnsw.ef_search={min(n, 1000)}; "
                      f"set hnsw.iterative_scan=strict_order; select avg(c)::float8 / {K} from (select q.id, (select "
                      f"count(*) from ({sql.rstrip(';').replace(Q, '(select vec from bench_query b where b.id = q.id)')})"
                      f" x) c from bench_query q where q.id <= {a.truth}) s").strip()
            print(f"{p:7} {n:5d} | {pct(lat, .5):8.1f} {pct(lat, .99):8.1f} {float(rk):7.2f}", flush=True)
        fenced = (f"select id from passage where tenant = 'acme' and container in (select * from "
                  f"rebac.readable_set('{p}')) order by embedding <=> {Q} limit {K};")
        lat = pgbench(fenced, ENV, opts(p, "-c hnsw.iterative_scan=relaxed_order"), a.tx + a.warmup, a.warmup)
        print(f"{p:7} {'fence':>5} | {pct(lat, .5):8.1f} {pct(lat, .99):8.1f} {fence_rows(p, a.truth):7.2f}  "
              "(relaxed_order)", flush=True)


def fence_rows(p, truth):
    """rows/k of the fenced query with relaxed_order, over the first `truth` queries."""
    sql = (f"select id from passage where tenant = 'acme' and container in (select * from rebac.readable_set('{p}')) "
           f"order by embedding <=> q.vec limit {K}")
    return float(psql(f"set rebac.tenant = 'acme'; set jit = off; set hnsw.iterative_scan = relaxed_order; "
                      f"select avg((select count(*) from ({sql}) x))::float8 / {K} from bench_query q "
                      f"where q.id <= {truth}").strip())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rls", action="store_true")
    ap.add_argument("--overfetch", action="store_true")
    ap.add_argument("--tx", type=int, default=200, help="timed queries per cell, at most")
    ap.add_argument("--warmup", type=int, default=10)
    ap.add_argument("--budget", type=float, default=30, help="seconds per RLS cell; slow cells get fewer queries")
    ap.add_argument("--truth", type=int, default=100, help="queries for rows/k in the over-fetch study")
    a = ap.parse_args()
    print("server", psql("show server_version").strip(), "| vector",
          psql("select extversion from pg_extension where extname = 'vector'").strip(),
          "| passages", psql("select count(*) from passage").strip(),
          "| readable rows", psql("select count(*) from rebac.readable").strip())
    if a.rls:
        rls(a)
    if a.overfetch:
        overfetch(a)


if __name__ == "__main__":
    main()
