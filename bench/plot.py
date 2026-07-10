#!/usr/bin/env python3
"""Render the benchmark results in bench/results/ as committable SVG charts.

Reads the hyperfine JSON (one file per benchmark) + rss.txt produced by
bench/run-benchmarks.sh and writes self-contained SVGs to bench/graphs/:

  speedup.svg   jinx speedup vs the C++ Nix oracle, all benchmarks
  walltime.svg  jinx vs oracle wall time on the real nixpkgs evals
  rss.svg       jinx vs oracle peak RSS (the memory trade-off)

Pure standard library (no matplotlib/numpy) so `python3 bench/plot.py` is a
zero-dependency, reproducible step. Usage:
    python3 bench/plot.py [results_dir] [graphs_dir]
"""
import json, os, sys, html

RES = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "results")
OUT = sys.argv[2] if len(sys.argv) > 2 else os.path.join(os.path.dirname(__file__), "graphs")
os.makedirs(OUT, exist_ok=True)

INK, MUT, GRID = "#1a1a2e", "#6b7280", "#e5e7eb"
JINX, JIT, ORACLE = "#2563eb", "#60a5fa", "#9ca3af"
BG = "#ffffff"


def classify(cmd):
    if ".oracle" in cmd or "nix-instantiate" in cmd and "target/release" not in cmd:
        return "oracle"
    if "JINX_GC_OFF=1" in cmd:
        return None  # skip the gc-off ablation
    if "JINX_JIT=1" in cmd:
        return "jit"
    return "jinx"


def load(name):
    p = os.path.join(RES, name + ".json")
    if not os.path.exists(p):
        return None
    d = json.load(open(p))
    out = {}
    for r in d["results"]:
        k = classify(r["command"])
        if k and k not in out:
            out[k] = (r["mean"], r.get("stddev", 0))
    return out


def load_rss():
    p = os.path.join(RES, "rss.txt")
    r = {}
    if os.path.exists(p):
        for line in open(p):
            parts = line.split()
            if len(parts) == 2:
                r[parts[0]] = int(parts[1])
    return r


def svg_header(w, h, title):
    return [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" '
        f'viewBox="0 0 {w} {h}" font-family="-apple-system,Segoe UI,Roboto,sans-serif">',
        f'<rect width="{w}" height="{h}" fill="{BG}"/>',
        f'<text x="{w/2}" y="28" text-anchor="middle" font-size="17" '
        f'font-weight="700" fill="{INK}">{html.escape(title)}</text>',
    ]


def bar(x, y, w, h, fill, rx=3):
    return f'<rect x="{x:.1f}" y="{y:.1f}" width="{w:.1f}" height="{h:.1f}" rx="{rx}" fill="{fill}"/>'


def txt(x, y, s, size=12, fill=INK, anchor="middle", weight="400"):
    return (f'<text x="{x:.1f}" y="{y:.1f}" text-anchor="{anchor}" font-size="{size}" '
            f'font-weight="{weight}" fill="{fill}">{html.escape(str(s))}</text>')


def write(name, lines):
    lines.append("</svg>")
    p = os.path.join(OUT, name)
    open(p, "w").write("\n".join(lines))
    print("wrote", p)


# ---- chart 1: speedup vs oracle (horizontal bars) --------------------------
def chart_speedup():
    order = [("parse", "parse all-packages.nix"), ("ops", "ops.nix (alloc/list)"),
             ("fib", "fib.nix (compute)"), ("hello", "nixpkgs -A hello"),
             ("firefox", "nixpkgs -A firefox"), ("iso", "NixOS minimal ISO")]
    rows = []
    for key, label in order:
        d = load(key)
        if not d or "oracle" not in d:
            continue
        # For the compute micros (fib/ops) the JIT is the intended config
        # (labeled below); the real evals show jinx's shipping default (JIT off).
        jkey = "jit" if key in ("fib", "ops") and "jit" in d else "jinx"
        if jkey not in d:
            jkey = "jinx" if "jinx" in d else "jit"
        speed = d["oracle"][0] / d[jkey][0]
        rows.append((label + (" (jit)" if jkey == "jit" else ""), speed))
    W, rowh, top, left = 720, 42, 56, 250
    H = top + rowh * len(rows) + 30
    L = svg_header(W, H, "jinx speedup vs C++ Nix (higher is better)")
    maxr = max(r[1] for r in rows) * 1.12
    plotw = W - left - 70
    x0 = left
    # 1x reference line
    x1 = x0 + plotw * (1.0 / maxr)
    L.append(f'<line x1="{x1:.1f}" y1="{top-6}" x2="{x1:.1f}" y2="{H-24}" stroke="{MUT}" stroke-dasharray="4 3" stroke-width="1"/>')
    L.append(txt(x1, top - 10, "1× (parity)", 11, MUT))
    for i, (label, s) in enumerate(rows):
        y = top + i * rowh
        bw = plotw * (s / maxr)
        L.append(txt(left - 12, y + rowh * 0.62, label, 12.5, INK, "end"))
        L.append(bar(x0, y + 8, bw, rowh - 18, JINX))
        L.append(txt(x0 + bw + 8, y + rowh * 0.62, f"{s:.2f}×", 12.5, INK, "start", "700"))
    write("speedup.svg", L)


# ---- chart 2: wall time on real evals (grouped bars) -----------------------
def chart_walltime():
    evals = [("hello", "hello", 1000, "ms"), ("firefox", "firefox", 1000, "ms"),
             ("iso", "NixOS ISO", 1, "s")]
    panels = []
    for key, label, scale, unit in evals:
        d = load(key)
        if d and "oracle" in d and "jinx" in d:
            panels.append((label, d["oracle"][0] * scale, d["jinx"][0] * scale, unit))
    if not panels:
        return
    pw, gap, top = 200, 40, 70
    H, W = 300, len(panels) * pw + (len(panels) + 1) * gap
    L = svg_header(W, H, "Wall time: jinx vs C++ Nix on real nixpkgs evals")
    baseY, barmax = H - 46, H - 46 - top
    for i, (label, o, j, unit) in enumerate(panels):
        cx = gap + i * (pw + gap) + pw / 2
        m = max(o, j) * 1.15
        oh, jh = barmax * o / m, barmax * j / m
        bw = 58
        L.append(bar(cx - bw - 8, baseY - oh, bw, oh, ORACLE))
        L.append(bar(cx + 8, baseY - jh, bw, jh, JINX))
        L.append(txt(cx - bw / 2 - 8, baseY - oh - 7, f"{o:.0f}{unit}" if unit == "ms" else f"{o:.2f}{unit}", 11.5, MUT))
        L.append(txt(cx + bw / 2 + 8, baseY - jh - 7, f"{j:.0f}{unit}" if unit == "ms" else f"{j:.2f}{unit}", 11.5, INK, weight="700"))
        L.append(txt(cx, baseY + 18, label, 13, INK))
        L.append(txt(cx, baseY + 34, f"{o/j:.2f}× faster", 11, JINX, weight="700"))
    # legend
    L.append(bar(W - 200, 44, 12, 12, ORACLE)); L.append(txt(W - 182, 54, "C++ Nix", 11.5, MUT, "start"))
    L.append(bar(W - 110, 44, 12, 12, JINX)); L.append(txt(W - 92, 54, "jinx", 11.5, INK, "start"))
    write("walltime.svg", L)


# ---- chart 3: peak RSS (grouped bars) --------------------------------------
def chart_rss():
    rss = load_rss()
    evals = [("hello", "hello"), ("firefox", "firefox"), ("iso", "NixOS ISO")]
    panels = []
    for key, label in evals:
        o, j = rss.get(f"rss-{key}-oracle"), rss.get(f"rss-{key}-jinx")
        if o and j:
            panels.append((label, o, j))
    if not panels:
        return
    pw, gap, top = 200, 40, 70
    H, W = 300, len(panels) * pw + (len(panels) + 1) * gap
    L = svg_header(W, H, "Peak RSS: jinx vs C++ Nix (lower is better)")
    baseY, barmax = H - 46, H - 46 - top

    def fmt(b):
        return f"{b/2**30:.2f} GiB" if b >= 2**30 else f"{b/2**20:.0f} MiB"
    for i, (label, o, j) in enumerate(panels):
        cx = gap + i * (pw + gap) + pw / 2
        m = max(o, j) * 1.15
        oh, jh = barmax * o / m, barmax * j / m
        bw = 58
        L.append(bar(cx - bw - 8, baseY - oh, bw, oh, ORACLE))
        L.append(bar(cx + 8, baseY - jh, bw, jh, JINX))
        L.append(txt(cx - bw / 2 - 8, baseY - oh - 7, fmt(o), 11, MUT))
        L.append(txt(cx + bw / 2 + 8, baseY - jh - 7, fmt(j), 11, INK, weight="700"))
        L.append(txt(cx, baseY + 18, label, 13, INK))
        L.append(txt(cx, baseY + 34, f"{j/o:.1f}× oracle", 11, MUT))
    L.append(bar(W - 200, 44, 12, 12, ORACLE)); L.append(txt(W - 182, 54, "C++ Nix (Boehm)", 11.5, MUT, "start"))
    L.append(bar(W - 78, 44, 12, 12, JINX)); L.append(txt(W - 60, 54, "jinx", 11.5, INK, "start"))
    write("rss.svg", L)


# ---- chart 4: parallel GC (single vs parallel marking) ---------------------
def chart_parallel_gc():
    p = os.path.join(RES, "parallel-gc.json")
    if not os.path.exists(p):
        return
    d = json.load(open(p))
    s, par = d["single"], d["parallel"]
    metrics = [
        ("GC mark pause", s["gc_pause_s"], par["gc_pause_s"], "s"),
        ("total wall", s["wall_ms"] / 1000, par["wall_ms"] / 1000, "s"),
    ]
    pw, gap, top = 220, 46, 74
    H, W = 300, len(metrics) * pw + (len(metrics) + 1) * gap
    L = svg_header(W, H, "Parallel GC: single vs parallel marking (search workload)")
    baseY, barmax = H - 46, H - 46 - top
    for i, (label, sv, pv, unit) in enumerate(metrics):
        cx = gap + i * (pw + gap) + pw / 2
        m = max(sv, pv) * 1.16
        sh, ph = barmax * sv / m, barmax * pv / m
        bw = 62
        L.append(bar(cx - bw - 8, baseY - sh, bw, sh, ORACLE))
        L.append(bar(cx + 8, baseY - ph, bw, ph, JINX))
        L.append(txt(cx - bw / 2 - 8, baseY - sh - 7, f"{sv:.2f}{unit}", 11.5, MUT))
        L.append(txt(cx + bw / 2 + 8, baseY - ph - 7, f"{pv:.2f}{unit}", 11.5, INK, weight="700"))
        L.append(txt(cx, baseY + 18, label, 13, INK))
        L.append(txt(cx, baseY + 34, f"{sv/pv:.2f}× faster", 11, JINX, weight="700"))
    L.append(bar(W - 250, 44, 12, 12, ORACLE)); L.append(txt(W - 232, 54, "single-threaded", 11.5, MUT, "start"))
    L.append(bar(W - 118, 44, 12, 12, JINX)); L.append(txt(W - 100, 54, f"parallel ({par['threads']}×)", 11.5, INK, "start"))
    write("parallel-gc.svg", L)


if __name__ == "__main__":
    chart_speedup()
    chart_walltime()
    chart_rss()
    chart_parallel_gc()
