# tekops

Helper CLI for operating a Teku node, run directly on the node over SSH.

    tekops logs                  # tail and colorize the node's logs
    tekops health                # health and sync status
    tekops peers                 # peer counts by direction and protocol

**[USAGE.md](USAGE.md) documents every command**, including running against
Eth Docker and Rocket Pool deployments.

## About this project

tekops is a personal project I build in my spare time to operate my own node. It
is not a Consensys project: not affiliated with, endorsed by, or supported by
Consensys or the Teku team. Teku is their software, this is just a CLI I wrote
for running it.

It comes with no guarantees and no support. I add what I need and fix what I hit
on my own node, on no particular schedule. Issues and pull requests are welcome,
but I may not get to them.

It is released under the [Apache License 2.0](LICENSE), which disclaims all
warranty and liability. tekops runs on a machine that runs a validator, so give
it the same scrutiny you would give any other third-party tool with access to
that host.

## Install

Download the binary for your platform from the [latest release](https://github.com/lucassaldanha/tekops/releases/latest):

| Platform | Asset |
| --- | --- |
| Linux x86_64 | `tekops-v<version>-x86_64-linux.tar.gz` |
| Linux arm64 | `tekops-v<version>-aarch64-linux.tar.gz` |
| macOS Apple silicon | `tekops-v<version>-aarch64-macos.tar.gz` |

## Update

    tekops update

Checks GitHub Releases, shows `current -> latest`, and asks before replacing the
running binary. The download is verified against the release's `SHA256SUMS` and
smoke-tested first, so a failed update leaves the working binary in place. Use
`sudo` if tekops lives in a root-owned directory such as `/usr/local/bin`.

tekops never checks for updates on its own. Nothing touches the network unless
you run this.

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

If you prefere to build it yourself.