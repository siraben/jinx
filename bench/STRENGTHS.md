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
| Parse `all-packages.nix` | 49.3 ms | 6.0 ms | **8.18×** |
| `genericClosure`, 20k keys | 54.1 ms | 5.5 ms | **9.93×** |
| Wide/shared/cyclic `deepSeq` | 70.5 ms | 15.4 ms | **4.59×** |
| Allocation/list/attr operations | 53.1 ms | 14.1 ms | **3.77×** |
| NixOS minimal ISO | 6.522 s | 4.483 s | **1.46×** |

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
