#!/usr/bin/env bash
# Runs exactly what CI runs; stops at the first failure. Use before every push.
set -euo pipefail
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
echo "all checks passed"
