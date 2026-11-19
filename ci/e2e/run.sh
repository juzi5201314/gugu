#!/usr/bin/env bash
set -euo pipefail

gugu=${1:-target/debug/gugu}
fixtures="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/fixtures" && pwd)"
scratch="$(mktemp -d)"
trap 'rm -rf -- "$scratch"' EXIT

run_case() {
    local name=$1 expected_status=$2 status=0
    local executable="$scratch/$name"

    echo "Compiling $name"
    "$gugu" build --target x86_64-linux --target-dir "$scratch" \
        --cache-dir "$scratch/cache" "$fixtures/$name.gg"
    chmod +x "$executable"

    echo "Running $name"
    timeout 30s "$executable" > "$scratch/$name.stdout" || status=$?
    if [[ $status -ne $expected_status ]]; then
        printf '%s: expected exit status %d, got %d\n' "$name" "$expected_status" "$status" >&2
        return 1
    fi
    diff -u "$fixtures/$name.stdout" "$scratch/$name.stdout"
}

run_case hello 0
run_case sum 7
