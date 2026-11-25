#!/usr/bin/env python3
"""The tables in docs/design/projection-scale.md, from the results-<layout>-<principals>.json files bench.py writes."""
import glob, json, os

HERE = os.path.dirname(os.path.abspath(__file__))
NAMES = {"v02": "0.2", "v03": "0.3"}


def ms(x):
    return f"{x:.1f}"


def size(b):
    return f"{b / 1e9:.1f} GB" if b >= 1e9 else f"{b / 1e6:.0f} MB"


def main():
    runs = sorted((json.load(open(f)) for f in glob.glob(os.path.join(HERE, "results-*.json"))),
                  key=lambda r: (r["principals"], r["layout"]))
    print("| Principals | Layout | Rows in `rebac.readable` | of them grant nodes' | Memberships | Size (tables and indexes) | Per principal |")
    print("|---|---|---|---|---|---|---|")
    for r in runs:
        print(f"| {r['principals']:,} | {NAMES[r['layout']]} | {r['readable_rows']:,} | {r['node_rows']:,} | "
              f"{r['member_rows']:,} | {size(r['total_bytes'])} | {r['total_bytes'] / r['principals'] / 1e3:.1f} kB |")
    print()
    print("| Principals | Layout | Reader | Breadth | `in`, relaxed ANN p50 / p95 | `= any`, exact p50 / p95 | "
          "`rebac.fenced_ids` p50 / p95 (path, recall) |")
    print("|---|---|---|---|---|---|---|")
    for r in runs:
        for cls, q in r.get("queries", {}).items():
            a, e, f = q["ann"], q["exact"], q["fenced_ids"]
            plan = "" if a["hnsw_outer"] else " (exact plan)"
            print(f"| {r['principals']:,} | {NAMES[r['layout']]} | {cls} | {q['breadth']:,} | "
                  f"{ms(a['p50_ms'])} / {ms(a['p95_ms'])} ms{plan} | {ms(e['p50_ms'])} / {ms(e['p95_ms'])} ms | "
                  f"{ms(f['p50_ms'])} / {ms(f['p95_ms'])} ms ({f['path']}, {f['recall']}) |")
    print()
    print("| Principals | Layout | `rebac.project`, unchanged set, p50 / p95 |")
    print("|---|---|---|")
    for r in runs:
        if "sweep_db" in r:
            s = r["sweep_db"]
            print(f"| {r['principals']:,} | {NAMES[r['layout']]} | {ms(s['project_p50_ms'])} / {ms(s['project_p95_ms'])} ms |")


if __name__ == "__main__":
    main()
