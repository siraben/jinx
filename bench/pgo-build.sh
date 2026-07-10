#!/usr/bin/env bash
# PGO build recipe for jinx (measured: -7..-13% wall vs stock release on
# hello/firefox/ISO/fib, aarch64-darwin, rustc 1.95 / LLVM 21).
#
# Requirements: the nix-provided cargo/rustc (LLVM 21) and an llvm-profdata
# with a matching profraw version. On macOS the Xcode one works:
#   xcrun llvm-profdata   (Apple LLVM 21 reads rustc-LLVM-21 profraw)
# On Linux use nixpkgs#llvmPackages_21.libllvm (bin/llvm-profdata):
#   PROFDATA="$(nix build --print-out-paths nixpkgs#llvmPackages_21.libllvm)/bin/llvm-profdata"
#
# Usage: bash bench/pgo-build.sh
# Produces target/release/jinx (PGO-optimized). Training data in $PGO_DIR;
# the merged profile is written to bench/jinx.profdata for reproducibility.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NIXPKGS="${NIXPKGS:?set NIXPKGS to a nixpkgs checkout}"
PGO_DIR="${PGO_DIR:-$ROOT/target/pgo-data}"
PROFDATA="${PROFDATA:-xcrun llvm-profdata}"

export NIX_REMOTE=dummy:// NIX_STORE_DIR=/nix/store

echo "== 1/3 build instrumented"
rm -rf "$PGO_DIR"
RUSTFLAGS="-Cprofile-generate=$PGO_DIR" cargo build --release -p jinx-cli
J="$ROOT/target/release/jinx"

echo "== 2/3 train (parse, fib jit-off/on, ops, hello, firefox, ISO)"
"$J" --parse "$NIXPKGS/pkgs/top-level/all-packages.nix" >/dev/null
"$J" --readonly-mode --eval --strict "$ROOT/bench/fib.nix" >/dev/null
JINX_JIT=1 "$J" --readonly-mode --eval --strict "$ROOT/bench/fib.nix" >/dev/null
"$J" --readonly-mode --eval --strict "$ROOT/bench/ops.nix" >/dev/null
"$J" --readonly-mode "$NIXPKGS" -A hello >/dev/null
"$J" --readonly-mode "$NIXPKGS" -A firefox >/dev/null
"$J" --readonly-mode "$NIXPKGS/nixos/release.nix" -A iso_minimal.x86_64-linux >/dev/null

echo "== 3/3 merge + rebuild optimized"
$PROFDATA merge -o "$ROOT/bench/jinx.profdata" "$PGO_DIR"/*.profraw
RUSTFLAGS="-Cprofile-use=$ROOT/bench/jinx.profdata" cargo build --release -p jinx-cli
echo "PGO binary: $ROOT/target/release/jinx"
echo "Profile:    $ROOT/bench/jinx.profdata"
