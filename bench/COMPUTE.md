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
| Strict numerical fold | 201.2 ms | 142.8 ms | 99.2 ms | **1.41x** | **2.03x** |
| Stable sort | 145.0 ms | 62.6 ms | 46.4 ms | **2.32x** | **3.12x** |
| Prime scan | 299.7 ms | 230.0 ms | 225.7 ms | **1.30x** | **1.33x** |
| Fibonacci | 769.1 ms | 606.8 ms | 593.0 ms | **1.27x** | **1.30x** |
| N-queens | 457.6 ms | 375.1 ms | 360.6 ms | **1.22x** | **1.27x** |
| Static record shapes | 199.6 ms | 163.8 ms | 117.0 ms | **1.22x** | **1.71x** |

Fresh one-shot `/usr/bin/time -lp` measurements put the fused numerical fold at
179–180 MiB peak RSS versus 134 MiB for C++ Nix. Fibonacci uses 238–241 MiB
versus Nix's 260 MiB, stable sort uses 49–52 MiB versus 83 MiB, and static
records use 139–142 MiB versus 154 MiB. Prime scan uses 201–207 MiB versus
103 MiB, while N-queens remains the memory outlier at about 395 MiB versus
198 MiB. The fold remains 17.8% below its unfused parent in the isolated A/B.

Every compute workload now favors jinx even with the shipping interpreter.
The JIT ranges from 1.27x C++ Nix on closure/list-heavy N-queens to 3.12x on the
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
