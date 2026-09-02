# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`tekops`: a single-binary Rust CLI for operating a Teku/Besu Ethereum node, run directly on the node over SSH. Three feature areas:

- `tekops logs [teku|besu] [path]` — tails and colorizes a JSON log file (replaces an old bashrc/jq function). Source defaults to teku when omitted.
- `tekops beacon validators|duties`, plus the top-level `tekops peers`, `tekops health`, and `tekops head` — a typed HTTP client wrapper around a curated set of Beacon API endpoints.
- `tekops duties` — published-duty counts (blocks, attestations, sync committee messages, aggregates) read directly from the validator client's own Prometheus `/metrics` scrape endpoint. Not to be confused with `tekops beacon duties attester|proposer`, which queries the Beacon API for upcoming duty assignments — same word, different data source, both requested by name.

## Commands

```bash
cargo build --release        # local build
cargo test                   # full suite
cargo test beaconapi::       # one module's tests (also: logfmt::, logs::, output::, protocol::, metrics::, http::, cli::)
cargo test health_ready_on_200  # a single test by name
cargo clippy --all-targets   # lint
```

### Cross-compiling for deployment

The node runs Linux x86_64. **This dev machine's Rust is Homebrew-installed, not `rustup`** — there is no `rustup target add`, and the `cross` tool doesn't work either (it shells out to `rustup toolchain list` even though the actual build runs in Docker). The working approach is to build directly inside a Rust-musl Docker image, forcing the `amd64` platform (this machine is Apple Silicon, so Docker otherwise defaults to an `aarch64` build):

```bash
docker run --rm --platform linux/amd64 \
  -v "$(pwd):/volume" clux/muslrust:stable cargo build --release
# binary at target/x86_64-unknown-linux-musl/release/tekops — copy just this one file
scp target/x86_64-unknown-linux-musl/release/tekops <node>:/usr/local/bin/tekops
```

The resulting binary is a static-PIE ELF (`file` confirms `static-pie linked`) — no runtime deps on the node, nothing else needs to be copied over. (The README's cross-compile section still says `rustup target add`, which doesn't apply on this machine — the Docker approach above is what actually works here.)

## Architecture

Each module has one job; `cli.rs` is the only place that wires them together.

- **`main.rs`** — just `mod` declarations and `cli::run()`.
- **`cli.rs`** — all `clap` command/subcommand definitions and dispatch. Every `beacon` subcommand (and the top-level `peers`/`health`/`head`/`duties` commands) follows the same shape: call one or more client methods, then either print a `serde_json`-serialized struct (`--json`) or hand the result to an `output::format_*` function. `exit_for` centralizes error handling — each handler returns `Result<(), ApiError>` and `exit_for` prints `error: {e}` and sets the exit code. `peers`, `health`, `head`, and `duties` live at the top level (not nested under `beacon`) since they're used far more often than the other `beacon` subcommands (`duties` also isn't a Beacon API call at all — see `metrics.rs`).
- **`http.rs`** — the `ApiError` enum (`Unreachable`, `Status(u16, String)`, `Malformed(String)`) and `map_ureq_error`, shared by both `beaconapi.rs` and `metrics.rs` since they're both thin `ureq`-backed HTTP clients with identical failure modes. Error text is endpoint-agnostic ("could not reach endpoint: ...") rather than naming the Beacon API specifically, precisely because `metrics.rs` reuses it.
- **`beaconapi.rs`** — `BeaconClient`, backed by `ureq`. Two shared helpers, `get_json`/`post_json`, wrap the request/JSON-decode boilerplate (via `map_ureq_error` from `http.rs`) — every endpoint method should go through one of these rather than hand-rolling a `ureq` call. Wire response shapes (nested `{"data": {...}}` envelopes, `validator.pubkey` nesting, etc.) are private structs that get flattened into the public structs (`SyncingStatus`, `BlockHeader`, `PeerInfo`, `ValidatorInfo`, ...) — callers never see the wire nesting.
- **`metrics.rs`** — `MetricsClient`, which fetches a Prometheus text-exposition page (Teku/Besu's own `/metrics` endpoint — not a Prometheus server's `/api/v1/query` HTTP API) and parses it locally with a small hand-rolled exposition-format parser (`parse_exposition`/`Sample`, private). `MetricsClient::duties()` sums (`matching_value`) samples matching a metric name + label matchers, mirroring what a PromQL `sum(metric{k="v",...})` would do in Grafana, since a raw scrape endpoint has no query engine behind it. `DutiesMetrics` is the resulting public, `Serialize`-able struct.
- **`output.rs`** — pure formatting functions (`format_health_table`, `format_head_table`, `format_duties_table`, `format_peers_table`, etc.) that take already-fetched data and return a `String`. `comfy-table` is used for all of them. `format_health_table`, `format_head_table`, and `format_duties_table` are all Field/Value tables (one row per field on the single fetched object). `format_peers_table` prints a summary grouped by direction/protocol (count per group plus a total), not one row per peer — matching how peer counts actually get eyeballed on this node.
- **`protocol.rs`** — classifies a peer's transport as `Protocol::Tcp`/`Quic` from its `last_seen_p2p_address` multiaddr (`Quic` if it contains a `/quic` component, `Tcp` otherwise). An earlier version decoded the peer's ENR instead (via the `enr`/`k256` crates), but the Beacon API doesn't reliably populate that field and it always classified as `Unknown` in practice — don't reintroduce ENR-based classification.
- **`logfmt.rs`** — pure function, one JSON log line in, one colorized/formatted line out. No I/O, easy to unit test in isolation.
- **`logs.rs`** — `LogSource` (teku/besu, default paths), `resolve_log_path` (positional path > `$TEKOPS_LOGS_FILE` for teku only > source's hardcoded default — takes the env value as a parameter rather than reading it directly so it stays a pure, race-free unit test), and the generic `stream_logs<R: BufRead, W: Write>` loop, kept generic specifically so it's testable with in-memory buffers instead of real subprocess pipes.

### The `logs` subcommand's process lifetime (non-obvious, don't regress)

`run_logs` in `cli.rs` spawns `tail -F` piped into `stream_logs`, which writes into a pager (`less`)'s stdin. `tail -F` never reaches EOF by design, so the streaming loop can't be what ends the process — the code runs `stream_logs` on a separate thread and has the **main thread block on `pager.wait()`** instead. When the pager exits, the streaming thread either gets `BrokenPipe` (writing to the closed pager stdin, treated as normal termination) or `Ok(())` (once `tail` is killed and its pipe hits EOF). Getting this backwards (e.g. blocking the main thread on the streaming loop, or not killing `tail` before joining) reintroduces a hang when quitting the pager on a small/quiet log — this was a real regression caught in final review, not a hypothetical.

### `--json` output

Every `beacon` subcommand's `--json` output is `#[derive(Serialize)]` + `serde_json::to_string` on typed structs — never hand-built with `format!`. An earlier version did this by hand across all six subcommands and it was both fragile (invalid JSON on any field containing `"`) and a silent data-corruption bug (a malicious/unusual field value could forge extra JSON keys). If you add a new `beacon` subcommand's `--json` output, derive `Serialize` on its response struct rather than reaching for `format!`.
