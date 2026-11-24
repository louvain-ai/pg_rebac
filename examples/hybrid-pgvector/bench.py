#!/usr/bin/env python3
"""The planner study: a fenced HNSW ordering at every reader breadth.

For each reader and each mode it reports the plan the planner chose, the median latency, how many of the k rows came
back, and recall@k against the exact answer over the reader's own passages. Drives psql, so it needs nothing but
Python 3 and a database built by setup.sql.

    REBAC_BENCH_URL=postgres://... python3 bench.py [--k 20] [--queries 10]
"""
import argparse, json, os, statistics, subprocess

URL = os.environ.get("REBAC_BENCH_URL", "postgres://postgres:rebac@127.0.0.1:55433/rebac_bench")
READERS = ["b0001", "b001", "b005", "b010", "b020", "b030", "b050", "b100"]


def psql(sql: str) -> str:
    out = subprocess.run(["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1", URL], input=sql, capture_output=True, text=True)
    if out.returncode != 0:
        raise RuntimeError(out.stderr)
    return out.stdout


def fenced(p: str, q: str, k: int) -> str:
    return (f"select id from passage where tenant = 'acme' and container in (select * from rebac.readable_set('{p}')) "
            f"order by embedding <=> '{q}' limit {k}")


def exact(p: str, q: str, k: int) -> str:
    # The exact path: the reader's containers as one array (an InitPlan), a bitmap scan of passage(tenant, container),
    # and an exact sort. The array form keeps the planner from misjudging the reader's breadth.
    return (f"select id from passage where tenant = 'acme' and container = any(array(select * from "
            f"rebac.readable_set('{p}'))) order by embedding <=> '{q}' limit {k}")


def exact_in(p: str, q: str, k: int) -> str:
    return (f"with s as materialized (select id, embedding from passage where tenant = 'acme' and container in "
            f"(select * from rebac.readable_set('{p}'))) select id from s order by embedding <=> '{q}' limit {k}")


# Route by breadth: what an application (or a future helper) does with rebac.breadth().
EXACT_BELOW_CONTAINERS = int(os.environ.get("REBAC_EXACT_BELOW", "50"))


def auto(p: str, q: str, k: int) -> str:
    n = int(psql(f"set rebac.tenant = 'acme'; select rebac.breadth('{p}')").strip().split()[-1])
    return exact(p, q, k) if n < EXACT_BELOW_CONTAINERS else fenced(p, q, k)


MODES = {
    "ann, iterative off": ("set hnsw.iterative_scan = off;", fenced),
    "ann, relaxed_order": ("set hnsw.iterative_scan = relaxed_order;", fenced),
    "ann, relaxed, ef 200": ("set hnsw.iterative_scan = relaxed_order; set hnsw.ef_search = 200;", fenced),
    "exact (in, CTE)": ("", exact_in),
    "exact (array)": ("", exact),
    "auto (by breadth)": ("set hnsw.iterative_scan = relaxed_order;", auto),
}


def plan_summary(node, acc):
    name = node.get("Node Type", "")
    if node.get("Index Name"):
        name += f" [{node['Index Name']}]"
    acc.append(name)
    for child in node.get("Plans", []):
        plan_summary(child, acc)
    return acc


def run(prelude: str, query: str):
    sql = f"set rebac.tenant = 'acme'; set jit = off; {prelude}\nexplain (analyze, format json) {query};\n{query};"
    out = psql(sql).strip().split("\n")
    # psql -A prints the JSON plan as one block, then the ids one per line.
    text = "\n".join(out)
    end = text.rindex("]") + 1
    start = text.index("[")
    plan = json.loads(text[start:end])[0]
    ids = [int(x) for x in text[end:].split() if x.strip()]
    nodes = plan_summary(plan["Plan"], [])
    uses_hnsw = any("passage_embedding_idx" in n for n in nodes)
    return plan["Execution Time"], ids, uses_hnsw, nodes


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--k", type=int, default=20)
    ap.add_argument("--queries", type=int, default=10)
    args = ap.parse_args()
    dims = int(psql("select vector_dims(embedding) from passage limit 1").strip())
    # A query is near a topic, as a real question is near what it asks about.
    queries = [psql(f"select (select array_agg(c + (random() - 0.5) * 0.3) from unnest(centre) c)::vector::text "
                    f"from topic order by random() limit 1").strip() for _ in range(args.queries)]
    total = int(psql("select count(distinct container) from passage").strip())
    print(f"passages={psql('select count(*) from passage').strip()} containers={total} k={args.k} queries={args.queries}")
    print(f"{'reader':8} {'breadth':>8} {'passages':>9} | {'mode':20} {'plan':34} {'p50 ms':>8} {'max ms':>8} {'rows/k':>7} {'recall':>7}")
    for p in READERS:
        n = int(psql(f"select count(*) from rebac.readable where tenant = 'acme' and principal = '{p}'").strip())
        reach = int(psql(f"select count(*) from passage where container in (select container from rebac.readable where tenant='acme' and principal='{p}')").strip())
        truth = {}
        for q in queries:
            truth[q] = run("", exact(p, q, args.k))[1]
        for mode, (prelude, make) in MODES.items():
            times, rows, recalls, hnsw, shape = [], [], [], 0, None
            for q in queries:
                ms, ids, uses, nodes = run(prelude, make(p, q, args.k))
                times.append(ms)
                rows.append(len(ids))
                want = set(truth[q])
                recalls.append(len(want & set(ids)) / max(1, len(want)))
                hnsw += uses
                shape = " > ".join(x for x in nodes if x not in ("Limit", "Result"))[:34]
            plan = ("HNSW " if hnsw == len(queries) else ("mixed " if hnsw else "")) + (shape or "")
            print(f"{p:8} {n / total:8.1%} {reach:9d} | {mode:20} {plan[:34]:34} {statistics.median(times):8.1f} "
                  f"{max(times):8.1f} {statistics.mean(rows) / args.k:7.2f} {statistics.mean(recalls):7.2f}")
    # The unfenced baseline, for scale.
    base = [run("set hnsw.iterative_scan = off;", f"select id from passage order by embedding <=> '{q}' limit {args.k}")[0]
            for q in queries]
    print(f"unfenced HNSW baseline p50 {statistics.median(base):.1f} ms")


if __name__ == "__main__":
    main()
