# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with
code in this repository.

## What this is

`tekops`: a single-binary Rust CLI for operating a Teku Ethereum node, run
directly on the node over SSH. Four groups of commands:

- **`logs`, `dump-logs`** - tail and colorize the Teku log, or write the last N
  lines to a file or a secret gist with sensitive values anonymised.
- **`peers`, `health`, `head`, `log-level`** - a typed client over a curated set
  of Beacon API endpoints. `log-level` is the only mutating command in tekops.
- **`duties`, `validators`, `version`, `doctor`** - read a Prometheus `/metrics`
  scrape endpoint. `doctor` reads that, the Beacon API, and host/Docker facts.
- **`update`, `autocomplete`, `about`** - self-management; these touch nothing
  on the node.

## Where things are documented

| Doc | Holds | Audience |
| --- | --- | --- |
| `USAGE.md` | every command, its flags and its output | operator |
| `README.md` | installing and building, nothing else | operator |
| `docs/ARCHITECTURE.md` | what each module owns and why it is shaped that way | maintainer |
| `docs/RELEASING.md` | building, the local gate, signing, cutting a release | maintainer |

`USAGE.md` and `README.md` ship in the release tarball; the two `docs/` files
deliberately do not. **Usage belongs in `USAGE.md`** - adding it back into the
README is a regression, not a convenience.

## Commands

```bash
cargo build --release        # local build
cargo test                   # full suite
cargo test beaconapi::       # one module's tests (also: logfmt::, logs::, loglevel::, output::, protocol::, metrics::, http::, curl::, term::, cli::, completions::)
cargo test health_ready_on_200  # a single test by name
cargo clippy --all-targets   # lint
scripts/check.sh             # every CI gate, in CI's order - what pre-push runs
scripts/build-release.sh <target>   # cross-compile; see docs/RELEASING.md
```

## Architecture

Each module has one job; `cli.rs` is the only place they are wired together.

**`docs/ARCHITECTURE.md` is the reference - read a module's entry there before
editing it.** Most of what it records is a constraint someone found the hard
way, and several have silent failure modes. It also holds the four patterns that
recur across the tree (pure/IO split, argv-as-data, sanitize-anything-external,
absent-is-not-zero); new code is expected to follow them.

| Module | Job |
| --- | --- |
| `main.rs` | `mod` declarations and `cli::run()` |
| `cli.rs` | every `clap` definition and all dispatch |
| `http.rs` | `ApiError`, `map_ureq_error`, `agent()` - shared by the two ureq clients |
| `beaconapi.rs` | typed client over a curated set of Beacon API endpoints |
| `metrics.rs` | Prometheus scrape fetching and exposition parsing |
| `curl.rs` | the HTTPS boundary, shelled out to `curl` |
| `output.rs` | pure formatting functions, plus the `--json` convention |
| `logfmt.rs` | one log line in, one colorized line out |
| `logs.rs` | `tekops logs`: target resolution, streaming, process lifetime |
| `dump.rs` | `tekops dump-logs` |
| `redact.rs` | the anonymiser behind `dump-logs` |
| `gist.rs` | gist semantics for `dump-logs --gist` |
| `loglevel.rs` | what `tekops log-level` sends, and where it came from |
| `doctor.rs` | `tekops doctor`: `probe` collects, `evaluate` judges |
| `host.rs` | host resource facts for `doctor` |
| `docker.rs` | `docker inspect` for `doctor` |
| `stack.rs` | `Stack` and `detect_stack`; per-stack ports and container naming |
| `protocol.rs` | classifies a peer's transport from its multiaddr |
| `config.rs` | the `~/.config/tekops/config.toml` file |
| `completions.rs` | `tekops autocomplete` |
| `update.rs` | `tekops update` |
| `term.rs` | `sanitize`: making untrusted text safe to print |
