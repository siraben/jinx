#!/usr/bin/env bash
# M8 benchmark harness: jinx (jit on/off, gc on/off) vs C++ nix master oracle.
# Usage: nix shell nixpkgs#hyperfine -c bash bench/run-benchmarks.sh [outdir]
# Produces per-benchmark hyperfine JSON + peak-RSS table in $OUTDIR.
set -euo pipefail

JINX_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
JINX="$JINX_ROOT/target/release/jinx"
ORACLE="$JINX_ROOT/.oracle/bin/nix-instantiate"
NIXPKGS="$NIXPKGS"
OUTDIR="${1:-$JINX_ROOT/bench/results}"
mkdir -p "$OUTDIR"

export NIX_REMOTE=dummy:// NIX_STORE_DIR=/nix/store

command -v hyperfine >/dev/null || { echo "hyperfine not on PATH (use: nix shell nixpkgs#hyperfine -c bash $0)"; exit 1; }
[ -x "$JINX" ] || { echo "build first: cargo build --release -p jinx-cli"; exit 1; }
[ -x "$ORACLE" ] || { echo "oracle missing at $ORACLE"; exit 1; }

hf() { # name runs warmups cmds...
  local name=$1 runs=$2 warm=$3; shift 3
  echo "=== $name"
  hyperfine --warmup "$warm" --runs "$runs" --export-json "$OUTDIR/$name.json" "$@"
}

rss() { # name cmd...
  local name=$1; shift
  local out
  out=$(/usr/bin/time -l "$@" 2>&1 >/dev/null | awk '/maximum resident set size/ {print $1}')
  echo "$name $out" | tee -a "$OUTDIR/rss.txt"
}

: > "$OUTDIR/rss.txt"

# --- parse-only: biggest single file in nixpkgs
hf parse 20 3 \
  "$ORACLE --parse $NIXPKGS/pkgs/top-level/all-packages.nix" \
  "$JINX --parse $NIXPKGS/pkgs/top-level/all-packages.nix"

# --- compute microbench (JIT showcase)
hf fib 10 3 \
  "$ORACLE --readonly-mode --eval --strict $JINX_ROOT/bench/fib.nix" \
  "JINX_JIT=0 $JINX --readonly-mode --eval --strict $JINX_ROOT/bench/fib.nix" \
  "JINX_JIT=1 $JINX --readonly-mode --eval --strict $JINX_ROOT/bench/fib.nix"

# --- alloc/list microbench
hf ops 10 3 \
  "$ORACLE --readonly-mode --eval --strict $JINX_ROOT/bench/ops.nix" \
  "JINX_JIT=0 $JINX --readonly-mode --eval --strict $JINX_ROOT/bench/ops.nix" \
  "JINX_JIT=1 $JINX --readonly-mode --eval --strict $JINX_ROOT/bench/ops.nix"

# --- real nixpkgs evals
hf hello 10 2 \
  "$ORACLE --readonly-mode $NIXPKGS -A hello" \
  "JINX_JIT=0 $JINX --readonly-mode $NIXPKGS -A hello" \
  "JINX_JIT=1 $JINX --readonly-mode $NIXPKGS -A hello" \
  "JINX_JIT=1 JINX_GC_OFF=1 $JINX --readonly-mode $NIXPKGS -A hello"

hf firefox 5 1 \
  "$ORACLE --readonly-mode $NIXPKGS -A firefox" \
  "JINX_JIT=0 $JINX --readonly-mode $NIXPKGS -A firefox" \
  "JINX_JIT=1 $JINX --readonly-mode $NIXPKGS -A firefox" \
  "JINX_JIT=1 JINX_GC_OFF=1 $JINX --readonly-mode $NIXPKGS -A firefox"

# --- NixOS minimal ISO (heaviest)
hf iso 3 1 \
  "$ORACLE --readonly-mode $NIXPKGS/nixos/release.nix -A iso_minimal.x86_64-linux" \
  "JINX_JIT=0 $JINX --readonly-mode $NIXPKGS/nixos/release.nix -A iso_minimal.x86_64-linux" \
  "JINX_JIT=1 $JINX --readonly-mode $NIXPKGS/nixos/release.nix -A iso_minimal.x86_64-linux"

# --- peak RSS (single runs)
rss rss-hello-oracle  "$ORACLE" --readonly-mode "$NIXPKGS" -A hello
rss rss-hello-jinx     "$JINX" --readonly-mode "$NIXPKGS" -A hello
rss rss-firefox-oracle "$ORACLE" --readonly-mode "$NIXPKGS" -A firefox
rss rss-firefox-jinx    "$JINX" --readonly-mode "$NIXPKGS" -A firefox
rss rss-iso-oracle    "$ORACLE" --readonly-mode "$NIXPKGS/nixos/release.nix" -A iso_minimal.x86_64-linux
rss rss-iso-jinx       "$JINX" --readonly-mode "$NIXPKGS/nixos/release.nix" -A iso_minimal.x86_64-linux

# --- GC stats (jinx only)
JINX_GC_STATS=1 "$JINX" --readonly-mode "$NIXPKGS" -A hello 2>"$OUTDIR/gcstats-hello.txt" >/dev/null
JINX_GC_STATS=1 "$JINX" --readonly-mode "$NIXPKGS" -A firefox 2>"$OUTDIR/gcstats-firefox.txt" >/dev/null
JINX_GC_STATS=1 "$JINX" --readonly-mode "$NIXPKGS/nixos/release.nix" -A iso_minimal.x86_64-linux 2>"$OUTDIR/gcstats-iso.txt" >/dev/null

echo "done: results in $OUTDIR"
