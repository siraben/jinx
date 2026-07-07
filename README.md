# jinx

A JIT-compiling, garbage-collected Nix evaluator in Rust, wire-compatible with
[Nix](https://github.com/NixOS/nix) where it counts: byte-identical `.drv` files and
store paths, the daemon worker protocol as a client, and byte-exact CLI output —
including error messages and traces — for the evaluator surface of `nix-instantiate`
and `nix eval` (flakes included).

Built and validated against Nix master (2.36.0pre, worker protocol 1.39) on
aarch64-darwin.

## Status

| Surface | Result |
|---|---|
| Nix language test suite (`tests/functional/lang`, 467 fixtures) | **466 pass / 0 fail / 1 skip** (the skip is disabled upstream), byte-exact stdout+stderr incl. traces |
| Upstream `lang.sh` harness via PATH shim | passes (EXIT 0, incl. inline assertions) |
| nixpkgs derivation parity (readonly) | `hello`, `firefox`, 49-package sample: **byte-identical drv paths** vs C++ Nix |
| NixOS minimal ISO eval (`nixos/release.nix -A iso_minimal.x86_64-linux`) | **byte-identical drv path** (full module system + x86_64-linux from-source bootstrap) |
| Flakes | `jinx eval --raw /path/to/nixpkgs#hello.drvPath` == `nix eval`; `flake.lock` v5–7, path + git+file fetchers, registry; lock *generation* not implemented |
| Store | real writes via `nix-daemon` (AddToStore/FramedSink at protocol 1.38): `.drv` files, `toFile`, `path`/`filterSource`; import-from-derivation triggers builds via `BuildPaths` |
| GC | custom non-moving mark-sweep (32 KiB blocks, bump allocation, precise VM roots + conservative native/JIT stack scan); full suite passes with forced collections every ~4 KB |
| JIT | Cranelift tier: all 40 opcodes lowered, entry-point tiering (**off by default** — a net regression on real nixpkgs evals; `--jit` / `JINX_JIT=1` enables it at threshold 4000 for compute-heavy code, ~1.4× on `fib`); full suite passes with **every chunk compiled**, alone and combined with GC stress |
| Performance vs C++ Nix (see `bench/REPORT.md`) | parse **4.6× faster**; nixpkgs `-A hello` **1.11× faster**, `-A firefox` **parity**, NixOS minimal ISO **1.14× faster**; RSS 1.7–3.2× higher (GC tuned for pauses over footprint; `JINX_GC_HEAP_MB` to trade back) |

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

Knobs: `--jit=on|off` / `JINX_JIT` / `JINX_JIT_THRESHOLD` (JIT off by default);
`JINX_GC_OFF`, `JINX_GC_STRESS`, `JINX_GC_STATS`, `JINX_GC_HEAP_MB` (GC min-trigger 1 GiB).

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
  (or lock-free single flake) is required; github/tarball/network-git fetchers are
  not yet wired.
- Evaluator surface only: `nix build`-style scheduling, substituters, the daemon
  *server*, `nix repl`, and the debugger are out of scope; builds happen via the real
  `nix-daemon` (IFD works this way).
- Eval caching (`~/.cache/nix/eval-cache`) is not implemented.
