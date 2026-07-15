#!/usr/bin/env bash
# Usage: bash bench/run-compute-benchmarks.sh [outdir]
exec bash "$(dirname "$0")/run-parity-benchmarks.sh" compute "$@"
