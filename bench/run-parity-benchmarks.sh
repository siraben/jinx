#!/usr/bin/env bash
# Shared parity gate and hyperfine runner for the compute and strengths suites.
set -euo pipefail

MODE=${1:?usage: run-parity-benchmarks.sh compute|strengths [outdir]}
shift
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
JINX="${JINX:-$ROOT/target/release/jinx}"

case "$MODE" in
  compute)
    if [[ -z "${ORACLE:-}" ]]; then
      if [[ -x "$ROOT/.oracle/bin/nix-instantiate" ]]; then
        ORACLE="$ROOT/.oracle/bin/nix-instantiate"
      else
        ORACLE="$(command -v nix-instantiate || true)"
      fi
    fi
    OUTDIR="${1:-$ROOT/bench/results/compute}"
    GRAPH="$ROOT/bench/graphs/compute.svg"
    ;;
  strengths)
    ORACLE="${ORACLE:-$ROOT/.oracle/bin/nix-instantiate}"
    NIXPKGS="${NIXPKGS:?set NIXPKGS to the pinned nixpkgs checkout}"
    OUTDIR="${1:-$ROOT/bench/results/strengths}"
    GRAPH="$ROOT/bench/graphs/eval-strengths.svg"
    export JINX_JIT=0
    ;;
  *) echo "unknown suite: $MODE" >&2; exit 2 ;;
esac

export NIX_REMOTE=dummy:// NIX_STORE_DIR=/nix/store
command -v hyperfine >/dev/null || { echo "hyperfine not on PATH" >&2; exit 1; }
test -x "$JINX" || { echo "missing jinx binary: $JINX" >&2; exit 1; }
test -n "$ORACLE" && test -x "$ORACLE" || {
  echo "missing nix-instantiate oracle (set ORACLE=/path/to/nix-instantiate)" >&2
  exit 1
}
mkdir -p "$OUTDIR"

dirty=$(test -n "$(git -C "$ROOT" status --porcelain -- . \
  ':(exclude)bench/graphs/*.svg' ':(exclude)bench/results/**')" && echo yes || echo no)
{
  echo "timestamp_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  [[ "$MODE" == compute ]] && echo "system=$(uname -a)"
  echo "jinx_commit=$(git -C "$ROOT" rev-parse HEAD)"
  echo "jinx_dirty=$dirty"
  echo "jinx=$JINX"
  echo "oracle=$ORACLE"
  echo "oracle_version=$($ORACLE --version 2>&1 | head -n 1)"
  [[ "$MODE" == strengths ]] && echo "nixpkgs_commit=$(git -C "$NIXPKGS" rev-parse HEAD)"
  echo "hyperfine=$(hyperfine --version | head -n 1)"
} > "$OUTDIR/metadata.txt"

parity_and_bench() {
  local name=$1 runs=$2 warmups=$3
  shift 3
  local -a commands=("$@") outputs=()
  local tempdir index
  tempdir=$(mktemp -d "${TMPDIR:-/tmp}/jinx-$MODE.XXXXXX")
  trap 'rm -rf "$tempdir"' RETURN

  for index in "${!commands[@]}"; do
    outputs+=("$tempdir/$index")
    eval "${commands[$index]}" > "${outputs[$index]}"
  done
  for index in "${!outputs[@]}"; do
    (( index == 0 )) && continue
    if ! cmp -s "${outputs[0]}" "${outputs[$index]}"; then
      echo "$name: output mismatch; refusing to benchmark" >&2
      diff -u "${outputs[0]}" "${outputs[$index]}" >&2 || true
      exit 1
    fi
  done

  echo "=== $name (stdout parity: yes)"
  hyperfine --warmup "$warmups" --runs "$runs" \
    --export-json "$OUTDIR/$name.json" "${commands[@]}"
  rm -rf "$tempdir"
  trap - RETURN
}

if [[ "$MODE" == compute ]]; then
  compute() {
    local name=$1 runs=$2 warmups=$3 expression="$ROOT/bench/$4"
    parity_and_bench "$name" "$runs" "$warmups" \
      "$ORACLE --readonly-mode --eval --strict $expression" \
      "JINX_JIT=0 $JINX --readonly-mode --eval --strict $expression" \
      "JINX_JIT=1 $JINX --readonly-mode --eval --strict $expression"
  }
  compute numerical-fold 10 3 compute-fold.nix
  compute stable-sort 10 3 compute-sort.nix
  compute prime-scan 7 2 compute-primes.nix
  compute fibonacci 5 1 compute-fib.nix
  compute nqueens 5 1 compute-nqueens.nix
  compute record-shapes 7 2 compute-records.nix
else
  strength() { parity_and_bench "$@"; }
  strength parse 20 3 \
    "$ORACLE --parse $NIXPKGS/pkgs/top-level/all-packages.nix" \
    "$JINX --parse $NIXPKGS/pkgs/top-level/all-packages.nix"
  for spec in \
    "generic-closure:10:3:generic-closure.nix" \
    "deep-force:10:3:deep-force-recursive.nix" \
    "ops:10:3:ops.nix"
  do
    IFS=: read -r name runs warmups file <<< "$spec"
    strength "$name" "$runs" "$warmups" \
      "$ORACLE --readonly-mode --eval --strict $ROOT/bench/$file" \
      "$JINX --readonly-mode --eval --strict $ROOT/bench/$file"
  done
  strength iso 3 1 \
    "$ORACLE --readonly-mode $NIXPKGS/nixos/release.nix -A iso_minimal.x86_64-linux" \
    "$JINX --readonly-mode $NIXPKGS/nixos/release.nix -A iso_minimal.x86_64-linux"
fi

python3 "$ROOT/bench/plot-parity-benchmarks.py" "$MODE" "$OUTDIR" "$GRAPH"
echo "done: $OUTDIR"
