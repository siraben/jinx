# jinx

A JIT-compiling, garbage-collected Nix evaluator in Rust, wire-compatible with
[Nix](https://github.com/NixOS/nix) where it counts: byte-identical `.drv` files and
store paths, the daemon worker protocol as a client, and byte-exact evaluation output —
including error messages and traces — across the entire language conformance corpus, for
the evaluator surface of `nix-instantiate` and `nix eval` (flakes included).

"Byte-exact" is measured against all 467 upstream `tests/functional/lang` fixtures and
derivation outputs, not asserted globally: CLI-surface behaviors outside that corpus that
still differ are enumerated honestly in [`KNOWN_DIVERGENCES.md`](KNOWN_DIVERGENCES.md)
(mostly upstream position quirks, out-of-scope fetchers, and library-driven parse-error
wording). Correctness was hardened by two rounds of adversarial differential review.

Built and validated against Nix master (2.36.0pre, worker protocol 1.39) on
aarch64-darwin.

## Status

| Surface | Result |
|---|---|
| Nix language test suite (`tests/functional/lang`, 467 fixtures) | **466 pass / 0 fail / 1 skip** (the skip is disabled upstream), byte-exact stdout+stderr incl. `--show-trace` traces |
| Upstream `lang.sh` harness via PATH shim | passes (EXIT 0, incl. inline assertions) |
| nixpkgs derivation parity (readonly) | `hello`, `firefox`, 49-package sample: **byte-identical drv paths** vs C++ Nix |
| NixOS minimal ISO eval (`nixos/release.nix -A iso_minimal.x86_64-linux`) | **byte-identical drv path** (full module system + x86_64-linux from-source bootstrap) |
| Flakes | `jinx eval --raw /path/to/nixpkgs#hello.drvPath` == `nix eval`; `flake.lock` v5–7, path + git+file fetchers, registry; lock *generation* not implemented |
| Store | real writes via `nix-daemon` (AddToStore/FramedSink at protocol 1.38): `.drv` files, `toFile`, `path`/`filterSource`; import-from-derivation triggers builds via `BuildPaths` |
| GC | custom non-moving **sticky-mark generational** mark-sweep (32 KiB blocks over one contiguous reservation with O(1) locate, precise VM roots + conservative native/JIT stack scan, write barrier at a single cell-mutation choke point); minor/major policy with `JINX_GC_GEN=0` escape hatch; full suite passes with forced collections every ~4 KB |
| JIT | Cranelift tier: all 40 opcodes lowered, entry-point tiering with **background compilation** (worker thread, on by default when the JIT is active; `JINX_JIT_BG=0` for synchronous). Tiering is **off by default** — still a small regression on real nixpkgs evals; `--jit` / `JINX_JIT=1` enables it at threshold 4000 for compute-heavy code, **~1.8× on `fib`**; full suite passes with **every chunk compiled**, alone and combined with GC stress |
| Performance vs C++ Nix (see `bench/REPORT.md`) | PGO build: parse **~4.1× faster**; nixpkgs `-A hello` **1.18× faster**, `-A firefox` **1.19× faster**, NixOS minimal ISO **1.18× faster**; ISO peak RSS cut **−841 MiB** in wave 5 (2.77 GiB). Cross-platform: x86_64-linux validated on AMD Ryzen (conformance 466/0/1, byte-identical drv, hello **1.39× faster**). `JINX_GC_HEAP_MB` / `JINX_GC_GEN=0` trade RSS back; `JINX_NAR_JOBS` opts into parallel NAR IO for cold-cache/CI |

## Layout

- `crates/jinx-syntax` — hand-written lexer/parser mirroring Nix's grammar, byte-exact
  `--parse` pretty-printer and parse-error strings
- `crates/jinx-eval` — GC heap, bytecode compiler (flat closures/upvalues), VM,
  all builtins, string contexts, printers (value/XML/JSON/TOML), POSIX-ERE regex engine
- `crates/jinx-jit` — Cranelift codegen for hot chunks (shares the interpreter's frame
  layout; transparent fallback)
- `crates/jinx-store` — store-path math, derivation ATerm + `hashDerivationModulo`,
  NAR, daemon worker-protocol client
- `crates/jinx-fetch`, `crates/jinx-flake` — fetchers (path, git+file), `flake.lock`,
  flakerefs, registry
- `crates/jinx-cli` — the `jinx` binary: `nix-instantiate` personality (default) and
  `jinx eval` (`nix eval` personality)
- `crates/jinx-conformance` — parallel runner replicating upstream `lang.sh` semantics

## Usage

```sh
cargo build --release -p jinx-cli

# nix-instantiate personality
./target/release/jinx --readonly-mode /path/to/nixpkgs -A hello
./target/release/jinx --eval --strict -E '1 + 1'

# nix eval personality (flakes)
./target/release/jinx eval --extra-experimental-features 'nix-command flakes' \
  --raw /path/to/nixpkgs#hello.drvPath
```

Knobs: `--jit=on|off` / `JINX_JIT` / `JINX_JIT_THRESHOLD` (JIT off by default),
`JINX_JIT_BG=0` (disable background compilation); `JINX_GC_OFF`, `JINX_GC_STRESS`,
`JINX_GC_STATS`, `JINX_GC_HEAP_MB` (GC min-trigger 1 GiB), `JINX_GC_GEN=0`
(disable generational collection), `JINX_GC_YOUNG_MB` (young-gen trigger).

For the benchmark numbers above, build with PGO: `bash bench/pgo-build.sh`
(instrument → train → merge → rebuild; see `bench/REPORT.md`).

## Conformance & benchmarks

```sh
# language suite (expects the Nix source tree at /path/to/nix)
cargo run -q -p jinx-conformance -- --engine ./target/release/jinx

# strictest correctness gate: every chunk JIT-compiled + GC every ~4 KB
JINX_JIT=1 JINX_JIT_THRESHOLD=0 JINX_GC_STRESS=1 \
  cargo run -q -p jinx-conformance -- --engine ./target/release/jinx

# benchmark suite vs the C++ oracle (see bench/REPORT.md for results)
nix shell nixpkgs#hyperfine -c bash bench/run-benchmarks.sh
```

## Known limitations

- Flake lock **generation** (`lockFlake`) is not implemented — an on-disk `flake.lock`
  (or lock-free single flake) is required; `github:`/tarball/network-git fetchers are
  not yet wired (flake inputs of type `path` and `git+file` are).
- `builtins.fetchGit` and `builtins.storePath` are not implemented.
- Under `--readonly-mode` / `dummy://`, store-path *validity* checks are approximated by
  filesystem checks, so some not-yet-built-path errors C++ raises do not fire identically
  (real builds via the daemon are byte-identical).
- Evaluator surface only: `nix build`-style scheduling, substituters, the daemon
  *server*, `nix repl`, and the debugger are out of scope; builds happen via the real
  `nix-daemon` (IFD works this way).
- Eval caching (`~/.cache/nix/eval-cache`) is not implemented.

See [`KNOWN_DIVERGENCES.md`](KNOWN_DIVERGENCES.md) for the full, itemized list of
observable differences outside the conformance corpus (upstream quirks, platform-specific
rendering, and library-driven parse-error wording).

## License & provenance

LGPL-2.1-or-later (see `COPYING`), matching upstream Nix. jinx is an independent
reimplementation whose semantics were ported by reading the
[NixOS/nix](https://github.com/NixOS/nix) sources; a few Nix-language files
(`derivation.nix`, `call-flake.nix`) are vendored verbatim from that repository.
Conformance fixtures live in the upstream repo and are read at test time (expected at
`/path/to/nix`), not vendored.
