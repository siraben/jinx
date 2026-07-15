#!/usr/bin/env python3
"""Render speedups from either parity-checked evaluator suite."""

import html
import json
import os
import sys

MODE = sys.argv[1] if len(sys.argv) > 1 else "compute"
RESULTS = sys.argv[2] if len(sys.argv) > 2 else f"bench/results/{MODE}"
if MODE == "compute":
    WORKLOADS = [
        ("numerical-fold", "strict numerical fold, 1M"),
        ("stable-sort", "stable sort, 100k mixed ints"),
        ("prime-scan", "trial-division prime scan"),
        ("fibonacci", "naive recursive fibonacci"),
        ("nqueens", "10-queens search"),
        ("record-shapes", "static record equality/select"),
    ]
    OUTPUT = sys.argv[3] if len(sys.argv) > 3 else "bench/graphs/compute.svg"
    width, left, right, top, row_height = 850, 270, 80, 88, 58
    title = "Compute-heavy Nix evaluation"
    description = "C++ Nix parity line with paired jinx interpreter and JIT speedup bars for six compute workloads."
else:
    if MODE != "strengths":
        raise SystemExit(f"unknown suite: {MODE}")
    WORKLOADS = [
        ("parse", "parse all-packages.nix"),
        ("generic-closure", "genericClosure, 20k keys"),
        ("deep-force", "deepSeq, wide/shared/cyclic"),
        ("ops", "allocation/list/attr operations"),
        ("iso", "NixOS minimal ISO evaluation"),
    ]
    OUTPUT = sys.argv[3] if len(sys.argv) > 3 else "bench/graphs/eval-strengths.svg"
    width, left, right, top, row_height = 790, 280, 85, 70, 46
    title = "Where jinx materially outperforms C++ Nix"
    description = "Jinx speedup over C++ Nix for five parity-checked evaluator workloads."


def load(name):
    with open(os.path.join(RESULTS, name + ".json"), encoding="utf-8") as source:
        results = json.load(source)["results"]
    expected = 3 if MODE == "compute" else 2
    if len(results) != expected:
        raise ValueError(f"{name}: expected {expected} benchmark rows")
    oracle = results[0]["mean"]
    return [oracle / result["mean"] for result in results[1:]], [
        result["mean"] for result in results
    ]


rows = [(label, *load(name)) for name, label in WORKLOADS]
height = top + len(rows) * row_height + (42 if MODE == "compute" else 34)
plot_width = width - left - right
maximum = max(max(speedups) for _, speedups, _ in rows) * 1.12
if MODE == "compute":
    maximum = max(1.15, maximum)
parity_x = left + plot_width / maximum

lines = [
    f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
    f'viewBox="0 0 {width} {height}" font-family="-apple-system,Segoe UI,Roboto,sans-serif" '
    'role="img" aria-labelledby="chart-title chart-description">',
    f'<title id="chart-title">{html.escape(title)}</title>',
    f'<desc id="chart-description">{html.escape(description)}</desc>',
    f'<rect width="{width}" height="{height}" fill="#ffffff"/>',
    f'<text x="{width / 2}" y="{27 if MODE == "compute" else 28}" text-anchor="middle" '
    f'font-size="17" font-weight="700" fill="#1a1a2e">{title}</text>',
]
if MODE == "compute":
    lines += [
        f'<rect x="{left}" y="40" width="12" height="12" rx="2" fill="#60a5fa"/>',
        f'<text x="{left + 18}" y="51" font-size="11" fill="#374151">jinx interpreter</text>',
        f'<rect x="{left + 130}" y="40" width="12" height="12" rx="2" fill="#1d4ed8"/>',
        f'<text x="{left + 148}" y="51" font-size="11" fill="#374151">jinx JIT</text>',
    ]
lines += [
    f'<line x1="{parity_x:.1f}" y1="{top - (4 if MODE == "compute" else 9)}" '
    f'x2="{parity_x:.1f}" y2="{height - 24}" stroke="#6b7280" stroke-width="1" '
    'stroke-dasharray="4 3"/>',
    f'<text x="{parity_x:.1f}" y="{top - (9 if MODE == "compute" else 14)}" '
    f'text-anchor="middle" font-size="11" fill="#6b7280">'
    f'{"1× C++ Nix" if MODE == "compute" else "1× parity"}</text>',
]

for index, (label, speedups, means) in enumerate(rows):
    y = top + index * row_height
    safe_label = html.escape(label)
    if MODE == "compute":
        lines.append(
            f'<text x="{left - 12}" y="{y + 27}" text-anchor="end" font-size="12.5" '
            f'fill="#1a1a2e">{safe_label}</text>'
        )
        bars, labels = [], []
        for offset, (speedup, color) in enumerate(zip(speedups, ("#60a5fa", "#1d4ed8"))):
            bar_width = plot_width * speedup / maximum
            bar_y = y + 5 + offset * 22
            bars.append(
                f'<rect x="{left}" y="{bar_y}" width="{bar_width:.1f}" height="18" '
                f'rx="3" fill="{color}"/>'
            )
            labels.append(
                f'<text x="{left + bar_width + 7:.1f}" y="{bar_y + 14}" font-size="11.5" '
                f'font-weight="700" fill="#1a1a2e">{speedup:.2f}×</text>'
            )
        lines += bars + labels
    else:
        speedup, bar_width = speedups[0], plot_width * speedups[0] / maximum
        detail = html.escape(f"Nix {means[0] * 1000:.1f} ms; jinx {means[1] * 1000:.1f} ms")
        lines += [
            f'<text x="{left - 12}" y="{y + 22}" text-anchor="end" font-size="12.5" '
            f'fill="#1a1a2e">{safe_label}</text>',
            f'<rect x="{left}" y="{y + 7}" width="{bar_width:.1f}" height="22" rx="3" fill="#2563eb"/>',
            f'<text x="{left + bar_width + 8:.1f}" y="{y + 23}" text-anchor="start" font-size="12.5" '
            f'font-weight="700" fill="#1a1a2e">{speedup:.2f}×</text>',
            f'<text x="{left - 12}" y="{y + 38}" text-anchor="end" font-size="11" '
            f'fill="#6b7280">{detail}</text>',
        ]

lines.append("</svg>")
os.makedirs(os.path.dirname(OUTPUT) or ".", exist_ok=True)
with open(OUTPUT, "w", encoding="utf-8") as output:
    output.write("\n".join(lines))
print("wrote", OUTPUT)
