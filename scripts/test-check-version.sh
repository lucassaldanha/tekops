#!/usr/bin/env bash
# Tests for check-version.sh. Run: scripts/test-check-version.sh
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

fail=0
assert_ok() {
  if scripts/check-version.sh "$1" >/dev/null 2>&1; then
    echo "ok: $1 accepted"
  else
    echo "FAIL: $1 should have been accepted"; fail=1
  fi
}
assert_rejects() {
  if scripts/check-version.sh "$1" >/dev/null 2>&1; then
    echo "FAIL: $1 should have been rejected"; fail=1
  else
    echo "ok: $1 rejected"
  fi
}

version="$(scripts/read-version.sh)"
[ -n "$version" ] || { echo "FAIL: read-version.sh printed nothing"; exit 1; }
echo "manifest version: $version"

assert_ok "v$version"
assert_rejects "v9.9.9"
assert_rejects "$version"        # missing the v prefix
assert_rejects ""                # no tag at all

exit "$fail"
