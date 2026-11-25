#!/usr/bin/env python3
"""Projection scale, measured: storage, fenced query latency and the database side of a sweep, for 0.2.0's layout
(every grant expanded per principal) against 0.3's (grant nodes), at several principal counts.

Needs psql and pgbench on PATH, git (0.2.0's SQL is read from the v0.2.0 tag), Python 3 and a Postgres with pgvector.
Nothing else. Each cell gets its own database, copied from a template that holds the passages and their HNSW index.

    REBAC_BENCH_URL=postgres://postgres:rebac@127.0.0.1:55644 python3 bench.py \\
        --principals 10000,100000 [--layouts v02,v03] [--tx 300] [--warmup 30] [--keep]

The model is in model.sql; the two layouts are load_v02.sql and load_v03.sql; passages.sql builds the template.
Results are printed as a table and written to results-<principals>.json beside this file.
"""
import argparse, glob, json, os, statistics, subprocess, tempfile, time
from urllib.parse import urlparse

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
URL = os.environ.get("REBAC_BENCH_URL", "postgres://postgres:rebac@127.0.0.1:55644")
U = urlparse(URL)
BASE_ENV = dict(os.environ, PGPASSWORD=U.password or "", PGHOST=U.hostname, PGPORT=str(U.port or 5432),
                PGUSER=U.username or "postgres")
TENANT = "tenant-0001"


def env(db, opts=""):
    return dict(BASE_ENV, PGDATABASE=db, PGOPTIONS=opts)


def psql(db, sql=None, files=(), vars=None, opts=""):
    cmd = ["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1"]
    for k, v in (vars or {}).items():
        cmd += ["-v", f"{k}={v}"]
    for f in files:
        cmd += ["-f", f]
    out = subprocess.run(cmd, input=sql, capture_output=True, text=True, env=env(db, opts))
    if out.returncode != 0:
        raise RuntimeError(out.stderr)
    return out.stdout.strip()


def ensure_template():
    if psql("postgres", "select 1 from pg_database where datname = 'bench_base'") != "1":
        psql("postgres", "create database bench_base")
        t = time.time()
        psql("bench_base", files=[os.path.join(HERE, "passages.sql")])
        print(f"template built in {time.time() - t:.0f} s")


def sql_file(layout):
    if layout == "v03":
        return os.path.join(ROOT, "sql", "rebac.sql")
    path = os.path.join(tempfile.gettempdir(), "pg_rebac-v0.2.0.sql")
    with open(path, "w") as f:
        f.write(subprocess.run(["git", "-C", ROOT, "show", "v0.2.0:sql/rebac.sql"], capture_output=True, text=True,
                               check=True).stdout)
    return path


def pct(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(q * (len(xs) - 1))))]


def pgbench(db, script, tx, warmup, opts):
    with tempfile.TemporaryDirectory() as d:
        f = os.path.join(d, "q.sql")
        open(f, "w").write(script)
        out = subprocess.run(["pgbench", "-n", "-c", "1", "-t", str(tx + warmup), "-f", f, "-l", "--log-prefix",
                              os.path.join(d, "log")], capture_output=True, text=True, cwd=d, env=env(db, opts))
        if out.returncode != 0:
            raise RuntimeError(out.stderr + out.stdout)
        lat = []
        for lf in glob.glob(os.path.join(d, "log*")):
            for line in open(lf):
                lat.append(int(line.split()[2]) / 1000.0)
        lat = lat[warmup:]
        return {"p50_ms": round(pct(lat, 0.5), 2), "p95_ms": round(pct(lat, 0.95), 2),
                "mean_ms": round(statistics.mean(lat), 2), "n": len(lat)}


def reader(cls, principals):
    if cls == "broad":
        return f"\\set k random(0, {max(principals // 10000 - 1, 0)})\n\\set i 7 + 10000 * :k\n"
    return f"\\set i random(0, {principals - 1})\n"


def queries(db, principals, tx, warmup):
    base = f"-c rebac.tenant={TENANT} -c jit=off"
    out = {}
    for cls in ("typical", "broad"):
        pick = "\\set qid random(0, 99)\n" + reader(cls, principals)
        p = "md5('p' || :i)::uuid::text"
        q = "(select vec from bench_query where id = :qid)"
        ann = (pick + f"select id from passage where container in (select * from rebac.readable_set({p})) "
               f"order by embedding <=> {q} limit 20;\n")
        exact = (pick + f"select id from passage where container = any(array(select * from rebac.readable_set({p}))) "
                 f"order by embedding <=> {q} limit 20;\n")
        routed = pick + f"select id from rebac.fenced_ids({p}, 'passage', {q}, 20);\n"
        out[cls] = {
            "ann": pgbench(db, ann, tx, warmup, base + " -c hnsw.iterative_scan=relaxed_order"),
            "exact": pgbench(db, exact, tx, warmup, base + " -c hnsw.iterative_scan=off"),
            "fenced_ids": pgbench(db, routed, tx, warmup, base),
        }
        # Recall@20 of the ANN leg against the exact answer over the reader's own passages, and the plan it ran.
        ids = [7 + 10000 * k for k in range(max(principals // 10000, 1))] if cls == "broad" else \
            [(i * 7919) % principals for i in range(20)]
        rec = []
        for n, i in enumerate(ids[:20]):
            qid = (n * 13) % 100
            r = psql(db, f"""
              set hnsw.iterative_scan = relaxed_order;
              with a as (select id from passage where container in (select * from rebac.readable_set(md5('p{i}')::uuid::text))
                         order by embedding <=> (select vec from bench_query where id = {qid}) limit 20),
                   e as (select id from passage where container = any(array(select * from rebac.readable_set(md5('p{i}')::uuid::text)))
                         order by embedding <=> (select vec from bench_query where id = {qid}) limit 20)
              select (select count(*) from a where id in (select id from e))::float / greatest((select count(*) from e), 1)""",
                     opts=base)
            rec.append(float(r.splitlines()[-1]))
        out[cls]["ann"]["recall"] = round(statistics.mean(rec), 3)
        rec = []
        for n, i in enumerate(ids[:20]):
            qid = (n * 13) % 100
            r = psql(db, f"""
              with a as (select id::bigint from rebac.fenced_ids(md5('p{i}')::uuid::text, 'passage',
                                                              (select vec from bench_query where id = {qid}), 20)),
                   e as (select id from passage where container = any(array(select * from rebac.readable_set(md5('p{i}')::uuid::text)))
                         order by embedding <=> (select vec from bench_query where id = {qid}) limit 20)
              select (select count(*) from a where id in (select id from e))::float / greatest((select count(*) from e), 1),
                     (select path from rebac.fenced_ids(md5('p{i}')::uuid::text, 'passage',
                                                        (select vec from bench_query where id = {qid}), 20) limit 1)""",
                     opts=base)
            v, path = r.splitlines()[-1].split("|")
            rec.append(float(v))
        out[cls]["fenced_ids"]["recall"] = round(statistics.mean(rec), 3)
        out[cls]["fenced_ids"]["path"] = path
        plan = psql(db, f"set hnsw.iterative_scan = relaxed_order; explain (costs off) select id from passage where "
                        f"container in (select * from rebac.readable_set(md5('p{ids[0]}')::uuid::text)) order by "
                        f"embedding <=> (select vec from bench_query where id = 0) limit 20", opts=base)
        out[cls]["ann"]["hnsw_outer"] = "passage_embedding_idx" in plan
        out[cls]["breadth"] = int(psql(db, f"select count(distinct c) from rebac.readable_set(md5('p{ids[0]}')::uuid::text) c",
                                       opts=base).splitlines()[-1])
    return out


def sweep_db_side(db, layout, principals, n=200):
    """rebac.project for n principals with the full set SpiceDB would return (unchanged since the last projection):
    what a sweep costs the database per principal, before any SpiceDB call."""
    nodes_arg = ", array(select '#' || node from bench.member where i = x.i)" if layout == "v03" else ""
    r = psql(db, f"""
      create temp table s as
      select i, array(select bench.cid(c) from (select nc.container c from bench.member m join bench.nc using (node)
                                               where m.i = s.i union select bench.direct_of(s.i)) y) arr
      from (select (k * 7919) % {principals} as i from generate_series(0, {n - 1}) k) s;
      do $$ declare x record; t0 timestamptz; begin
        create temp table took (ms float8);
        for x in select * from s loop
          t0 := clock_timestamp();
          perform rebac.lock_principal('{TENANT}', bench.pid(x.i));
          perform rebac.project('{TENANT}', bench.pid(x.i), x.arr, 'tok', 'live', now(){nodes_arg});
          insert into took values (extract(epoch from clock_timestamp() - t0) * 1000);
        end loop; end $$;
      select percentile_cont(0.5) within group (order by ms), percentile_cont(0.95) within group (order by ms),
             avg(ms) from took""")
    p50, p95, avg = (float(x) for x in r.splitlines()[-1].split("|"))
    return {"project_p50_ms": round(p50, 2), "project_p95_ms": round(p95, 2), "project_mean_ms": round(avg, 2)}


def sizes(db):
    has_members = psql(db, "select to_regclass('rebac.node_member') is not null") == "t"
    members = ("pg_total_relation_size('rebac.node_member'), (select count(*) from rebac.node_member)"
               if has_members else "0, 0")
    r = psql(db, f"""
      select (select count(*) from rebac.readable), (select count(*) from rebac.readable where principal like '#%'),
             pg_total_relation_size('rebac.readable'), {members}, pg_total_relation_size('rebac.principal_state')""")
    rows, node_rows, readable_b, member_b, member_rows, state_b = (int(x) for x in r.split("|"))
    return {"readable_rows": rows, "node_rows": node_rows, "member_rows": member_rows,
            "readable_bytes": readable_b, "node_member_bytes": member_b, "principal_state_bytes": state_b,
            "total_bytes": readable_b + member_b + state_b}


def cell(layout, principals, args):
    db = f"bench_{layout}_{principals}"
    psql("postgres", f"drop database if exists {db} with (force)")
    psql("postgres", f"create database {db} template bench_base")
    psql(db, files=[sql_file(layout), os.path.join(HERE, "model.sql")])
    t = time.time()
    psql(db, files=[os.path.join(HERE, f"load_{layout}.sql")], vars={"principals": principals})
    load_s = time.time() - t
    res = {"layout": layout, "principals": principals, "load_s": round(load_s), **sizes(db)}
    if not args.storage_only:
        res["queries"] = queries(db, principals, args.tx, args.warmup)
        res["sweep_db"] = sweep_db_side(db, layout, principals)
    if not args.keep:
        psql("postgres", f"drop database {db} with (force)")
    return res


def gb(b):
    return f"{b / 1e9:.2f} GB" if b >= 1e8 else f"{b / 1e6:.1f} MB"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--principals", default="10000,100000")
    ap.add_argument("--layouts", default="v02,v03")
    ap.add_argument("--tx", type=int, default=300)
    ap.add_argument("--warmup", type=int, default=30)
    ap.add_argument("--storage-only", action="store_true")
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()
    ensure_template()
    for n in (int(x) for x in args.principals.split(",")):
        for layout in args.layouts.split(","):
            r = cell(layout, n, args)
            json.dump(r, open(os.path.join(HERE, f"results-{layout}-{n}.json"), "w"), indent=2)
            line = (f"{layout} principals={n:>8} rows={r['readable_rows']:>11} (nodes {r['node_rows']}) "
                    f"memberships={r['member_rows']} total={gb(r['total_bytes'])} load={r['load_s']} s")
            if "queries" in r:
                for cls, q in r["queries"].items():
                    line += (f"\n    {cls:7} breadth={q['breadth']:>6}  ann p50/p95 {q['ann']['p50_ms']}/{q['ann']['p95_ms']} ms"
                             f" recall {q['ann']['recall']} hnsw-outer={q['ann']['hnsw_outer']}"
                             f"  exact p50/p95 {q['exact']['p50_ms']}/{q['exact']['p95_ms']} ms"
                             f"  fenced_ids ({q['fenced_ids']['path']}) p50/p95 {q['fenced_ids']['p50_ms']}/"
                             f"{q['fenced_ids']['p95_ms']} ms recall {q['fenced_ids']['recall']}")
                s = r["sweep_db"]
                line += f"\n    sweep, database side: rebac.project p50/p95 {s['project_p50_ms']}/{s['project_p95_ms']} ms"
            print(line, flush=True)


if __name__ == "__main__":
    main()
