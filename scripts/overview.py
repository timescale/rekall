#!/usr/bin/env python3
"""Compile a best-HNSW-vs-best-PRISM overview from published rekall runs.

For each dataset, picks the newest published run (gh-pages branch) at or
after --since that contains both a PRISM and a competitor (default
pgvector-hnsw) index, and prints a markdown table of QPS and latency at
the requested recall targets, plus build time and index size, with a
PRISM-vs-competitor delta column. All numbers within a dataset table
come from that single run — runs are never mixed.

Numbers are read from the run's summary.json (the same compact data that
drives the published comparison tables) and interpolated with the same
rules as the site: per-config Pareto line, log-interpolation at exactly
the stated recall, no extrapolation. Within each family the single best
config is chosen (most recall targets won; the footnote names it) and all
of that family's cells come from that one config.

summary.json is looked up on gh-pages first, then in the S3 archive; for
older runs that predate summary.json, the run archive is downloaded from
S3 and the summary is regenerated locally via `rekall publish` with AWS
credentials stripped (the local write happens before the upload step, so
nothing is published).

Usage:
  scripts/overview.py --since 2026-07-10
  scripts/overview.py --since 2026-07-10 --recalls 0.90,0.93,0.95,0.98 \
      --datasets cohere-50m,cohere-100m --percentile p50
"""

import argparse
import json
import math
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
PAGES_REF = "origin/gh-pages"
PAGES_BASE_URL = "https://timescale.github.io/rekall"
S3_BUCKET = "rekall-bench"
S3_PREFIX = "runs"
PRICING_REGION = "us-east-1"  # infra default_region()


def sh(cmd, check=True, **kw):
    return subprocess.run(
        cmd, cwd=REPO, capture_output=True, text=True, check=check, **kw
    )


def git_show(path):
    r = sh(["git", "show", f"{PAGES_REF}:{path}"], check=False)
    return r.stdout if r.returncode == 0 else None


def list_runs(since):
    """Published runs at/after `since`, as (dataset, date, rundir, path)."""
    r = sh(["git", "fetch", "origin", "gh-pages"], check=False)
    if r.returncode != 0:
        print("warning: git fetch origin gh-pages FAILED — numbers may be "
              f"stale:\n  {r.stderr.strip()}", file=sys.stderr)
    r = sh(["git", "ls-tree", "-r", "--name-only", PAGES_REF])
    runs = {}
    for line in r.stdout.splitlines():
        m = re.match(r"runs/([^/]+)/(\d{4}-\d{2}-\d{2})/([^/]+)/", line)
        if m and m.group(2) >= since:
            ds, date, rd = m.groups()
            runs[(ds, date, rd)] = f"runs/{ds}/{date}/{rd}"
    out = {}
    for (ds, date, rd), path in runs.items():
        # Sort key: date then launch time (rundir suffix is HHMMSS).
        hhmmss = rd.rsplit("-", 1)[-1]
        out.setdefault(ds, []).append(((date, hhmmss), path))
    for ds in out:
        out[ds].sort(reverse=True)
    return out


def load_summary(run_path, cache):
    """summary.json for a run: gh-pages -> S3 -> regenerate locally."""
    s = git_show(f"{run_path}/summary.json")
    if s:
        return json.loads(s)
    rid = run_path[len("runs/") :]
    s3_url = f"s3://{S3_BUCKET}/{S3_PREFIX}/{rid}/"
    r = sh(["aws", "s3", "cp", s3_url + "summary.json", "-"], check=False)
    if r.returncode == 0 and r.stdout.strip():
        return json.loads(r.stdout)
    # Legacy run without summary.json: pull the archive and regenerate
    # (cached across invocations).
    dest = cache / rid
    cached = dest / "summary.json"
    if cached.exists():
        return json.loads(cached.read_text())
    print(f"  regenerating summary for {rid} (downloading archive)…",
          file=sys.stderr)
    dest.mkdir(parents=True, exist_ok=True)
    r = sh(
        ["aws", "s3", "cp", s3_url, str(dest), "--recursive",
         "--exclude", "*.svg", "--exclude", "*.html",
         "--exclude", "*.gnuplot"],
        check=False,
    )
    if r.returncode != 0:
        print(f"  S3 download failed for {rid}: {r.stderr.strip()}",
              file=sys.stderr)
        return None
    (dest / "summary.json").unlink(missing_ok=True)
    # Strip AWS credentials so `publish` writes summary.json locally and
    # then fails on the (unwanted) upload step: nothing is published.
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.startswith("AWS_")
    }
    env["AWS_CONFIG_FILE"] = "/dev/null"
    env["AWS_SHARED_CREDENTIALS_FILE"] = "/dev/null"
    env["AWS_EC2_METADATA_DISABLED"] = "true"
    sh(
        ["cargo", "run", "--release", "--quiet", "--", "publish",
         str(dest), "--force", "--s3-only"],
        check=False, env=env,
    )
    p = dest / "summary.json"
    if not p.exists():
        print(f"  could not regenerate summary for {rid}", file=sys.stderr)
        return None
    return json.loads(p.read_text())


# --- interpolation (mirrors rekall-lib/src/publish/compare.rs) -----------


def frontier(points):
    """Pareto frontier, recall-ascending / QPS-ascending."""
    pts = sorted(points, key=lambda p: (-p["recall"], -p["qps"]))
    out, best = [], -math.inf
    for p in pts:
        if p["qps"] > best:
            out.append(p)
            best = p["qps"]
    out.reverse()
    return out


def log_interp(v0, v1, t):
    if v0 <= 0 or v1 <= 0:
        return v0 + t * (v1 - v0)
    return math.exp(math.log(v0) + t * (math.log(v1) - math.log(v0)))


def cell_at(line, target):
    """Interpolated dict at exactly `target`, or None if unspanned."""
    if not line or target > line[-1]["recall"] or target < line[0]["recall"]:
        return None
    for p in line:
        if p["recall"] == target:
            return dict(p)
    for a, b in zip(line, line[1:]):
        if a["recall"] < target < b["recall"]:
            t = (target - a["recall"]) / (b["recall"] - a["recall"])
            return {
                k: log_interp(a[k], b[k], t)
                for k in ("qps", "p50_latency_us", "p95_latency_us",
                          "p99_latency_us")
            }
    return None


def best_config(summary, index_type, targets):
    """(name, {target: cell}) for the family's single best config."""
    by_cfg = {}
    for p in summary["points"]:
        if p["index_type"] == index_type:
            by_cfg.setdefault(p["index_name"], []).append(p)
    if not by_cfg:
        return None, {}
    lines = {name: frontier(pts) for name, pts in by_cfg.items()}
    cells = {
        name: {t: cell_at(line, t) for t in targets}
        for name, line in lines.items()
    }
    # Most targets won (highest QPS among spanning configs); tie-break on
    # QPS at the highest target the tied configs all span.
    def wins(name):
        n = 0
        for t in targets:
            c = cells[name][t]
            if c is None:
                continue
            others = [
                oc["qps"]
                for o, tc in cells.items() if o != name
                for oc in [tc[t]] if oc is not None
            ]
            if not others or c["qps"] >= max(others):
                n += 1
        return n

    def top_qps(name):
        spanned = [
            c for t in sorted(targets)
            for c in [cells[name][t]] if c is not None
        ]
        return spanned[-1]["qps"] if spanned else 0.0

    name = max(cells, key=lambda n: (wins(n), top_qps(n)))
    return name, cells[name]


# --- formatting -----------------------------------------------------------


def fmt_qps(v):
    return "—" if v is None else f"{v:,.0f}"


def fmt_ms(us):
    return "—" if us is None else f"{us / 1000:.2f}"


def fmt_delta(prism, ref, lower_is_better=False):
    if prism is None or ref is None or ref == 0:
        return "—"
    d = (prism / ref - 1) * 100
    arrow = ""
    if lower_is_better:
        arrow = " ✓" if d < 0 else ""
    else:
        arrow = " ✓" if d > 0 else ""
    return f"{d:+.0f}%{arrow}"


def fmt_secs(s):
    if s is None:
        return "—"
    h, rem = divmod(int(s), 3600)
    m = rem // 60
    return f"{h}h {m:02d}m" if h else f"{m}m {rem % 60:02d}s"


def fmt_gb(b):
    return "—" if b is None else f"{b / 1e9:.1f} GB"


def render_table(rows):
    """Pipe-table lines (header + separator + body), columns aligned."""
    widths = [max(len(r[i]) for r in rows) for i in range(len(rows[0]))]
    def line(r):
        return "| " + " | ".join(c.ljust(w) for c, w in zip(r, widths)) + " |"
    sep = "|" + "|".join("-" * (w + 2) for w in widths) + "|"
    return [line(rows[0]), sep] + [line(r) for r in rows[1:]]




def instance_price(itype, cache={}):
    if itype in cache:
        return cache[itype]
    price = None
    r = sh(
        ["aws", "pricing", "get-products", "--service-code", "AmazonEC2",
         "--region", "us-east-1",
         "--filters",
         f"Type=TERM_MATCH,Field=instanceType,Value={itype}",
         f"Type=TERM_MATCH,Field=regionCode,Value={PRICING_REGION}",
         "Type=TERM_MATCH,Field=operatingSystem,Value=Linux",
         "Type=TERM_MATCH,Field=tenancy,Value=Shared",
         "Type=TERM_MATCH,Field=preInstalledSw,Value=NA",
         "Type=TERM_MATCH,Field=capacitystatus,Value=Used",
         "--max-results", "1", "--output", "json"],
        check=False,
    )
    if r.returncode == 0:
        try:
            p = json.loads(json.loads(r.stdout)["PriceList"][0])
            for term in p["terms"]["OnDemand"].values():
                for dim in term["priceDimensions"].values():
                    price = float(dim["pricePerUnit"]["USD"])
        except (KeyError, IndexError, ValueError):
            pass
    cache[itype] = price
    return price


def main():
    ap = argparse.ArgumentParser(
        description="Best-HNSW-vs-best-PRISM overview from published "
        "rekall runs")
    ap.add_argument("--since", default="2026-07-10",
                    help="only runs on/after this date (YYYY-MM-DD)")
    ap.add_argument("--recalls", default="0.90,0.93,0.95,0.98",
                    help="comma-separated recall targets")
    ap.add_argument("--datasets", default=None,
                    help="comma-separated dataset filter (default: all)")
    ap.add_argument("--competitor", default="pgvector-hnsw",
                    help="competitor index_type (default: pgvector-hnsw)")
    ap.add_argument("--percentiles", default="p95",
                    help="comma-separated latency percentiles to tabulate "
                    "(of p50,p95,p99 — what summary.json records)")
    ap.add_argument("--cache",
                    default=os.path.expanduser("~/.cache/rekall-overview"),
                    help="cache dir for regenerated legacy summaries")
    ap.add_argument("--no-pricing", action="store_true",
                    help="skip the AWS pricing API lookup")
    ap.add_argument("--snapshot-recall", type=float, default=0.93,
                    help="recall target for the condensed snapshot table")
    args = ap.parse_args()

    targets = [float(x) for x in args.recalls.split(",")]
    snap = args.snapshot_recall
    calc_targets = targets if snap in targets else targets + [snap]
    pcts = [p.strip() for p in args.percentiles.split(",")]
    if bad := [p for p in pcts if p not in ("p50", "p95", "p99")]:
        ap.error(f"unknown percentile(s) {bad}; summary.json records "
                 "p50, p95, p99")
    cache = Path(args.cache)
    ds_filter = set(args.datasets.split(",")) if args.datasets else None

    if not shutil.which("aws"):
        print("warning: aws CLI not found — S3/pricing lookups disabled",
              file=sys.stderr)

    def ds_key(ds):
        # Numeric size when the name ends like "-10m"/"-50m", else lexical.
        m = re.search(r"(\d+)([kmb]?)$", ds)
        scale = {"": 1, "k": 1e3, "m": 1e6, "b": 1e9}
        return (int(m.group(1)) * scale[m.group(2)], ds) if m else (0, ds)

    runs = list_runs(args.since)
    results = []
    missing = []
    for ds in sorted(runs, key=ds_key):
        if ds_filter and ds not in ds_filter:
            continue
        chosen = None
        for _, path in runs[ds]:
            summary = load_summary(path, cache)
            if summary is None:
                continue
            types = {p["index_type"] for p in summary["points"]}
            if "prism" in types and args.competitor in types:
                chosen = (path, summary)
                break
            print(f"  {path}: no PRISM-vs-{args.competitor} pairing, "
                  "trying older run", file=sys.stderr)
        if not chosen:
            missing.append(ds)
            continue
        path, summary = chosen
        meta = json.loads(git_show(f"{path}/meta.json") or "{}")
        prism_name, prism = best_config(summary, "prism", calc_targets)
        cmp_name, cmp_ = best_config(summary, args.competitor, calc_targets)
        builds = {b["index_name"]: b for b in summary.get("builds", [])}
        results.append({
            "ds": ds, "path": path, "summary": summary, "meta": meta,
            "prism_name": prism_name, "prism": prism,
            "cmp_name": cmp_name, "cmp": cmp_,
            "prism_b": builds.get(prism_name, {}),
            "cmp_b": builds.get(cmp_name, {}),
        })

    def vs(c_val, m_val, fmt, lower_is_better=False):
        if c_val is None and m_val is None:
            return "—"
        d = fmt_delta(m_val, c_val, lower_is_better=lower_is_better)
        return f"{fmt(c_val)} → {fmt(m_val)} ({d.replace(' ✓', '')})"

    if results:
        print(f"## Snapshot — HNSW → PRISM at recall {snap:.2f}\n")
        snap_rows = [["Dataset", "Instance", "QPS", "p95 (ms)",
                      "Build time", "Index size"]]
        for r in results:
            c, m = r["cmp"].get(snap), r["prism"].get(snap)
            snap_rows.append([
                r["ds"],
                r["meta"].get("instance_type", "?"),
                vs(c and c["qps"], m and m["qps"], fmt_qps),
                vs(c and c["p95_latency_us"], m and m["p95_latency_us"],
                   fmt_ms, lower_is_better=True),
                vs(r["cmp_b"].get("build_time_secs"),
                   r["prism_b"].get("build_time_secs"),
                   fmt_secs, lower_is_better=True),
                vs(r["cmp_b"].get("size_bytes"),
                   r["prism_b"].get("size_bytes"),
                   fmt_gb, lower_is_better=True),
            ])
        print("\n".join(render_table(snap_rows)))

    for ds in missing:
        print(f"\n## {ds}\n\nNo run since {args.since} with both "
              f"PRISM and {args.competitor}.")

    for r in results:
        ds, path, summary, meta = r["ds"], r["path"], r["summary"], r["meta"]
        prism_name, prism = r["prism_name"], r["prism"]
        cmp_name, cmp_ = r["cmp_name"], r["cmp"]
        prism_b, cmp_b = r["prism_b"], r["cmp_b"]

        price = None if args.no_pricing else instance_price(
            meta.get("instance_type", ""))
        cost = f", ~${price:.2f}/h on-demand ({PRICING_REGION})" \
            if price else ""
        print(f"\n## {ds} — {meta.get('date')} "
              f"({meta.get('instance_type', '?')})\n")
        print(f"Instance: {meta.get('instance_type', '?')} — "
              f"{meta.get('cpu_model', '?')}, "
              f"{meta.get('cpu_count', '?')} vCPU, "
              f"{meta.get('memory_gb', 0):.0f} GB RAM{cost}  ")
        if meta.get("comment"):
            print(f"Run comment: {meta['comment']}  ")
        print(f"Run page: {PAGES_BASE_URL}/{path}/  ")
        print(f"Recall basis: {summary.get('recall_basis', '?')}\n")

        build_rows = [
            ["Metric", "HNSW", "PRISM", "Δ"],
            ["Build time",
             fmt_secs(cmp_b.get("build_time_secs")),
             fmt_secs(prism_b.get("build_time_secs")),
             fmt_delta(prism_b.get("build_time_secs"),
                       cmp_b.get("build_time_secs"), lower_is_better=True)],
            ["Index size",
             fmt_gb(cmp_b.get("size_bytes")),
             fmt_gb(prism_b.get("size_bytes")),
             fmt_delta(prism_b.get("size_bytes"),
                       cmp_b.get("size_bytes"), lower_is_better=True)],
        ]
        print("\n".join(render_table(build_rows)))
        print()

        def lat(cell):
            vals = [fmt_ms(cell and cell[f"{p}_latency_us"]) for p in pcts]
            return "/".join(vals)

        def lat_delta(m_cell, c_cell):
            ds = [
                fmt_delta(m_cell and m_cell[f"{p}_latency_us"],
                          c_cell and c_cell[f"{p}_latency_us"],
                          lower_is_better=True)
                for p in pcts
            ]
            return "/".join(ds)

        pct_label = "/".join(pcts)
        qps_rows = [["Recall", "HNSW QPS", "PRISM QPS", "Δ"]]
        lat_rows = [["Recall", f"HNSW {pct_label} (ms)",
                     f"PRISM {pct_label} (ms)", "Δ"]]
        for t in targets:
            c, m = cmp_.get(t), prism.get(t)
            qps_rows.append([
                f"{t:.2f}",
                fmt_qps(c and c["qps"]),
                fmt_qps(m and m["qps"]),
                fmt_delta(m and m["qps"], c and c["qps"]),
            ])
            lat_rows.append([f"{t:.2f}", lat(c), lat(m), lat_delta(m, c)])
        print("\n".join(render_table(qps_rows)))
        print()
        print("\n".join(render_table(lat_rows)))

        print(f"\nConfigs: PRISM = `{prism_name}`, "
              f"HNSW = `{cmp_name}`. \"—\" = recall target outside the "
              "config's measured sweep (no extrapolation). QPS/latency "
              "log-interpolated at exactly the stated recall within the "
              "config's own Pareto line. Δ is PRISM vs HNSW; ✓ marks "
              "PRISM better.")


if __name__ == "__main__":
    main()
