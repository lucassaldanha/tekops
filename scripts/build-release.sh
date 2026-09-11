#!/usr/bin/env bash
# Builds and packages one release target.
#
# The same script runs in CI and locally, so "works on my machine" and
# "works in CI" are the same question. Linux targets build inside a
# digest-pinned container that carries the musl C toolchain ring needs;
# they cross-compile from x86_64 rather than emulating, so no QEMU and no
# hosted ARM runner is involved.
#
# Usage: scripts/build-release.sh <target>
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
repo_root="$PWD"

# Pinned by digest, not tag. Refresh with:
#   docker pull --platform linux/amd64 messense/rust-musl-cross:<tag>
#   docker inspect --format='{{index .RepoDigests 0}}' messense/rust-musl-cross:<tag>
IMAGE_X86_64="messense/rust-musl-cross@sha256:ce75e9174325d4fbb3de85c309e2d7ca29f7500169bc4b5d2c611ff7e86d549a"
IMAGE_AARCH64="messense/rust-musl-cross@sha256:ecae5dd62d1c938c14f8071d36c16fa699860aace03bfb5284fb1216474d2643"

target="${1:-}"
if [ -z "$target" ]; then
  echo "usage: build-release.sh <target>" >&2
  exit 1
fi

# The commit's own timestamp, so a rebuild of the same commit gets the same
# value rather than "now".
SOURCE_DATE_EPOCH="$(git log -1 --pretty=%ct)"
export SOURCE_DATE_EPOCH

version="$(scripts/read-version.sh)"

build_in_container() {
  local image="$1"
  docker run --rm \
    --platform linux/amd64 \
    -v "$repo_root:/volume" \
    -w /volume \
    -e SOURCE_DATE_EPOCH \
    -e "RUSTFLAGS=--remap-path-prefix=/volume=." \
    "$image" \
    cargo build --locked --release --target "$target"
}

# Each arm also names the published asset. Release assets are named
# <arch>-<os> rather than by the cargo target triple: the triple's vendor
# field ("unknown") carries no information, and "musl" is implied because
# every Linux build here is static. The mapping lives inside the case that
# already dispatches per target so a new target cannot be added without
# choosing its asset name - `src/update.rs`'s TARGET constants must agree
# with these strings or `tekops update` 404s.
case "$target" in
  x86_64-unknown-linux-musl)
    asset_target="x86_64-linux"
    build_in_container "$IMAGE_X86_64"
    ;;
  aarch64-unknown-linux-musl)
    asset_target="aarch64-linux"
    build_in_container "$IMAGE_AARCH64"
    ;;
  aarch64-apple-darwin)
    asset_target="aarch64-macos"
    # No macOS containers exist, so this one builds on the host/runner.
    # It gets the same pinned toolchain and locked inputs, but the SDK
    # underneath is whatever the runner ships.
    RUSTFLAGS="--remap-path-prefix=$repo_root=." \
      cargo build --locked --release --target "$target"
    ;;
  *)
    echo "unsupported target: $target" >&2
    echo "supported: x86_64-unknown-linux-musl, aarch64-unknown-linux-musl, aarch64-apple-darwin" >&2
    exit 1
    ;;
esac

binary="target/$target/release/tekops"
[ -f "$binary" ] || { echo "build produced no binary at $binary" >&2; exit 1; }

mkdir -p dist
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
cp "$binary" "$stage/tekops"
cp README.md LICENSE "$stage/"

tarball="dist/tekops-v$version-$asset_target.tar.gz"
tar -czf "$tarball" -C "$stage" tekops README.md LICENSE

# The reproducibility claim is about the binary, not the archive: tar
# metadata differs between GNU tar in the container and bsdtar on macOS.
# The tarball is transport; SHA256SUMS covers it for download integrity.
echo "built   $tarball"
echo -n "binary sha256: "
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$binary" | cut -d' ' -f1
else
  shasum -a 256 "$binary" | cut -d' ' -f1
fi
