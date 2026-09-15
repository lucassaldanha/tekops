#!/usr/bin/env bash
# Tests for prepare-release.sh. Run: scripts/test-prepare-release.sh
#
# The script under test commits and pushes, so this builds a throwaway clone
# with a bare repository standing in for origin. Nothing here touches this
# checkout, this repository's remote, or the network beyond whatever
# `cargo update` needs from the registry cache.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
repo="$PWD"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

work="$tmp/work"
mkdir -p "$work"
# Tracked files as of HEAD, then the working-tree copy of the script under
# test laid over the top - so an edit is exercised before it is committed.
git archive HEAD | tar -x -C "$work"
cp "$repo/scripts/prepare-release.sh" "$work/scripts/prepare-release.sh"

git init -q --bare "$tmp/origin.git"
cd "$work" || exit 1
git init -q -b master .
git config user.name 'test'
git config user.email 'test@example.com'
git add -A
git commit -qm 'initial'
git remote add origin "$tmp/origin.git"
git push -q origin master

current="$(scripts/read-version.sh)"
echo "manifest version: $current"

fail=0
assert_eq() {
  local label="$1" got="$2" want="$3"
  if [ "$got" = "$want" ]; then
    echo "ok: $label is '$got'"
  else
    echo "FAIL: $label is '$got', expected '$want'"; fail=1
  fi
}
assert_rejects() {
  local label="$1" version="$2"
  if scripts/prepare-release.sh "$version" master >/dev/null 2>&1; then
    echo "FAIL: $label ('$version') should have been rejected"; fail=1
  else
    echo "ok: $label rejected"
  fi
  if [ -n "$(git status --porcelain)" ]; then
    echo "FAIL: $label left the tree dirty"; fail=1
    git checkout -- .
  fi
}

assert_rejects "not X.Y.Z"      "1.2"
assert_rejects "not X.Y.Z"      "1.2.3-rc1"
assert_rejects "the current version" "$current"
assert_rejects "a lower version" "0.0.1"

git tag v99.0.0
assert_rejects "an existing tag" "99.0.0"
git tag -d v99.0.0 >/dev/null

# The accepting case mutates, so it goes last. A leading v is accepted and
# stripped, which is the half of the contract easiest to regress.
next="99.1.0"
if scripts/prepare-release.sh "v$next" master; then
  echo "ok: v$next accepted"
else
  echo "FAIL: v$next should have been accepted"; fail=1
fi

assert_eq "Cargo.toml" "$(scripts/read-version.sh)" "$next"

# Anchored to the tekops stanza, since another package could coincidentally
# carry the same version string.
locked="$(awk '/^name = "tekops"$/ { getline; print; exit }' Cargo.lock)"
assert_eq "Cargo.lock" "$locked" "version = \"$next\""

# The tag is the release job's to create - see the comment at the top of
# prepare-release.sh. A tag here would mean a failed build could not be
# retried under the same version.
if git rev-parse -q --verify "refs/tags/v$next" >/dev/null; then
  echo "FAIL: prepare-release.sh created a tag"; fail=1
else
  echo "ok: no tag created"
fi

pushed="$(git --git-dir="$tmp/origin.git" log -1 --format=%s master)"
assert_eq "origin/master subject" "$pushed" "chore(release): v$next"

exit "$fail"
