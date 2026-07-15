# Compute-heavy evaluator benchmark

`run-compute-benchmarks.sh` isolates six CPU-heavy Nix-language kernels. It is
self-contained (no nixpkgs checkout) and compares C++ Nix with both the shipping
jinx interpreter and the opt-in JIT. Before timing, all three commands must
produce byte-identical strict-evaluation output.

## Workloads

| Workload | Dominant evaluator work |
|---|---|
| `numerical-fold` | One million strict fold steps, closure calls, integer multiply/add |
| `stable-sort` | 100k deterministically mixed integers and an evaluator comparator |
| `prime-scan` | Trial division, branches, recursion, and integer division |
| `fibonacci` | Naive recursion, function calls, forcing, branches, integer arithmetic |
| `nqueens` | Combinatorial search, closures, recursive lists, and conflict checks |
| `record-shapes` | Repeated construction, equality, and selection on shared static record shapes |

The mix is deliberate. Fold and sort exercise bulk arithmetic and comparison,
record-shapes stresses the evaluator's attribute representation, while
Fibonacci, prime scanning, and N-queens expose evaluator dispatch, thunk
allocation, recursive calls, and list retention. The recursive workloads remain
useful structural regression targets rather than only best-case JIT demonstrations.

## Current results

Measured on aarch64-darwin with the PGO jinx binary against Determinate Nix
2.33.3. Ratios are throughput relative to C++ Nix, so values above 1.00x favor
jinx:

| Workload | C++ Nix | jinx interpreter | jinx JIT | Interpreter / Nix | JIT / Nix |
|---|---:|---:|---:|---:|---:|
| Strict numerical fold | 188.7 ms | 137.4 ms | 96.0 ms | **1.37x** | **1.97x** |
| Stable sort | 132.4 ms | 59.5 ms | 45.2 ms | **2.22x** | **2.93x** |
| Prime scan | 288.8 ms | 221.2 ms | 215.0 ms | **1.31x** | **1.34x** |
| Fibonacci | 739.4 ms | 554.7 ms | 487.5 ms | **1.33x** | **1.52x** |
| N-queens | 397.1 ms | 354.3 ms | 341.6 ms | **1.12x** | **1.16x** |
| Static record shapes | 185.6 ms | 121.7 ms | 110.9 ms | **1.52x** | **1.67x** |

One-shot `/usr/bin/time -lp` measurements put the fused numerical fold at
180–182 MiB peak RSS versus 134 MiB for C++ Nix, down 17.8% from the unfused
jinx parent. Fibonacci uses 239–241 MiB versus Nix's 260 MiB, stable sort uses
49–52 MiB versus 83 MiB, and static-records uses 139–142 MiB versus 154 MiB.
N-queens remains the memory outlier at about 395 MiB versus 198 MiB for C++ Nix.

Every compute workload now favors jinx even with the shipping interpreter.
The JIT ranges from 1.16x C++ Nix on closure/list-heavy N-queens to 2.93x on the
comparison-heavy stable sort. The remaining structural target is still generic
closure/thunk lifetime: production N-queens performs no GC, so its extra RSS is
live or not-yet-collected allocation rather than collector pause overhead.

## Reproduce

```sh
bash bench/pgo-build.sh
bash bench/run-compute-benchmarks.sh
```

Set `ORACLE=/path/to/nix-instantiate` to choose a specific C++ Nix build. The
harness otherwise uses `.oracle/bin/nix-instantiate` when available, then the
`nix-instantiate` on `PATH`. It writes raw hyperfine JSON and provenance to the
local `bench/results/compute/` directory, then regenerates
`bench/graphs/compute.svg`; the generated result directory is not checked in.
