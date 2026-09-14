# tekops

Helper CLI for operating a Teku/Besu node, run directly on the node over SSH.

    tekops logs                  # tail and colorize the node's logs
    tekops health                # health and sync status
    tekops peers                 # peer counts by direction and protocol

**[USAGE.md](USAGE.md) documents every command**, including running against
Eth Docker and Rocket Pool deployments.

## Install

Download the binary for your platform from the [latest release](https://github.com/lucassaldanha/tekops/releases/latest):

| Platform | Asset |
| --- | --- |
| Linux x86_64 | `tekops-v<version>-x86_64-linux.tar.gz` |
| Linux arm64 | `tekops-v<version>-aarch64-linux.tar.gz` |
| macOS Apple silicon | `tekops-v<version>-aarch64-macos.tar.gz` |

Releases up to v0.3.1 used the cargo target triple instead
(`x86_64-unknown-linux-musl` and friends); the names above start at v0.4.0.

    VERSION=0.4.0
    TARGET=x86_64-linux
    curl -LO "https://github.com/lucassaldanha/tekops/releases/download/v$VERSION/tekops-v$VERSION-$TARGET.tar.gz"
    tar xzf "tekops-v$VERSION-$TARGET.tar.gz"
    sudo install -m755 tekops /usr/local/bin/tekops

The Linux binaries are statically linked, so there is nothing else to install
on the node.

Verify what you downloaded against `SHA256SUMS` from the same release:

    curl -LO "https://github.com/lucassaldanha/tekops/releases/download/v$VERSION/SHA256SUMS"
    sha256sum -c SHA256SUMS --ignore-missing

**macOS:** the binary is not signed or notarized. Downloaded with `curl` as
above it runs normally. If you download it through a browser instead, macOS
quarantines it and reports that the developer cannot be verified; clear that
with:

    xattr -d com.apple.quarantine tekops

While this repository is private, the release assets need an authenticated
download instead of `curl`:

    gh release download "v$VERSION" -p "tekops-v$VERSION-$TARGET.tar.gz"

## Requirements

tekops shells out to a few standard tools rather than reimplementing them.
All of them are present on a default Debian/Ubuntu install.

| Tool | Needed by | Why |
| --- | --- | --- |
| `tail` | `tekops logs` | follows the log file |
| `less` | `tekops logs` | the pager, with scrollback and search |
| `curl` | `tekops update`, `tekops log-level <url>` | HTTPS - tekops itself is built without a TLS stack |
| `tar` | `tekops update` | unpacks the release tarball |

Everything else is statically linked into the binary; nothing but that one
file needs to reach the node.

## Build

    cargo build --release

## Building a release binary

    scripts/build-release.sh x86_64-unknown-linux-musl

The argument is a cargo target triple. Supported targets are
`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, and
`aarch64-apple-darwin`.

The tarball it writes is named for the platform rather than the triple, so
the command above produces `dist/tekops-v<version>-x86_64-linux.tar.gz`. It
also prints the binary's SHA256.

Linux targets build inside a digest-pinned musl container and cross-compile
from x86_64, so no `rustup target add` is needed on the host (a Homebrew
Rust has no cross-compile targets available, and `cross` requires `rustup`
on the host even though its build runs in a container) and the aarch64 build
does not emulate. The resulting binaries are statically linked with no
runtime dependencies - nothing but that one file needs to reach the node.

This is the same script CI runs, so a locally built Linux binary hashes
identically to the published one.
