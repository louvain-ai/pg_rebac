#!/usr/bin/env python3
"""A more realistic study of fenced pgvector search: percentile latency, recall and concurrency.

Needs a database built by setup.sql, psql and pgbench on PATH, and Python 3 (nothing else). Latency is measured by
pgbench (one transaction = one fenced query, per-transaction log); recall and rows/k are computed inside Postgres
against the exact answer over the reader's own passages.

    REBAC_BENCH_URL=postgres://postgres:rebac@127.0.0.1:55444/rebac_bench_real python3 bench.py \
        [--k 20] [--tx 320] [--warmup 20] [--truth-queries 200] [--seconds 45] [--skip-concurrency]

Per cell (reader x mode) it runs --tx single-client queries (the first --warmup are dropped) drawn at random from the
stored query vectors, and recall over the first --truth-queries of them. The concurrency run drives 1, 8 and 32
clients over a mix of readers for --seconds each.
"""
import argparse, glob, os, re, statistics, subprocess, tempfile, concurrent.futures as cf
from urllib.parse import urlparse

URL = os.environ.get("REBAC_BENCH_URL", "postgres://postgres:rebac@127.0.0.1:55444/rebac_bench_real")
U = urlparse(URL)
ENV = dict(os.environ, PGPASSWORD=U.password or "", PGHOST=U.hostname, PGPORT=str(U.port or 5432),
           PGUSER=U.username or "postgres", PGDATABASE=U.path.lstrip("/"))
NAMED = ["b0001", "b001", "b005", "b010", "b020", "b050"]


def psql(sql: str, env=None) -> str:
    out = subprocess.run(["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1"], input=sql, capture_output=True, text=True,
                         env=env or ENV)
    if out.returncode != 0:
        raise RuntimeError(out.stderr)
    return out.stdout


def fenced(p, k):
    return (f"select id from passage where tenant = 'acme' and container in (select * from rebac.readable_set('{p}')) "
            f"order by embedding <=> (select vec from bench_query where id = :qid) limit {k}")


def exact(p, k):
    return (f"select id from passage where tenant = 'acme' and container = any(array(select * from "
            f"rebac.readable_set('{p}'))) order by embedding <=> (select vec from bench_query where id = :qid) limit {k}")


# mode -> (GUCs, query builder). Every session runs with rebac.tenant = 'acme' and jit off.
MODES = {
    "ann, iterative off": ("hnsw.iterative_scan=off", fenced),
    "ann, relaxed_order": ("hnsw.iterative_scan=relaxed_order", fenced),
    "ann, relaxed, ef 200": ("hnsw.iterative_scan=relaxed_order -c hnsw.ef_search=200", fenced),
    "exact (array)": ("hnsw.iterative_scan=off", exact),
}


def pgopts(gucs: str) -> str:
    return f"-c rebac.tenant=acme -c jit=off -c {gucs}"


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(q * (len(xs) - 1))))]


def pgbench(scripts, clients, tx=None, seconds=None, gucs="hnsw.iterative_scan=relaxed_order", warmup=0):
    """Run pgbench with weighted scripts [(sql, weight)], return (latencies_ms, elapsed_s)."""
    with tempfile.TemporaryDirectory() as d:
        args = ["pgbench", "-n", "-M", "simple", "-c", str(clients), "-j", str(min(clients, 8)), "-l",
                f"--log-prefix={d}/log"]
        args += ["-t", str(tx)] if tx else ["-T", str(seconds)]
        for i, (sql, w) in enumerate(scripts):
            path = f"{d}/s{i}.sql"
            open(path, "w").write("\\set qid random(1, :nq)\n" + sql + ";\n")
            args += ["-f", f"{path}@{w}"]
        nq = psql("select count(*) from bench_query").strip()
        args += ["-D", f"nq={nq}"]
        out = subprocess.run(args, capture_output=True, text=True, env=dict(ENV, PGOPTIONS=pgopts(gucs)))
        if out.returncode != 0:
            raise RuntimeError(out.stderr)
        rows = []
        for f in glob.glob(f"{d}/log*"):
            lines = [l.split() for l in open(f)]
            rows += [(int(l[4]) * 1_000_000 + int(l[5]), int(l[2]) / 1000.0) for l in lines[warmup:]]
        lat = [r[1] for r in rows]
        elapsed = (max(r[0] for r in rows) - min(r[0] for r in rows)) / 1_000_000 if rows else 0
        m = re.search(r"number of failed transactions: (\d+)", out.stdout)
        if m and int(m.group(1)):
            print("  WARNING: failed transactions:", m.group(1))
        return lat, elapsed


def plan_of(p, mode):
    gucs, make = MODES[mode]
    q = make(p, 20).replace(":qid", "1")
    out = psql(f"set rebac.tenant='acme'; set jit=off; set {gucs.replace(' -c ', '; set ').replace('=', ' = ', 1)}; "
               f"explain {q};")
    return "HNSW" if "passage_embedding_idx" in out else "exact"


def ef_study(k, tq):
    """Recall@k of ANN against hnsw.ef_search, unfenced and fenced (relaxed_order), over tq queries. The unfenced
    baseline separates what HNSW alone loses from what the fence adds."""
    # Exact: the index scan is switched off, so this is a (parallel) sequential scan and an exact sort per query.
    psql("set enable_indexscan = off; set enable_bitmapscan = off; set jit = off; "
         "drop table if exists bench_truth_all; create table bench_truth_all as select q.id as qid, array(select id "
         f"from passage order by embedding <=> q.vec limit {k}) as ids from bench_query q where q.id <= {tq}")
    print("# recall@%d vs hnsw.ef_search (%d queries)" % (k, tq))
    print(f"{'reader':9} {'ef':>5} {'p50 ms':>8} {'rows/k':>7} {'recall':>7}")
    for ef in (40, 100, 200, 400, 800):
        out = psql(f"load 'vector'; set jit=off; set hnsw.iterative_scan=off; set hnsw.ef_search={ef}; "
                   f"select avg(cardinality(a))::float8 / {k}, avg((select count(*) from unnest(a) x where x = any(t.ids))"
                   f"::float8 / {k}) from (select q.id, array(select id from passage order by embedding <=> q.vec "
                   f"limit {k}) as a from bench_query q where q.id <= {tq}) r join bench_truth_all t on t.qid = r.id")
        lat, _ = pgbench([("select id from passage order by embedding <=> (select vec from bench_query where id = :qid) "
                           f"limit {k}", 1)], 1, tx=120, gucs=f"hnsw.iterative_scan=off -c hnsw.ef_search={ef}", warmup=20)
        rk, rec = map(float, out.strip().split("|"))
        print(f"{'unfenced':9} {ef:5d} {pct(lat, .5):8.1f} {rk:7.2f} {rec:7.2f}", flush=True)
    for p in ("b010", "b050"):
        for ef in (40, 200, 800):
            gucs = f"hnsw.iterative_scan=relaxed_order -c hnsw.ef_search={ef}"
            lat, _ = pgbench([(fenced(p, k), 1)], 1, tx=120, gucs=gucs, warmup=20)
            sql = "array(" + fenced(p, k).replace("(select vec from bench_query where id = :qid)", "q.vec") + ")"
            out = psql(f"load 'vector'; set rebac.tenant='acme'; set jit=off; set hnsw.iterative_scan=relaxed_order; "
                       f"set hnsw.ef_search={ef}; select avg(cardinality(a))::float8 / {k}, avg((select count(*) from "
                       f"unnest(a) x where x = any(t.ids))::float8 / greatest(1, cardinality(t.ids))) from (select q.id, "
                       f"{sql} as a from bench_query q where q.id <= {tq}) r join bench_truth t on t.reader = '{p}' "
                       f"and t.qid = r.id")
            rk, rec = map(float, out.strip().split("|"))
            print(f"{p:9} {ef:5d} {pct(lat, .5):8.1f} {rk:7.2f} {rec:7.2f}", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--k", type=int, default=20)
    ap.add_argument("--tx", type=int, default=320)
    ap.add_argument("--warmup", type=int, default=20)
    ap.add_argument("--truth-queries", type=int, default=200)
    ap.add_argument("--seconds", type=int, default=45)
    ap.add_argument("--ef-study", action="store_true", help="only: recall of unfenced and fenced ANN against hnsw.ef_search")
    ap.add_argument("--retruth", action="store_true")
    ap.add_argument("--skip-concurrency", action="store_true")
    a = ap.parse_args()
    k, tq = a.k, a.truth_queries

    print("# settings")
    for s in ["server_version", "shared_buffers", "work_mem", "effective_cache_size", "max_worker_processes",
              "max_parallel_workers_per_gather", "random_page_cost", "hnsw.ef_search", "hnsw.max_scan_tuples", "jit"]:
        print(f"{s} = {psql(f"load 'vector'; show {s}").strip()}")
    print("vector", psql("select extversion from pg_extension where extname='vector'").strip())
    total_p = int(psql("select count(*) from passage"))
    total_c = int(psql("select count(distinct container) from passage"))
    print(f"passages={total_p} containers={total_c} dims={psql('select vector_dims(embedding) from passage limit 1').strip()} "
          f"readable_rows={psql('select count(*) from rebac.readable').strip()} "
          f"readers={psql('select count(*) from rebac.principal_state').strip()}")
    print("passage heap+toast+indexes:", psql("select pg_size_pretty(pg_table_size('passage')) || ' table, ' || "
          "pg_size_pretty(pg_relation_size('passage_embedding_idx')) || ' hnsw'").strip())

    reach = {}
    for p in NAMED:
        n = int(psql(f"select n_containers from rebac.principal_state where principal='{p}'"))
        r = int(psql(f"select count(*) from passage where container in (select container from rebac.readable "
                     f"where tenant='acme' and principal='{p}')"))
        reach[p] = (n, r)

    if a.ef_study:
        return ef_study(k, tq)
    # Ground truth: the exact top-k over each reader's own passages, for the first tq queries, in parallel.
    # Truth is kept between runs (it depends only on the data); pass --retruth to recompute.
    def truth(p):
        psql(f"set rebac.tenant='acme'; set jit=off; insert into bench_truth select '{p}', q.id, array(select id from "
             f"passage where tenant='acme' and container = any(array(select * from rebac.readable_set('{p}'))) "
             f"order by embedding <=> q.vec limit {k}) from bench_query q where q.id <= {tq}")
    have = 0
    if psql("select to_regclass('bench_truth') is not null").strip() == "t":
        have = int(psql("select count(*) from bench_truth").strip())
    if a.retruth or have != len(NAMED) * tq:
        psql("drop table if exists bench_truth; create table bench_truth (reader text, qid int, ids bigint[], "
             "primary key (reader, qid))")
        with cf.ThreadPoolExecutor(len(NAMED)) as ex:
            list(ex.map(truth, NAMED))

    def recall(p, gucs, make):
        sql = make(p, k).replace("(select vec from bench_query where id = :qid)", "q.vec")
        sql = "array(" + sql + ")"
        out = psql(f"set rebac.tenant='acme'; set jit=off; set {gucs.replace(' -c ', '; set ').replace('=', ' = ', 1)}; "
                   f"select avg(cardinality(a))::float8 / {k}, avg((select count(*) from unnest(a) x where x = any(t.ids))"
                   f"::float8 / greatest(1, cardinality(t.ids))) from (select q.id, {sql} as a from bench_query q "
                   f"where q.id <= {tq}) r join bench_truth t on t.reader = '{p}' and t.qid = r.id")
        rows_k, rec = out.strip().split("|")
        return float(rows_k), float(rec)

    print("\n# single client, warm cache: latency over", a.tx - a.warmup, "queries per cell; rows/k and recall@%d over %d queries" % (k, tq))
    print(f"{'reader':7} {'breadth':>8} {'containers':>10} {'passages':>9} | {'mode':21} {'plan':6} {'p50 ms':>8} {'p95 ms':>8} "
          f"{'p99 ms':>8} {'rows/k':>7} {'recall':>7}")
    for p in NAMED:
        n, r = reach[p]
        for mode, (gucs, make) in MODES.items():
            lat, _ = pgbench([(make(p, k), 1)], 1, tx=a.tx, gucs=gucs, warmup=a.warmup)
            rk, rec = (1.0, 1.0) if mode.startswith("exact") else recall(p, gucs, make)
            print(f"{p:7} {r / total_p:8.1%} {n:10d} {r:9d} | {mode:21} {plan_of(p, mode):6} {pct(lat, .5):8.1f} "
                  f"{pct(lat, .95):8.1f} {pct(lat, .99):8.1f} {rk:7.2f} {rec:7.2f}", flush=True)

    base, _ = pgbench([("select id from passage order by embedding <=> (select vec from bench_query where id = :qid) "
                        f"limit {k}", 1)], 1, tx=a.tx, gucs="hnsw.iterative_scan=off", warmup=a.warmup)
    print(f"unfenced HNSW baseline: p50 {pct(base, .5):.1f} p95 {pct(base, .95):.1f} p99 {pct(base, .99):.1f} ms")

    if a.skip_concurrency:
        return
    # Background readers across the breadth range (by passages reached), for the concurrency mixes.
    bg = psql("select principal from rebac.principal_state where principal like 'bg%' and n_containers > 0 "
              "order by n_containers").split()
    picks = [bg[int(i * (len(bg) - 1) / 11)] for i in range(12)]
    for p in picks:
        r = int(psql(f"select count(*) from passage where container in (select container from rebac.readable "
                     f"where tenant='acme' and principal='{p}')"))
        reach[p] = (0, r)
    ROUTE = 0.01 * total_p  # exact below 1% of the passages (about where ANN recall and cost cross over)

    def mix(names, routed):
        out = []
        for p in names:
            use_exact = reach[p][1] < ROUTE
            if use_exact and not routed:
                continue
            out.append((exact(p, k) if use_exact else fenced(p, k), 1))
        return out

    print(f"\n# concurrency: pgbench, {a.seconds}s per level, every client a fenced query for a random reader of the mix")
    print("mix 'ann': relaxed_order ANN for the named readers 1%..50% (b001..b050) and the background readers of 1%+")
    print("mix 'routed': all 6 named + 12 background readers; readers under 1% of passages take the exact path")
    print(f"{'mix':7} {'clients':>7} | {'qps':>8} {'p50 ms':>8} {'p95 ms':>8} {'p99 ms':>8} {'queries':>8}")
    for name, scripts in (("ann", mix(NAMED[1:] + picks, False)), ("routed", mix(NAMED + picks, True))):
        for c in (1, 8, 32):
            # relaxed_order for the ANN legs; the exact legs ignore the setting.
            lat, el = pgbench(scripts, c, seconds=a.seconds, gucs="hnsw.iterative_scan=relaxed_order")
            print(f"{name:7} {c:7d} | {len(lat) / max(el, 1e-9):8.1f} {pct(lat, .5):8.1f} {pct(lat, .95):8.1f} "
                  f"{pct(lat, .99):8.1f} {len(lat):8d}", flush=True)
    print("mix members (passages reached):", ", ".join(f"{p}={reach[p][1]}" for p in NAMED + picks))


if __name__ == "__main__":
    main()
