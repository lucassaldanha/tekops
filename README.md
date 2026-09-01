# teku-op

Helper CLI for operating a Teku/Besu node, run directly on the node over SSH.

## Build

    cargo build --release

## Cross-compile for a Linux node (from macOS)

    rustup target add x86_64-unknown-linux-musl   # or aarch64-unknown-linux-musl
    cargo build --release --target x86_64-unknown-linux-musl
    scp target/x86_64-unknown-linux-musl/release/teku-op <node>:/usr/local/bin/

## Usage

    teku-op logs teku [path]      # defaults to /var/log/teku/teku.log
    teku-op logs besu [path]      # defaults to /var/log/besu/besu.log

    teku-op beacon health
    teku-op beacon head
    teku-op beacon peers
    teku-op beacon validators <index-or-pubkey>...
    teku-op beacon duties attester --epoch=N <index>...
    teku-op beacon duties proposer --epoch=N

Every `beacon` subcommand accepts `--json` for the raw API response, and
`--api-url` (or `$TEKU_OP_API_URL`) to point at a non-default Beacon API
(default: `http://localhost:5051`).

## Shell completion

    source <(teku-op completion bash)   # or: zsh
