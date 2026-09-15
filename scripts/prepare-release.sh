#!/usr/bin/env bash
# Bumps Cargo.toml (and Cargo.lock) to a new version, commits the bump and
# pushes it to a branch. Called by the workflow_dispatch path of
# .github/workflows/release.yml; see docs/RELEASING.md.
#
# Usage: scripts/prepare-release.sh 0.9.0 master
#
# This deliberately does NOT create the tag. The tag is created later, by
# `gh release create --target` in the release job, so that a failing test
# suite or a broken build leaves a bump commit on the branch rather than a
# tag pointing at a release that was never published - a bump commit is
# revertable, a published-then-deleted tag is not.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

version="${1:-}"
branch="${2:-}"
if [ -z "$version" ] || [ -z "$branch" ]; then
  echo "usage: prepare-release.sh <version> <branch>" >&2
  exit 1
fi

# The dispatch form takes a bare X.Y.Z, but a leading v is what everyone
# types after years of tagging, so accept it rather than failing on it.
version="${version#v}"
tag="v$version"

if ! printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "version '$version' is not X.Y.Z" >&2
  exit 1
fi

if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  echo "tag '$tag' already exists" >&2
  echo "a released version is never rebuilt in place; pick the next one" >&2
  exit 1
fi

current="$(scripts/read-version.sh)"

# sort -V puts the lower version first, so the new one has to sort second and
# differ. This is what catches a typo'd digit ('0.7.0' for '0.9.0') - the tag
# check above cannot see it, because that tag genuinely does not exist yet.
lower="$(printf '%s\n%s\n' "$current" "$version" | sort -V | head -n1)"
if [ "$version" = "$current" ] || [ "$lower" != "$current" ]; then
  echo "version '$version' does not follow the current '$current'" >&2
  exit 1
fi

# Only the [package] stanza's version: a dependency pinned at the same string
# must not be caught. Same anchoring as read-version.sh, which is what reads
# the result back.
awk -v v="$version" '
  /^\[package\]/ { in_package = 1; print; next }
  /^\[/          { in_package = 0 }
  in_package && !done && /^version *=/ {
    print "version = \"" v "\""; done = 1; next
  }
  { print }
' Cargo.toml > Cargo.toml.bump
mv Cargo.toml.bump Cargo.toml

written="$(scripts/read-version.sh)"
if [ "$written" != "$version" ]; then
  echo "bumped Cargo.toml but it reads back as '$written', not '$version'" >&2
  exit 1
fi

# --workspace restricts this to the workspace member, so no dependency is
# re-resolved; every build downstream passes --locked and would fail on a
# Cargo.lock still carrying the old version.
cargo update --workspace --quiet

# Cheap proof the two files agree, before anything is pushed. `cargo metadata`
# resolves the graph without compiling, so a lock that --locked would reject
# fails here in a second instead of in the verify job, after the commit has
# already landed on the branch.
cargo metadata --locked --format-version 1 >/dev/null

git config user.name 'github-actions[bot]'
git config user.email '41898282+github-actions[bot]@users.noreply.github.com'
git add Cargo.toml Cargo.lock
git commit -m "chore(release): $tag"
git push origin "HEAD:refs/heads/$branch"

echo "prepared $tag on $branch ($(git rev-parse HEAD))"
