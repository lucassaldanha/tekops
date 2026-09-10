#!/usr/bin/env bash
# Fails unless the git tag agrees with the version in Cargo.toml.
# Usage: scripts/check-version.sh v0.3.0
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

tag="${1:-}"
if [ -z "$tag" ]; then
  echo "usage: check-version.sh <tag>" >&2
  exit 1
fi

case "$tag" in
  v*) expected="${tag#v}" ;;
  *)  echo "tag '$tag' does not start with 'v'" >&2; exit 1 ;;
esac

manifest="$(scripts/read-version.sh)"

if [ "$manifest" != "$expected" ]; then
  echo "tag '$tag' implies version '$expected' but Cargo.toml declares '$manifest'" >&2
  echo "bump Cargo.toml or retag; refusing to publish a mislabelled release" >&2
  exit 1
fi

echo "version '$manifest' matches tag '$tag'"
