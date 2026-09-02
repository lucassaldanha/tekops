# teku-op

Helper CLI for operating a Teku/Besu node, run directly on the node over SSH.

## Build

    cargo build --release

## Cross-compile for a Linux node (from macOS)

    docker run --rm --platform linux/amd64 \
      -v "$(pwd):/volume" clux/muslrust:stable cargo build --release
    scp target/x86_64-unknown-linux-musl/release/teku-op <node>:/usr/local/bin/

(For an `aarch64` node, drop `--platform linux/amd64` on Apple Silicon, or
add `--target aarch64-unknown-linux-musl` to the `cargo build` on x86_64
macOS.)

This builds inside a Rust+musl Docker image rather than via `rustup target
add`, since a plain Homebrew-installed Rust toolchain has no cross-compile
targets available and `cross` requires `rustup` on the host even though its
build runs in a container. The resulting binary is a static-PIE ELF with no
runtime dependencies - nothing but that one file needs to reach the node.

## Usage

    teku-op logs teku [path]      # defaults to /var/log/teku/teku.log
    teku-op logs besu [path]      # defaults to /var/log/besu/besu.log

    teku-op beacon health
    teku-op beacon head
    teku-op beacon peers
    teku-op beacon validators <index-or-pubkey>...
    teku-op beacon duties attester --epoch=N <index>...
    teku-op beacon duties proposer --epoch=N

Every `beacon` subcommand accepts `--json` to print a JSON-serialized version
of the parsed response instead of the summary/table (not a raw passthrough of
the Beacon API's wire response - e.g. `peers --json` includes a `protocol`
field that's derived locally, not sent by the API), and `--api-url` (or
`$TEKU_OP_API_URL`) to point at a non-default Beacon API (default:
`http://localhost:5051`).

The Peers table's Protocol column reflects what the peer advertises in its
ENR (exactly one of TCP or QUIC), not necessarily which transport is in
active use for that specific connection.

## Shell completion

    source <(teku-op completion bash)   # or: zsh
