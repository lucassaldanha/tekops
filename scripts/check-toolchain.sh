#!/usr/bin/env bash
# Fails unless the rustc that will actually build is the one
# rust-toolchain.toml pins. Run it by hand on a new build host, and in CI on
# any job that compiles on the host rather than inside the pinned container.
#
# rust-toolchain.toml is a rustup feature. On a host whose Rust came from
# anywhere else - Homebrew, a distro package - it is ignored in silence and
# the build uses whatever compiler happens to be installed. GitHub-hosted
# runners ship rustup, so the pin binds there and this should always pass. It
# guards against the day an image, or a new build host, puts another Rust
# ahead of the shim: that failure has no symptom, it just publishes binaries
# built by a compiler nobody chose.
#
# Usage: scripts/check-toolchain.sh
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

pinned="$(awk -F'"' '/^channel *=/ { print $2; exit }' rust-toolchain.toml)"
if [ -z "$pinned" ]; then
  echo "no channel found in rust-toolchain.toml" >&2
  exit 1
fi

if ! command -v rustup >/dev/null 2>&1; then
  found="$(rustc --version 2>/dev/null)" || found="no rustc at all"
  echo "rustup is not installed, so rust-toolchain.toml's '$pinned' pin does nothing here" >&2
  echo "this host would build with: $found" >&2
  # An installed rustup that is merely invisible looks identical to an absent
  # one from here, and the two need opposite fixes: install it, or put its
  # shims on the PATH of whatever ran this. Say which it is rather than
  # leaving the reader to guess from a message that only describes the
  # symptom.
  if [ -x "${HOME:-}/.cargo/bin/rustup" ]; then
    echo "note: $HOME/.cargo/bin/rustup exists - it is installed, just not on this process's PATH" >&2
    echo "note: PATH=$PATH" >&2
  fi
  exit 1
fi

# The effective rustc, not rustup's opinion of it: if something earlier on
# PATH shadows the rustup shim, that shadowing compiler is what cargo runs and
# what this has to report on.
active="$(rustc --version | awk '{ print $2 }')"
if [ "$active" != "$pinned" ]; then
  echo "rust-toolchain.toml pins '$pinned' but rustc reports '$active'" >&2
  echo "rustup is installed but something else is ahead of its shim on PATH" >&2
  exit 1
fi

echo "toolchain '$pinned' is active ($(rustup show active-toolchain))"
