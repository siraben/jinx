# Performance report

This report is the durable performance record for jinx. It replaces the
chronological experiment logs with the current published results, the isolated
evidence behind retained changes, and the reasons rejected candidates stayed
out. [`COMPUTE.md`](COMPUTE.md) and [`STRENGTHS.md`](STRENGTHS.md) contain the
focused compute and evaluator-strength suites.

## Measurement policy

- Every evaluator benchmark first requires byte-identical output from jinx and
  the C++ Nix oracle. Derivation workloads additionally require identical `.drv`
  paths.
- Runtime candidates are compared with their exact parent. Small control-flow
  changes invalidate old PGO profiles, so final decisions use a freshly trained
  candidate and baseline on the same corpus.
- Hyperfine alternates commands after explicit warmups. `/usr/bin/time -lp`
  supplies peak RSS, retired instructions, cycles, and user/system CPU.
- Wall time is load-sensitive. Instruction counts and repeated RSS series are
  used to decide close results; a synthetic win alone is not presented as a
  general evaluator win.
- The general suite uses the shipping configuration: PGO enabled and JIT off.
  The JIT is reported separately on compute-dense workloads.

The checked-in general results were recorded on Apple arm64 Darwin against
Determinate Nix 2.33.3 and nixpkgs `9675111`. The jinx binary was built from the
final audited tree with a freshly trained profile after restoring 128-byte
recycling lines and simplifying the NAR walker.

## Current realistic results

| Workload | C++ Nix | jinx | Speedup |
|---|---:|---:|---:|
| Parse `all-packages.nix` | 52.2 ms | **6.8 ms** | **7.72x** |
| nixpkgs `hello` | 226.5 ms | **153.6 ms** | **1.47x** |
| nixpkgs `firefox` | 747.5 ms | **489.7 ms** | **1.53x** |
| NixOS minimal ISO | 7.213 s | **4.910 s** | **1.47x** |

These are end-to-end evaluations, not parser or opcode microbenchmarks. The
ISO workload includes substantial filesystem and NAR work; `hello` and Firefox
exercise the evaluator without collecting in the measured configuration.

| Workload | C++ Nix peak RSS | jinx peak RSS | jinx / Nix |
|---|---:|---:|---:|
| `hello` | 136 MiB | **159 MiB** | 1.17x |
| `firefox` | 405 MiB | **520 MiB** | 1.29x |
| NixOS minimal ISO | 1.73 GiB | **1.82 GiB** | 1.05x |

Jinx intentionally trades resident memory for stable object addresses, cheap
call-by-need updates, and throughput. The compute suite shows where later
representation work pays back that memory: Fibonacci and static-record
workloads use less RSS than the oracle, while closure/list-heavy N-queens
remains roughly 395 MiB versus 198 MiB. See [`COMPUTE.md`](COMPUTE.md) for the
six current kernels and raw ratios.

## Profile and allocation shape

The representative ISO profile attributed 28.8% of running samples to
interpreter dispatch, 14.2% to forcing, 6.5% to `memmove`, 4.2% to data tracing,
and 4.1% to calls. Firefox had the same shape with more attribute work: 23.3%
dispatch, 12.2% `memmove`, 12.2% forcing, 4.3% logical attrset materialization,
and 3.2% merge.

ISO executed about 216 million bytecodes and allocated 63 million value cells.
It created roughly 38 million thunk objects, 3.3 million flat binding objects,
and 617,000 binding layers. Allocation, capture construction, dispatch, and
forcing dominate; collector pause is material for ISO memory but cannot explain
the zero-collection `hello` and Firefox runs.

The store/path half of ISO is syscall-heavy. The measured run issued about
98,000 `openat`, 98,000 `fstatat`, 67,000 reads, and 39,000 directory-read calls.
This is why eliminating redundant metadata queries moved wall time while hash
sink and directory-container tuning did not.

## Retained changes and isolated evidence

| Change | Isolated evidence | Decision |
|---|---|---|
| Reuse NAR walker metadata | ISO wall -2.9%, system CPU -3.8%; path-filter `lstat` samples fell from 546 ms to 14 ms | Keep |
| Stream the serial NAR dump in one pass | Filtered NAR 2–3% faster; ISO wall -2.9%, system CPU -5.8% | Keep |
| Bounded small-right attrset layers | Peak RSS: `hello` -11.7%, Firefox -13.9%, ISO -4.7%; ISO avoided about 251 MiB of entry copies | Keep |
| Precise allocation safepoints plus 128-byte data-line recycling | ISO RSS 2.98 -> 2.78 GB (-6.7%), 239 MiB reused, one fewer collection | Keep |
| Typed WHNF force checks | Firefox cycles -2.9%; ISO cycles -2.0%, instructions -1.4% | Keep |
| Fixed two-slot parser lookahead | Parse instructions -9.0%; Firefox -2.3%, ISO -2.1% | Keep |
| Exact frame stack reservation | Firefox cycles -3.4%; ISO cycles -1.4% | Keep |
| Shared static attrset shapes | ISO RSS -3.69%, instructions -0.81% | Keep for memory |
| Packed zero/one-capture thunks and closures | ISO RSS -11.55%; Firefox -6.42% | Keep |
| Inline context-free strings up to 14 bytes | ISO RSS -3.86%; CPU mixed/noisy | Keep for memory |
| Stable grouping for `zipAttrsWith` | ISO cycles -0.89% to -1.15%; median wall -1.22% | Keep |
| Recursive-only `deepSeq` tracking | Wide/shared/cyclic bench: instructions -19.2%, cycles -31.1%, RSS -22.1%; representative suite neutral | Keep, localized |
| Primitive-key `genericClosure` hashing | 20,001-key bench instructions -38.45%; representative suite within 0.07% | Keep, localized |
| Bounded exact-prefix environment sharing | Avoids 83.4 MB of ISO capture payload; repeated RSS series save 31–67 MB, at a 0.7–1.5% instruction cost | Keep, memory-first |
| Direct store-path rendering | Five-million-print bench instructions/cycles -75.8%; ISO instructions -0.83% | Keep, localized |
| Equivalent flake-input deduplication | Dedicated workload: evaluations 2 -> 1, instructions -46.8%, cycles -41.6%, RSS -37.9% | Keep, localized |
| Static-shape equality metadata | Equal-record bench instructions -6.1%; interpreter wall -8.2%, JIT wall -6.2% | Keep |
| Integer multiply/bitwise JIT paths | Fold instructions -9.5%; stable-sort instructions -2.5% | Keep in opt-in JIT |
| Bounded `genList`/strict-fold deforestation | Million-element fold RSS -17.8%, instructions -1.26%, JIT wall -4.6% | Keep, compiler-proven cases only |

The four-byte verified-slot select cache records a 98.7% hit rate on ISO and
removes object identity from the representation. Its isolated realistic delta
was small (about -0.24% instructions and 6 MiB RSS), so campaign-only counters
were removed; the compact cache remains because it is simpler than the former
16-byte object-keyed cache and does not add a general polymorphic structure.

## Rejected and removed work

| Candidate | Evidence | Outcome |
|---|---|---|
| Cached NAR filter type strings | Instructions +0.14%, cycles +0.73%, RSS -0.21% | Removed: below 1% |
| 64-byte recycler refinement | Instructions -0.10%, cycles -0.84%, RSS -0.56% | Removed: retain simpler 128-byte lines |
| Campaign opcode/allocation metrics | Useful once for attribution; no production consumer or runtime benefit | Removed after recording results |
| Two-phase and opt-in parallel NAR walkers | Compact hardened one-pass replacement removed 458 lines while retaining descriptor-relative, no-follow traversal. Its portable prototype changed ten alternating ISO pairs by wall +0.61%, instructions +0.33%, cycles +0.22%, RSS -0.11%; filtered-NAR wall time was neutral despite about +2.3% hardware counters | Removed: realistic cost below 1% did not justify the machinery |
| Direct attr-layer heap writer | Firefox instructions +9.1% for RSS -0.4% | Reject |
| Lazy force-root publication | ISO instructions -0.61%, Firefox +0.68% and cycles +2.85% | Reject |
| Cached recycled-cell bitmap word | Instructions -0.97%, cycles +1.32%, user CPU +1.5% | Reject |
| Buffered NAR hashing | ISO wall/user time regressed about 4% | Reject |
| Vector-plus-sort directory enumeration | Instructions +0.21%, cycles +0.47% | Reject |
| Bounded deletion masks | Only 21 ISO masks and 36 KiB avoided; instructions +0.63% | Reject |
| Bounded list-concat nodes | Only 395 ISO nodes and 0.67 MiB avoided; representative CPU regressed | Reject |
| Frozen post-parse attr builders | Parse RSS +1.6%, instructions +0.8–1.1%; ISO neutral | Reject |
| Attrset-needle `elem` specialization | Only 1,354 of 111,833 calls eligible; deltas mixed and below 0.3% | Reject |
| Rooted source-position cache | Hot-position bench -13.6% instructions, but Firefox RSS crossed a 4 MiB commitment boundary | Reject |
| Selector thunks | Firefox found 48,059 uses, but PGO ISO wall regressed 6.45% | Reject |
| Payload-identity equality | Would skip observable forcing and errors | Semantically invalid |
| Global reference counting or allocation-time hash-consing | Taxes every graph edge/table lookup and does not fit mutable cyclic thunk graphs | Reject structurally |

## GC and representation conclusions

Stable pointers to 16-byte `Value` cells remain the right base representation.
Forcing overwrites a thunk cell in place, so all aliases share memoized results,
blackholes, and errors without a side table. Variable payloads remain separate,
allowing recursive graphs and a non-moving collector.

Bounded attrset layering succeeds because `//` is asymmetric: small patches are
usually applied to much larger sorted sets. Sharing the left object turns an
eager O(left + right) copy into O(right) allocation. The depth-eight and
right-size bounds are essential; consumers that need full ordered iteration
still flatten into cache-friendly slices.

The collector reuses dead value cells and complete empty 128-byte data lines
inside surviving blocks. Precise VM safepoints cover execution-engine state;
Rust builtin temporaries retain the conservative native-stack fallback. A
moving nursery would require rewriting every stable `VRef`, JIT cache, and
in-place thunk alias. Global reference counting has the same mismatch with
recursive, mutable, cyclic graphs.

Cross-run memoization needs stable computation identities and complete
filesystem, environment, store, and import dependencies. Code pointer plus
captures is unsound. Coarse demand-shaped caches, such as compiled imports and
the existing search evaluation cache, remain better targets than retaining
arbitrary dynamic heaps.

## Validation

The workspace test suite and language corpus are required after integrated
changes. The strict gate combines threshold-zero JIT compilation, forced GC,
and heap verification; the published campaign passed 466/0/1.

```sh
cargo test --workspace

JINX_JIT=1 JINX_JIT_THRESHOLD=0 JINX_JIT_BG=0 \
JINX_GC_STRESS=1 JINX_GC_VERIFY=1 \
cargo run -q -p jinx-conformance -- \
  --engine ./target/release/jinx \
  --corpus /path/to/nix/tests/functional
```

## Reproduce

```sh
# Train and build the same seven-workload PGO configuration.
NIXPKGS=/path/to/nixpkgs bash bench/pgo-build.sh

# General real evaluations, RSS/GC logs, and graphs.
NIXPKGS=/path/to/nixpkgs bash bench/run-benchmarks.sh
python3 bench/plot.py

# Focused parity-checked suites.
bash bench/run-compute-benchmarks.sh
NIXPKGS=/path/to/nixpkgs bash bench/run-eval-strengths.sh
```

Set `ORACLE=/path/to/nix-instantiate` to choose the C++ Nix build explicitly.
The harnesses record binary paths, commits, system information, oracle version,
and nixpkgs revision beside their raw JSON. Re-run on a quiet machine and do
not compare candidates trained with different or stale PGO profiles.
