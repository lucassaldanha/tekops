# tekops

Helper CLI for operating a Teku/Besu node, run directly on the node over SSH.

## Install

Download the binary for your platform from the [latest release](https://github.com/lucassaldanha/tekops/releases/latest):

| Platform | Asset |
| --- | --- |
| Linux x86_64 | `tekops-v<version>-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 | `tekops-v<version>-aarch64-unknown-linux-musl.tar.gz` |
| macOS Apple silicon | `tekops-v<version>-aarch64-apple-darwin.tar.gz` |

    VERSION=0.2.1
    TARGET=x86_64-unknown-linux-musl
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

## Build

    cargo build --release

## Building a release binary

    scripts/build-release.sh x86_64-unknown-linux-musl

Writes `dist/tekops-v<version>-<target>.tar.gz` and prints the binary's
SHA256. Supported targets are `x86_64-unknown-linux-musl`,
`aarch64-unknown-linux-musl`, and `aarch64-apple-darwin`.

Linux targets build inside a digest-pinned musl container and cross-compile
from x86_64, so no `rustup target add` is needed on the host (a Homebrew
Rust has no cross-compile targets available, and `cross` requires `rustup`
on the host even though its build runs in a container) and the aarch64 build
does not emulate. The resulting binaries are statically linked with no
runtime dependencies - nothing but that one file needs to reach the node.

This is the same script CI runs, so a locally built Linux binary hashes
identically to the published one.

## Usage

    tekops logs [path]           # source defaults to teku; a bare path works
    tekops logs teku [path]      # defaults to /var/log/teku/teku.log, or $TEKOPS_LOGS_FILE
    tekops logs besu [path]      # defaults to /var/log/besu/besu.log
    tekops logs -n 2000          # 2000 lines of scrollback instead of the default 500
    tekops logs --lines 0        # skip existing output, follow only new lines

`logs` opens with the last 500 lines already in the buffer and then follows
new output. `-n`/`--lines` changes that count; it is passed straight to
`tail -n`, so `0` means "show nothing existing, follow only what arrives
next". The pre-loaded lines go into the scrollback buffer, not just on
screen, so searching covers all of them from the moment the session starts.

    tekops beacon validators <index-or-pubkey>...
    tekops beacon duties attester --epoch=N <index>...
    tekops beacon duties proposer --epoch=N

    tekops peers
    tekops health
    tekops head
    tekops duties
    tekops validators
    tekops version
    tekops log-level <LEVEL> [--filter=org.example ...]

Every `beacon` subcommand, plus `peers`, `health`, `head`, and `log-level`,
accepts `--json` to print a JSON-serialized version of the parsed response
instead of the table (not a raw passthrough of the Beacon API's wire
response - e.g. `peers --json` includes a `protocol` field that's derived
locally, not sent by the API), and `--api-url` (or `$TEKOPS_API_URL`) to
point at a non-default Beacon API (default: `http://localhost:5051`).

`tekops --version` reports the `tekops` build itself, which is a different
question from `tekops version` (the running Teku's version, below).

Every network command gives up after 10 seconds rather than waiting forever
on a node that accepts the connection but never answers.

`tekops` is built without a TLS backend, so `--api-url` and `--metric-url`
must be `http://` URLs. It is meant to run on the node against its own local
endpoints, and dropping TLS removes `rustls` and `ring` (and all C
compilation) from the build, cutting the binary roughly in half. An `https://`
URL fails immediately with `cannot make HTTPS request because no TLS backend
is configured` rather than doing anything surprising.

`duties`, `validators`, and `version` all read the validator client's own
Prometheus `/metrics` page instead of the Beacon API - they accept `--json`,
plus `--metric-url` (or `$TEKOPS_METRIC_URL`) to point at a non-default
metrics endpoint (default: `http://localhost:8010/metrics`). If the endpoint
responds but doesn't export the metric being asked for, these fail with an
error rather than reporting a confident zero - pointing at the beacon node's
metrics port instead of the validator client's would otherwise render as
"this validator published nothing". `duties` and
`validators` are a local equivalent of Grafana panel queries like
`sum(validator_beacon_node_requests_total{method="...",outcome="success"})` -
`tekops` fetches the raw exposition text and filters/sums locally, since a
single node's `/metrics` page has no query engine behind it (and no
`instance` label to filter on - that's added by Prometheus at scrape time).

`duties` prints published-duty counts: blocks, attestations, sync committee
messages, aggregates.

`validators` prints two tables: validator key counts grouped by status (a
local equivalent of `sort_desc(sum by (status)
(validator_local_validator_counts{instance=~"$system"}))`), and total
locally-stated ETH balance (summed from `validator_local_validator_balances`,
reported in Gwei and converted to ETH). Not to be confused with `tekops
beacon validators <index-or-pubkey>...`, which looks up individual
validators' on-chain status via the Beacon API rather than reading local
validator-client metrics.

`version` prints the running Teku version, read from the `version` label on
whichever of `beacon_teku_version_total` or `validator_teku_version_total` is
present on the scrape (only one exists at a time, depending on whether
`--metric-url` points at a beacon node's or a validator client's `/metrics`
endpoint).

`log-level` sends a `PUT` to `/teku/v1/admin/log_level` to change the node's
runtime log level - the only mutating command `tekops` has. The level is a
positional argument, e.g. `tekops log-level info`, sent to the API exactly
as typed (no case normalization). Repeat `--filter` to scope the change to
one or more logger names (e.g. `org.hyperledger.besu`, or a fully-qualified
class); omit it entirely to change the global log level - `log_filter` is
left out of the request body in that case rather than sent as `null` or
`[]`.

`peers` prints a table of peer counts grouped by direction and protocol, plus
the total peer count, rather than one row per peer:

    +-----------+----------+-------+-------+
    | Direction | Protocol | Count | Total |
    +======================================+
    | inbound   | QUIC     | 95    | 200   |
    |-----------+----------+-------+-------|
    | inbound   | TCP      | 75    | 200   |
    |-----------+----------+-------+-------|
    | outbound  | QUIC     | 24    | 200   |
    |-----------+----------+-------+-------|
    | outbound  | TCP      | 6     | 200   |
    +-----------+----------+-------+-------+

The Protocol column is derived from each peer's `last_seen_p2p_address`
multiaddr (QUIC if it advertises a `/quic` component, TCP otherwise) rather
than the peer's ENR, which the Beacon API doesn't reliably populate.
