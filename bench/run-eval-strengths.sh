#!/usr/bin/env bash
# Usage: NIXPKGS=/path/to/nixpkgs bash bench/run-eval-strengths.sh [outdir]
exec bash "$(dirname "$0")/run-parity-benchmarks.sh" strengths "$@"
