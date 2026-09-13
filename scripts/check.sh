#!/usr/bin/env bash
# Runs the same three gates as .github/workflows/ci.yml, in the same order.
# Usage: scripts/check.sh
#
# The order is CI's and is deliberate: formatting is the cheapest to fail, so
# it goes first. Keep the commands here identical to the workflow's - the test
# `local_check_runs_the_same_gates_as_ci` reads both files and fails if they
# drift apart.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

echo "==> Format"
cargo fmt --all -- --check

echo "==> Test"
cargo test --locked

echo "==> Clippy"
cargo clippy --locked --all-targets -- -D warnings

echo "all gates passed"
