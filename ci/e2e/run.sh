#!/usr/bin/env bash
set -euo pipefail

gugu=${1:-target/debug/gugu}
fixtures="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/fixtures" && pwd)"
scratch="$(mktemp -d)"
trap 'rm -rf -- "$scratch"' EXIT

run_case() {
    local name=$1 expected_status=$2 input
    shift 2
    local executable="$scratch/$name"

    echo "Compiling $name"
    "$gugu" build --target x86_64-linux --target-dir "$scratch" \
        --cache-dir "$scratch/cache" "$fixtures/$name.gg"
    chmod +x "$executable"

    echo "Running $name"
    if [[ $# -eq 0 ]]; then
        run_executable "$executable" "$expected_status" > "$scratch/$name.stdout"
    else
        for input in "$@"; do
            run_executable "$executable" "$expected_status" "$input"
        done > "$scratch/$name.stdout"
    fi
    diff -u "$fixtures/$name.stdout" "$scratch/$name.stdout"
}

run_executable() {
    local executable=$1 expected_status=$2 status=0
    shift 2
    timeout 30s "$executable" "$@" || status=$?
    if [[ $status -ne $expected_status ]]; then
        printf '%s: expected exit status %d, got %d\n' "${executable##*/}" "$expected_status" "$status" >&2
        return 1
    fi
}

run_case hello 0
run_case sum 7
run_case collections 0
run_case formatting 0
# Bound allocations per process while checking every UAX #15 test row.
run_case normalization 0 "$fixtures"/NormalizationTest.*.utf8
