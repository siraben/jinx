# jinx benchmark report

**Machine:** Apple M5 Pro (aarch64-darwin), 18 cores, 48 GB RAM, macOS (Darwin 25.3.0)
**jinx:** release build. **JIT is now off by default** (see "JIT policy" below); jit rows
set `JINX_JIT` explicitly.
**Oracle:** C++ Nix built from `/path/to/nix` master (2.36.0pre20260706, cff1f1138)
**Workload source:** `/path/to/nixpkgs` @ 2d62965239ba
**Method:** `bench/run-benchmarks.sh` — hyperfine (warmups + multiple runs; raw JSON in
`bench/results/`), peak RSS via `/usr/bin/time -l`, all evals `--readonly-mode` with
`NIX_REMOTE=dummy://`. Correctness precondition: every workload produces **byte-identical
output** to the oracle (drv paths are text-hashes of drv contents, so path equality
implies content equality), and the full 466-fixture language suite passes — including
under `JINX_GC_STRESS=1` and `JINX_JIT=1 JINX_JIT_THRESHOLD=0`.

This is the **v2** report, after the round-2 perf work (lexer first-byte gating,
`readDir` d_type, interpreter allocation diet, GC min-trigger 1 GiB, JIT default off).
The "v1" columns are the previous committed report's numbers, kept for history.

## Wall time (hyperfine mean ± σ)

| Workload | C++ nix | jinx (default = jit off) | jinx (jit on) | jinx vs nix | v1 jinx (best) | v1 vs nix |
|---|---|---|---|---|---|---|
| parse `all-packages.nix` | 44.3 ± 1.4 ms | **9.6 ± 0.5 ms** | — | **4.6× faster** | 14.3 ms | 2.8× faster |
| `fib.nix` (call/arith micro) | 96.2 ± 3.7 ms | 97.3 ± 3.5 ms | **65.5 ± 3.2 ms** | **1.5× faster** (jit) | 77.3 ms | 1.15× faster |
| `ops.nix` (alloc/list/attr micro) | 47.8 ± 1.2 ms | **23.2 ± 0.7 ms** | 24.9 ± 0.7 ms | **2.1× faster** | 28.1 ms | 1.5× faster |
| nixpkgs `-A hello` | 266.5 ± 5.3 ms | **241.2 ± 3.5 ms** | 246.9 ± 2.1 ms | **1.11× faster** | 458.6 ms | 1.8× slower |
| nixpkgs `-A firefox` | 624.8 ± 14.3 ms | **624.0 ± 3.9 ms** | 666.8 ± 19.0 ms | **parity (1.00×)** | 1269.4 ms | 2.2× slower |
| NixOS minimal ISO (x86_64-linux) | 10.15 ± 0.04 s | **8.92 ± 0.13 s** | 9.30 ± 0.04 s | **1.14× faster** | 12.21 s | 1.3× slower |

Round 2 turned the real-eval story around: v1 was 1.3–2.2× *slower* than C++ nix on
nixpkgs evals; v2 is at parity or faster on all three.

### JIT ablation and default policy (measured on the post-diet interpreter)

`fib.nix` across thresholds vs jit-off: **1.46×** @4000, 1.35× @50000, 1.28× @100000.
Real evals, jit-on @threshold vs jit-off (wall):

| Threshold | hello | firefox | ISO |
|---|---|---|---|
| 4000 (old default) | +2.3% | +3.9% | +4.6% (+368 ms CPU) |
| 50000 | −0.6% (noise) | +0.6% | +1.1% (+94 ms CPU, +2.1%) |
| 100000 | +0.3% | +0.8% | +1.6% (+109 ms CPU) |

Decision rule: keep JIT on only if some threshold holds fib ≥1.3× **and** all real evals
within 1% of jit-off. @50000 keeps fib at 1.35× but ISO is +1.1% wall / +2.1% CPU;
@100000 loses the fib bar (1.28×) and ISO is still +1.6%. **No threshold satisfies both,
so the default is JIT off.** `--jit` / `JINX_JIT=1` re-enable tiering (threshold 4000,
`JINX_JIT_THRESHOLD` to tune) and still deliver the ~1.5× compute win — the tier is a
knob for compute-shaped workloads, not a default tax on evals. The interpreter got fast
enough that Cranelift compile cost (hello ~103 chunks, firefox ~601) no longer pays for
itself on real nixpkgs code, whose time goes to hashing, attrset merges and string
building rather than opcode dispatch.

### GC ablation (`JINX_GC_OFF=1`, never-free, jit on)

hello 250.5 ms, firefox 646.9 ms — within a few percent of GC-on: collection remains
effectively free on these workloads (with the 1 GiB min-trigger, hello and firefox now
perform **zero** collections; ISO performs 2).

## Peak RSS (`/usr/bin/time -l`, single runs)

| Workload | C++ nix (Boehm) | jinx (GC on) | ratio | v1 jinx |
|---|---|---|---|---|
| hello | 138 MiB | 234 MiB | 1.70× | 226 MiB |
| firefox | 357 MiB | 679 MiB | 1.90× | 622 MiB |
| minimal ISO | 1.13 GiB | 3.62 GiB | 3.2× | 3.33 GiB |

The firefox RSS increase (+~37–57 MiB vs v1) is the deliberate cost of raising the GC
min-trigger from 256 MiB to 1 GiB: the collection it skips paused ~35 ms and freed
almost nothing. `JINX_GC_HEAP_MB=256` restores the old behavior (measured: firefox
647 MiB RSS, +26 ms wall). ISO RSS is within run-to-run variance of v1 — its heap
passes 1 GiB regardless, so the same doubling heuristic applies.

## GC statistics (`JINX_GC_STATS=1`, default heuristics)

| Workload | Collections | Total pause | Peak live footprint | v1 collections / pause |
|---|---|---|---|---|
| hello | 0 | 0 ms | — | 1 / 9.1 ms |
| firefox | 0 | 0 ms | — | 3 / 108.6 ms |
| minimal ISO | 2 | 510.0 ms | 1773 MiB | 5 / 610.5 ms |

## Reading the numbers

- **Lexer first-byte gating** (v2): the maximal-munch loop now only runs candidate
  scanners whose first-byte class matches the current byte, with identical
  longest-match/rule-order tie-breaks (validated byte-identical `--parse` vs the oracle
  on the full fixture suite plus 200 random nixpkgs files). Parse is now 4.6× faster
  than C++ nix, and since every eval parses thousands of files, hello/firefox dropped
  ~57/74 ms.
- **`readDir` uses dirent `d_type`** instead of one `lstat` per entry (matching what
  C++ nix does), with an lstat fallback when `d_type` is unknown: hello −32 ms,
  firefox −24 ms of pure syscall elimination.
- **Interpreter allocation diet**: thunks, attrsets, `//` merges and list concats are
  now allocated raw and filled in place (no intermediate `Vec`), and the GC poll moved
  from the dispatch loop top to the allocation sites. Invariant: `gc_check` runs
  *before* each raw allocation while the sources are still rooted, and the bump
  allocator can never collect between carve and fill. Behaviorally gated by the full
  suite under `JINX_GC_STRESS=1` (collection every ~4 KB) plus a complete stress-mode
  `-A hello` eval.
- **The JIT is now an explicit knob** (see policy above): honest ablation showed it
  costs 2–5% on real evals post-diet while still winning 1.5× on compute-shaped code.
- **Memory** remains the clearest cost: ~1.7–1.9× C++ RSS on package evals and 3.2× on
  the ISO, and v2 consciously spends a little more (firefox +~50 MiB) to skip
  low-yield collections. `JINX_GC_HEAP_MB` caps it back down when RSS matters more
  than the last ~30 ms.

## Reproduce

```sh
cargo build --release -p jinx-cli
nix shell nixpkgs#hyperfine -c bash bench/run-benchmarks.sh
```

Raw hyperfine JSON, RSS log, and GC stats for this run are committed under
`bench/results/`.
