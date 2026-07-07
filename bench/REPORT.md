# jinx benchmark report

This is the **v4** report: **round 4** (frameless thunk execution, cheap-eager
classifier, `Thunk0` capture-free thunk packing) plus **wave 5** (the measured
alloc/hash/IO floor: NAR-dump IO engine, attrset-builtin allocation diet, and a
NEON/SSE2 escape scan), and the first **cross-platform validation** on
x86_64-linux. The v3/v2/v1 reports are preserved below as history. Method,
oracle, and correctness preconditions are unchanged from v3 (byte-identical drv
output to the oracle; the full 466-fixture suite green under default,
`JINX_GC_STRESS=1`, `JINX_JIT=1 JINX_JIT_THRESHOLD=0`, and combined).

**Machines:** dev = Apple M-series (aarch64-darwin); x-plat bench = "justin",
AMD Ryzen 9 5950X (x86_64-linux, 32 threads).

## Wall time (hyperfine, PGO binary, interleaved vs the oracle; aarch64-darwin)

| Workload | C++ nix | jinx (default = jit off) | jinx vs nix | v3 vs nix |
|---|---|---|---|---|
| nixpkgs `-A hello` | 227.5 ± 13.6 ms | **192.4 ± 11.8 ms** (user −20%) | **1.18× faster** | 1.17× |
| nixpkgs `-A firefox` | 545.7 ± 18.9 ms | **458.4 ± 24.1 ms** (user −21%) | **1.19× faster** | 1.13× |
| NixOS minimal ISO | 8.68 ± 0.22 s | **7.35 ± 0.64 s** | **1.18× faster** | 1.18× |

Wave 5's headline is **firefox 1.13→1.19×** (user CPU −21% vs the oracle — the
attrset-builtin allocation diet and per-symbol string cache) and a large **ISO
memory** win (below). `parse`/`fib`/`ops` micro-benchmarks are unchanged code
this wave (4.1× / 2.0× jit / 2.2× vs the oracle — see v3).

### Peak RSS — the wave-5 headline (`/usr/bin/time -l`, ISO)

| Workload | C++ nix (Boehm) | jinx pre-wave5 | **jinx v4** | Δ this wave |
|---|---|---|---|---|
| minimal ISO | 1.10 GiB | 3.59 GiB | **2.77 GiB** | **−841 MiB (−23%)** |

P1a (streaming the filtered NAR straight into the hash sink instead of
materializing a 445 MB `Vec<u8>`) plus the attrset allocation diet cut ISO peak
RSS by 841 MiB — from 3.4× the oracle toward ~2.5×. hello/firefox RSS unchanged
(they barely dump NAR).

## Wave 5 — measured plan and attribution

The plan was built from profiles, not assumptions, and it **killed the
cargo-cult SIMD wishlist with evidence**: hardware SHA-256 is already fully
enabled (verified 32 `sha256h` instructions in the ARM binary; SHA-NI on x86 —
sha256 is 0.2% of ISO), and SIMD base32/interner-hash-swap/mmap are all <0.2%
surfaces. The real floor: **NAR-dump filesystem IO = ~48% of the ISO eval**
(3.4 s of syscalls), attrset byte-copying on firefox, and drv-text escaping.

| Item | What | Target | Result |
|---|---|---|---|
| P2 | per-symbol interned string cache (attr-name builtins) | firefox/hello | user −4–9% ff, −9–11% hello (agent A/B) |
| P3 | mapAttrs/zipAttrsWith alloc diet (drop defensive `to_vec`) | firefox/ISO | folds into firefox 1.19× |
| P1a | stream filtered NAR into `HashSink`, kill 445 MB `Vec` | ISO | **−841 MiB RSS** + ISO −6–8% wall |
| P1b | syscall diet: `d_type` dispatch, `openat`+`fstat`, exact reads | ISO | wall-neutral (darwin, warm cache); cuts syscall count |
| P1c | two-phase dump + read-ahead worker pool | ISO | **opt-in** (`JINX_NAR_JOBS`); see below |
| P4 | NEON/SSE2 5-needle escape scan (drv ATerm + toJSON) | ff/hello | in noise (~4% surface); proptested SIMD==scalar |

### P1c: honest verdict — opt-in, not default

The read-ahead pool was the plan's speculative headline (−20–30% ISO wall). It
does **not** deliver that on a warm page cache: measured on **both**
aarch64-darwin and x86_64-linux it gives only **~8% ISO wall** while **tripling
(darwin) / doubling (linux) system CPU** (darwin sys 3.6→12.3 s; justin sys
4.9→9.8 s), because page-cache-warm reads contend on kernel VM locks rather than
scaling across cores. That is a poor default for an interactively-run
evaluator, so the pool ships **gated behind `JINX_NAR_JOBS`** (default serial;
`=N` or `=auto` to enable) — it remains a real win for the cold-cache / CI /
batch regime where workers block on IO instead of spinning. Byte-identical NAR
under both paths, on both architectures.

## Cross-platform: x86_64-linux (justin, AMD Ryzen 9 5950X)

jinx's conservative GC previously scanned callee-saved registers and the thread
stack only on aarch64/darwin. Added a System V x86-64 register-dump (naked asm)
and a Linux `pthread_getattr_np` stack-base probe; **validated on real x86
hardware**:

| Check | Result |
|---|---|
| conformance (default / GC-stress / JIT-stress / combined) | **466/0/1 all four** |
| hello & ISO `.drv` vs oracle (interp + JIT; native ISO build) | **byte-identical** |
| `-A hello` vs C++ nix | **1.39× faster** |
| `-A firefox` vs C++ nix | 1.06× faster (user −11%) |

jinx is now correct, wire-compatible, and faster than C++ Nix on both
aarch64-darwin and x86_64-linux. (The x86 hello lead is larger than darwin's
because C++ nix pays more fixed startup there.)

---

# v3 (history)

**Machine:** Apple M-series (aarch64-darwin), macOS (Darwin 25.3.0)
**jinx:** release build, **PGO-optimized** (`bench/pgo-build.sh`). **JIT is off
by default** (see "JIT policy"); jit rows set `JINX_JIT` explicitly.
**Oracle:** C++ Nix built from `/path/to/nix` master (2.36.0pre, cff1f1138)
**Workload source:** `/path/to/nixpkgs`
**Method:** `bench/run-benchmarks.sh` — hyperfine (warmups + multiple runs, raw
JSON in `bench/results/`) plus interleaved A/B for per-phase deltas, peak RSS
via `/usr/bin/time -l`, all evals `--readonly-mode` with `NIX_REMOTE=dummy://`.
Correctness precondition: every workload produces **byte-identical output** to
the oracle (drv paths are text-hashes of drv contents, so path equality implies
content equality), and the full 466-fixture language suite passes — including
under `JINX_GC_STRESS=1`, `JINX_JIT=1 JINX_JIT_THRESHOLD=0`, and both combined.

This is the **v3** report, after the round-3 performance work (a six-phase plan:
interpreter P0, GC flat-locate, JIT quality, generational GC, and PGO; the
bindings-SoA phase was deferred — see notes). The "v2" report is preserved
below as history. Because the machine can carry background load, per-phase
deltas are reported from **interleaved A/B** runs and lean on **user (CPU)
time**, which is far less load-sensitive than wall time.

## Wall time (hyperfine, interleaved vs the oracle; PGO binary)

| Workload | C++ nix | jinx (default = jit off) | jinx (jit on) | jinx vs nix | v2 vs nix |
|---|---|---|---|---|---|
| parse `all-packages.nix` | 37.4 ± 1.0 ms | **9.2 ± 0.2 ms** | — | **4.1× faster** | 4.6× faster |
| `fib.nix` (call/arith micro) | ~94 ms | 84.1 ± 2.7 ms | **47.5 ± 4.3 ms** | **2.0× faster** (jit) | 1.5× (jit) |
| `ops.nix` (alloc/list/attr micro) | ~44 ms | **20.3 ± 1.6 ms** | 16.8 ± 0.9 ms | **2.2× faster** (2.6× jit) | 2.1× faster |
| nixpkgs `-A hello` | 220.9 ± 5.5 ms | **189.5 ± 4.4 ms** | ~parity | **1.17× faster** | 1.11× faster |
| nixpkgs `-A firefox` | 526.9 ± 6.2 ms | **467.9 ± 7.6 ms** | +2% (regress) | **1.13× faster** | parity (1.00×) |
| NixOS minimal ISO (x86_64-linux) | 9.05 ± 0.62 s | **7.66 ± 0.31 s** | +6% (regress) | **1.18× faster** | 1.14× faster |

Round 3 moved every real-eval workload further ahead of C++ nix: hello
1.11→1.17×, firefox parity→1.13×, ISO 1.14→1.18×. `parse` is unchanged code
(the lexer/parser were not touched this round); its 4.1× here vs v2's 4.6×
reflects a faster oracle build / different machine, not a jinx regression
(jinx parse is 9.2 ms, matching v2's 9.6 ms).

### Total speedup vs the pre-perf baseline (this round)

Interleaved A/B of the final PGO binary against a fresh build of the pre-round-3
commit, **user (CPU) time** (load-robust):

| Workload | pre-round-3 -> v3 (PGO) user time | wall (clean runs) |
|---|---|---|
| hello | -6% | ~1.05x |
| firefox | **-23%** | up to ~1.3x (noisy) |
| ISO | -14% (7.25->6.78 s) | 1.07x |

## Per-phase attribution (interleaved A/B, user time unless noted)

| Phase | Item | hello | firefox | ISO | notes |
|---|---|---|---|---|---|
| 1 | interpreter P0 (intersectAttrs asym search, Op::Call inline arg buffer, hoist frame constants) | -3.6% | -5.7% | (folds into below) | zero-risk interpreter wins on the shipping default |
| 2 | GC flat O(1) locate (contiguous reservation, no hashmap on the mark path) | ~0 | ~0 | -5.3% | hello/firefox do 0 collections at the 1 GiB trigger, so the win is on the GC-heavy ISO trace path |
| 3 | JIT quality (verifier-off, frame-entry ABI, Force/tag fast paths, monomorphic Select inline cache, background compile) | parity | +2% (jit on) | +0.6% (jit on) | **fib 1.5->1.77x**; kept **JIT off by default** (see policy) |
| 4 | generational GC + write barrier (tuned young-trigger) | ~0 | ~0 | wall 1.03-1.06x faster, **max GC pause -57%** | ISO peak RSS +~390 MiB (sticky-mark floating garbage); **stress gate ~8-9x cheaper** |
| 5 | bindings SoA layout | — | — | — | **deferred** (no proven artifact to port; see notes) |
| 6 | PGO (train on parse/fib/ops/hello/firefox/ISO, `-Cprofile-use`) | -4.7% | **-9.8%** | ~-13% | final build step; profile committed at `bench/jinx.profdata` |

### JIT ablation and default policy

With all of Phase 3 landed (including background compilation on by default when
the JIT is active), `fib.nix` is **1.77x faster** with `JINX_JIT=1` than the
interpreter, and `ops.nix` 1.21x. On real evals, jit-on vs jit-off (PGO binary):

| Workload | jit-off (default) | jit-on | delta |
|---|---|---|---|
| hello | 189-198 ms | ~parity | +/-0% |
| firefox | 495-527 ms | +2% wall / +5% user | regress |
| ISO | 6.96-7.66 s | +0.6% wall / +2% user | regress |

Background JIT compilation (worker thread, `AtomicPtr` entry publish observed
Acquire/Release; the `!Sync` per-chunk counter is only ever touched by the eval
thread) nearly **halved** the firefox jit-on regression (v2 was +6.8%, now +2%)
by moving Cranelift compile cost off the critical path — but did not erase it.
Real nixpkgs evals spend their time on hashing, attrset merges and string
building rather than opcode dispatch, so the ~601 firefox chunks the tier
compiles still don't repay their compile CPU on a one-shot eval.

**Decision: JIT stays OFF by default.** No configuration is parity-or-better on
*all* real evals (firefox/ISO still regress >1% user), so `--jit` / `JINX_JIT=1`
remains an opt-in knob (threshold 4000, `JINX_JIT_THRESHOLD` to tune;
`JINX_JIT_BG=0` forces synchronous compile) that delivers the ~1.8x compute win
on compute-shaped code without taxing evals. The honest ceiling: on this
interpreter, Cranelift is a compute-kernel accelerator, not a general eval win.

## GC: pauses, generations, and RSS

The heap is now a **sticky-mark generational** collector over a single
contiguous reservation (flat O(1) `locate`). Minor collections trace from a
remembered set (old cells mutated since the last GC, logged by a write barrier
at the single `vm::set_b` choke point) plus roots and sweep only young blocks;
majors run on the first GC, at a 2x-retained watermark, every 8th collection
under stress, or with `JINX_GC_GEN=0`.

| Workload | Collections (v3) | Total pause | Max pause | Peak footprint | v2 collections / pause |
|---|---|---|---|---|---|
| hello | 0 | 0 ms | 0 ms | — | 0 / 0 ms |
| firefox | 0 | 0 ms | 0 ms | — | 0 / 0 ms |
| minimal ISO | 3 (1 major + 2 minor) | 297 ms | **122.7 ms** | 2295 MiB | 2 / 510 ms |

The generational split cut the ISO **max pause** (the user-visible latency
spike) to 122.7 ms — roughly **-57%** vs the non-generational policy
(`JINX_GC_GEN=0` measures ~285 ms max on the same binary) — and total pause
297 ms vs v2's 510 ms. The production `young_trigger` default tracks
`min_trigger` (1 GiB) rather than a small REPL-style young generation, because
on a one-shot batch evaluator an aggressive young gen over-collects a
monotonically growing heap; the 256 MiB default regressed ISO +11% user, while
`young = min_trigger` makes ISO neutral-to-better. The stress gate is untouched
by that default (it triggers on the 4 KiB `min_trigger`), so
`JINX_GC_STRESS=1 cargo run -p jinx-conformance` is now **~8-9x cheaper**
(stress `-A hello` ~15 s vs ~138 s), a real developer-workflow win.

### Peak RSS (`/usr/bin/time -l`, single runs)

| Workload | C++ nix (Boehm) | jinx (GC on) | ratio | v2 jinx |
|---|---|---|---|---|
| hello | 137 MiB | 234 MiB | 1.70x | 234 MiB |
| firefox | 357 MiB | 678 MiB | 1.90x | 679 MiB |
| minimal ISO | 1.10 GiB | 3.74 GiB | 3.4x | 3.62 GiB |

hello/firefox RSS is unchanged from v2. The ISO peak RSS rose ~390 MiB
(3.2->3.4x): the honest cost of sticky-mark generational collection, whose
minors leave old-generation floating garbage between the (rarer) majors.
`JINX_GC_GEN=0` restores the non-generational policy and the lower ISO RSS
(at the cost of the max-pause and stress-gate wins); `JINX_GC_HEAP_MB` still
caps the trigger for RSS-sensitive runs.

## What landed, what didn't

- **Landed (one commit each):** Phase 1 interpreter P0 (x3), Phase 2 GC
  flat-locate, Phase 3 JIT quality (verifier-off, frame-ABI + fast paths +
  Select cache, background compile), Phase 4 generational GC, Phase 6 PGO.
- **JIT default:** re-measured and kept **off** (real evals still regress with
  it on; fib/ops win preserved as a knob).
- **Deferred — Phase 5 bindings SoA:** unlike the other phases it had **no
  proven worktree implementation to port** (only a standalone microbench), so
  it would be fresh from-scratch R&D across ~41 attrs consumers + the GC tracer
  + the JIT Select inline-cache codegen, with real byte-exactness risk. Its
  stated wins are also largely already captured here by Phase 1's algorithmic
  intersectAttrs and Phase 3's monomorphic Select cache, so the marginal
  integrated benefit did not justify destabilizing a green tree.
- **Not attempted (measured dead-ends from the plan):** fat LTO, target-cpu
  native, panic=abort, BOLT (Mach-O), NaN-boxing, bump-alloc fast path, lower
  JIT thresholds, the pos-drop side-table and incremental SATB marking.

## Reproduce

```sh
# stock release
cargo build --release -p jinx-cli
# OR PGO build (the numbers above): instrument -> train -> merge -> rebuild
bash bench/pgo-build.sh
# benchmark suite vs the C++ oracle
nix shell nixpkgs#hyperfine -c bash bench/run-benchmarks.sh
```

Raw hyperfine JSON, the RSS log, and GC stats for this run are committed under
`bench/results/`.

---

# v2 (history)

**jinx:** release build. **JIT off by default** (round 2); jit rows set
`JINX_JIT` explicitly. This is the round-2 report (lexer first-byte gating,
`readDir` d_type, interpreter allocation diet, GC min-trigger 1 GiB, JIT default
off).

## Wall time (hyperfine mean +/- sigma)

| Workload | C++ nix | jinx (default = jit off) | jinx (jit on) | jinx vs nix | v1 jinx (best) | v1 vs nix |
|---|---|---|---|---|---|---|
| parse `all-packages.nix` | 44.3 +/- 1.4 ms | **9.6 +/- 0.5 ms** | — | **4.6x faster** | 14.3 ms | 2.8x faster |
| `fib.nix` (call/arith micro) | 96.2 +/- 3.7 ms | 97.3 +/- 3.5 ms | **65.5 +/- 3.2 ms** | **1.5x faster** (jit) | 77.3 ms | 1.15x faster |
| `ops.nix` (alloc/list/attr micro) | 47.8 +/- 1.2 ms | **23.2 +/- 0.7 ms** | 24.9 +/- 0.7 ms | **2.1x faster** | 28.1 ms | 1.5x faster |
| nixpkgs `-A hello` | 266.5 +/- 5.3 ms | **241.2 +/- 3.5 ms** | 246.9 +/- 2.1 ms | **1.11x faster** | 458.6 ms | 1.8x slower |
| nixpkgs `-A firefox` | 624.8 +/- 14.3 ms | **624.0 +/- 3.9 ms** | 666.8 +/- 19.0 ms | **parity (1.00x)** | 1269.4 ms | 2.2x slower |
| NixOS minimal ISO (x86_64-linux) | 10.15 +/- 0.04 s | **8.92 +/- 0.13 s** | 9.30 +/- 0.04 s | **1.14x faster** | 12.21 s | 1.3x slower |

Round 2 turned the real-eval story around: v1 was 1.3-2.2x *slower* than C++ nix
on nixpkgs evals; v2 reached parity or faster on all three. v2 peak RSS: hello
234 MiB (1.70x), firefox 679 MiB (1.90x), ISO 3.62 GiB (3.2x). v2 GC (default):
hello 0 / 0 ms, firefox 0 / 0 ms, ISO 2 collections / 510 ms total pause. The
JIT was an explicit knob costing 2-5% on real evals post-diet while winning
~1.5x on compute; the default was off.
