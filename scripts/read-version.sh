#!/usr/bin/env bash
# Prints the version declared in Cargo.toml's [package] section.
# Deliberately dependency-free (no jq, no cargo) so it behaves identically
# on the host, on a CI runner, and inside the musl container.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

awk '
  /^\[package\]/ { in_package = 1; next }
  /^\[/          { in_package = 0 }
  in_package && /^version *=/ {
    gsub(/[" ]/, ""); sub(/^version=/, ""); print; exit
  }
' Cargo.toml
