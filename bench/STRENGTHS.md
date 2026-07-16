# Evaluator strengths benchmark

`run-eval-strengths.sh` is a focused complement to the general benchmark
harness.  It records evaluator shapes on which jinx has a clear advantage over
the pinned C++ Nix oracle.  It is intentionally not a claim that jinx wins every
workload: the general hello/Firefox/ISO results and memory tradeoffs remain in
`REPORT.md`.

Before timing a workload, the harness evaluates it once with each engine and
requires byte-identical stdout.  All jinx rows use the shipping configuration
(PGO binary, JIT off).  Hyperfine alternates commands and performs explicit
warmups.

## Workloads

| Workload | Evaluator surface |
|---|---|
| `parse` | Hand-written parser over nixpkgs' large `all-packages.nix` |
| `generic-closure` | 20,001 primitive integer keys and duplicate detection |
| `deep-force` | Wide scalar leaves, a shared graph, and real attr/list cycles |
| `ops` | `genList`, strict fold, sort, string-keyed attrset construction |
| `iso` | Full NixOS minimal-ISO evaluation and derivation construction |

## Current PGO results

Measured on the same aarch64-darwin machine against Determinate Nix 2.33.3
and nixpkgs `9675111`:

| Workload | C++ Nix | jinx | Speedup |
|---|---:|---:|---:|
| Parse `all-packages.nix` | 56.4 ms | 7.4 ms | **7.62×** |
| `genericClosure`, 20k keys | 63.7 ms | 8.4 ms | **7.60×** |
| Wide/shared/cyclic `deepSeq` | 75.4 ms | 15.9 ms | **4.73×** |
| Allocation/list/attr operations | 62.6 ms | 15.5 ms | **4.03×** |
| NixOS minimal ISO | 6.952 s | 4.665 s | **1.49×** |

These are workload-specific strengths. In particular, the attrset-heavy
Firefox evaluation remains a memory/CPU tradeoff and is intentionally reported
in the general benchmark report rather than this chart.

## Reproduce

```sh
NIXPKGS=/path/to/nixpkgs bash bench/run-eval-strengths.sh
```

The command writes JSON and provenance to the local
`bench/results/strengths/` directory and regenerates
`bench/graphs/eval-strengths.svg`; the generated result directory is not
checked in.
