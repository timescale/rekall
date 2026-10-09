#!/usr/bin/env python3
"""Cheapest instance per engine at a given (recall, QPS) target.

The cohere-1M sizing question is not "which engine is faster on box X" but
"what is the least expensive box on which each engine still hits the target".
That is a cost comparison at ISO-PERFORMANCE, so the target must be applied
after the fact rather than baked into the suite: run the whole ladder, then
read off, per engine, the cheapest rung whose recall/QPS frontier clears the
bar.

Input is one summary.json per rung (written by `rekall publish`, or found in
a published run directory). Each is tagged with the instance it ran on.

Cost is instance + EBS, because on these small boxes EBS is not a rounding
error: 100 GB of gp3 at the 16000-IOPS maximum costs ~$108/mo, more than any
of the instances in the ladder. A configuration that needs provisioned IOPS
to hit its target can easily lose to a bigger box that needs none.

Usage:
  # Local results directories:
  scripts/cost-frontier.py --recall 0.95 --qps 100 \\
      c8g.medium=results/rung-2g m8g.large=results/rung-8g

  # Or straight from the published runs (what remote ladders produce):
  BASE=https://timescale.github.io/rekall/runs/cohere-1m
  scripts/cost-frontier.py --recall 0.95 --qps 100 \\
      m8g.large=$BASE/2026-08-25/m8g.large-101500 \\
      c8g.large=$BASE/2026-08-25/c8g.large-141500 \\
      c8g.medium=$BASE/2026-08-26/c8g.medium-090000

  # sweep the target instead of picking one
  scripts/cost-frontier.py --grid <same args>

Prices are approximate us-east-1 on-demand and are declared below rather
than fetched. VERIFY THEM before quoting any number externally.
"""

import argparse
import json
import sys
import urllib.request
from pathlib import Path

HOURS = 730

# instance -> (vCPU, RAM GiB, $/hour on-demand, us-east-1, approximate)
INSTANCES = {
    "c8g.medium": (1, 2, 0.0399),
    "c8g.large": (2, 4, 0.0797),
    "m8g.large": (2, 8, 0.0898),
    "r8g.large": (2, 16, 0.1178),
    "c8i.large": (2, 4, 0.0850),
    "m8i.large": (2, 8, 0.0970),
    "r8i.large": (2, 16, 0.1270),
}


def ebs_monthly(gb: int, iops: int, throughput: int) -> float:
    """gp3: $0.08/GB-mo, $0.005/IOPS-mo above 3000, $0.04/MiB/s-mo above 125."""
    return gb * 0.08 + max(0, iops - 3000) * 0.005 + max(0, throughput - 125) * 0.04


def monthly_cost(instance: str, gb: int, iops: int, throughput: int) -> float:
    if instance not in INSTANCES:
        raise SystemExit(f"unknown instance '{instance}' — add it to INSTANCES")
    return INSTANCES[instance][2] * HOURS + ebs_monthly(gb, iops, throughput)


def engine_of(point: dict) -> str:
    """Group sweep points into engines by index type."""
    t = point.get("index_type", "")
    if t in ("prism"):
        return "PRISM"
    if t == "pgvector-hnsw":
        return "HNSW"
    return t or "unknown"


def best_qps(points: list, engine: str, recall: float):
    """Max QPS among this engine's points that reach `recall`.

    No interpolation: a sweep point either clears the bar or it does not.
    Interpolating between points would invent a configuration that was never
    measured, which is the wrong move when the output is a purchasing
    recommendation.
    """
    ok = [p for p in points if engine_of(p) == engine and p["recall"] >= recall]
    if not ok:
        return None
    return max(ok, key=lambda p: p["qps"])


def load(spec: str):
    """Accept `INSTANCE=<path|dir|url>`.

    URLs matter because the ladder runs remotely: `rekall infra run`
    auto-publishes each rung, so the summaries live on the published site
    rather than on the workstation where this script runs. Pointing straight
    at them avoids a manual download step.
    """
    if "=" not in spec:
        raise SystemExit(f"expected INSTANCE=<path|dir|url>, got '{spec}'")
    inst, loc = spec.split("=", 1)
    if loc.startswith(("http://", "https://")):
        url = loc if loc.endswith(".json") else loc.rstrip("/") + "/summary.json"
        with urllib.request.urlopen(url) as fh:  # noqa: S310 - operator-supplied
            return inst, json.load(fh)["points"]
    p = Path(loc)
    if p.is_dir():
        p = p / "summary.json"
    with p.open() as fh:
        return inst, json.load(fh)["points"]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("runs", nargs="+", metavar="INSTANCE=SUMMARY.JSON")
    ap.add_argument("--recall", type=float, default=0.95)
    ap.add_argument("--qps", type=float, default=100.0)
    ap.add_argument("--grid", action="store_true", help="sweep targets instead")
    ap.add_argument("--disk-gb", type=int, default=100)
    ap.add_argument("--disk-iops", type=int, default=16000)
    ap.add_argument("--disk-throughput", type=int, default=1000)
    args = ap.parse_args()

    rungs = [load(s) for s in args.runs]
    cost = {
        inst: monthly_cost(inst, args.disk_gb, args.disk_iops, args.disk_throughput)
        for inst, _ in rungs
    }
    engines = sorted({engine_of(p) for _, pts in rungs for p in pts})

    print(
        f"\ndisk: {args.disk_gb} GB gp3, {args.disk_iops} IOPS, "
        f"{args.disk_throughput} MiB/s  ->  ${ebs_monthly(args.disk_gb, args.disk_iops, args.disk_throughput):.0f}/mo"
    )
    print("prices are APPROXIMATE us-east-1 on-demand — verify before quoting\n")

    # Per-rung capability table: what each engine achieves where.
    print(f"{'instance':<12}{'RAM':>5}{'$/mo':>7}  ", end="")
    for e in engines:
        print(f"{e + ' QPS@' + str(args.recall):>22}", end="")
    print()
    for inst, pts in rungs:
        ram = INSTANCES[inst][1]
        print(f"{inst:<12}{ram:>4}G{cost[inst]:>7.0f}  ", end="")
        for e in engines:
            b = best_qps(pts, e, args.recall)
            cell = "—" if b is None else f"{b['qps']:.0f}"
            print(f"{cell:>22}", end="")
        print()

    if args.grid:
        targets = [(r, q) for r in (0.90, 0.95, 0.99) for q in (50, 100, 250, 500)]
    else:
        targets = [(args.recall, args.qps)]

    print(f"\n{'recall':>7}{'QPS':>7}  ", end="")
    for e in engines:
        print(f"{'cheapest ' + e:>28}", end="")
    print()
    for r, q in targets:
        print(f"{r:>7.2f}{q:>7.0f}  ", end="")
        for e in engines:
            winners = [
                (cost[inst], inst, b["qps"])
                for inst, pts in rungs
                if (b := best_qps(pts, e, r)) and b["qps"] >= q
            ]
            if not winners:
                print(f"{'infeasible on this ladder':>28}", end="")
            else:
                c, inst, achieved = min(winners)
                print(f"{inst + f' ${c:.0f} ({achieved:.0f} QPS)':>28}", end="")
        print()
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
