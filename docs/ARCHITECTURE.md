# Architecture

Each module has one job; `cli.rs` is the only place they are wired together.
This document is the per-module reference: what each one owns, and the reasoning
behind the decisions that are load-bearing. **Read a module's entry before
editing it.** Most of what is here is a constraint someone found the hard way,
and the failure mode for several of them is silent.

`CLAUDE.md` holds the module index and the conventions that span modules.
`USAGE.md` documents the commands themselves. `docs/RELEASING.md` covers
building, the local gate and shipping.

## Standing decisions about scope

- **There is deliberately no `beacon` subcommand tree.** It held `beacon
  validators <index-or-pubkey>...` and `beacon duties attester|proposer`, and
  was removed along with the `/eth/v1/beacon/states/head/validators` and
  `/eth/v1/validator/duties/*` client methods that backed it. New Beacon API
  work lands as a top-level command that answers a question an operator
  actually asks, not as a nested passthrough of an endpoint.
- **Execution-client log support was removed** - see issue #3. `logs` takes a
  path or a container and colorizes Teku output; it is not a general log tailer.
- **`log-level` is the only mutating command.** Everything else is read-only.
  A new command that writes to the node is a decision, not a detail.

## Patterns that recur

Four shapes repeat across the tree. A new module is expected to follow them, and
most of the entries below are an instance of one.

**Split pure from I/O.** `logs::resolve_log_sources`, `completions::plan`,
`stack::detect_stack`, `logfmt::format_log_line`, `docker::parse_inspect`,
`config::parse`, `doctor::evaluate` and the `host.rs` parsers all take every
input as a parameter, with a thin I/O wrapper as the only thing that touches the
filesystem, the environment or a subprocess. That is what makes behaviour
testable with no environment races, no Docker and no node - on either platform.

**Return argv as data, not a spawned `Command`.** `logs::producer_argv`,
`docker::inspect_argv` and `curl::curl_argv`/`post_argv` all do this, so the
shape of what gets run is assertable in a unit test.

**Sanitize everything that came from outside this binary.** Log fields, scrape
values, `docker inspect` output, node error bodies and curl's stderr all reach a
terminal. `term::sanitize` is the one place that is handled; `doctor::Finding::new`
and `LogLevelError::malformed` apply it in their constructors so call sites
cannot forget.

**Absent and zero are different answers.** This repo has been bitten by
conflating them and keeps designing against it. `metrics::require_metric` fails
when a metric family is missing from a scrape rather than summing to a confident
zero; `doctor` renders that as a wrong-port diagnosis; the `host.rs` parsers
return `None` for input they could not read, which `doctor` shows as an omitted
check rather than a failure. Reporting a measured zero for something that was
never measured is the failure mode to avoid.

---

Some modules also carry a short `//!` header in the source. Those orient someone
who has the file open; this document is the full reference, and is where a new
constraint belongs.
## `main.rs`

`tekops`: a single-binary CLI for operating a Teku Ethereum node, run
directly on the node over SSH.

Each module has one job; `cli.rs` is the only place they are wired together.
This file is just the `mod` declarations and `cli::run()`.

## `cli.rs`

Every `clap` definition and all dispatch. The only place the modules are
wired together.

Every command is a leaf - there are no subcommand trees at all - and they
all follow one shape: call one or more client methods, then either print a
`serde_json`-serialized struct (`--json`) or hand the result to an
`output::format_*` function.

### Error handling

`exit_for` centralizes it: each handler returns `Result<(), ApiError>`, and
`exit_for` prints `error: {e}` and sets the exit code. Every command that
talks to a node over HTTP goes through `exit_for_api` instead, which does
the same and appends a stack hint (`hint: detected a rocketpool stack; try
--stack rocketpool...`) when the error is `ApiError::Unreachable` - "could
not reach endpoint" on a Docker host almost always means the ports are the
default bare-metal ones. `exit_for` itself still serves `update` and
`autocomplete`, neither of which talks to the node, so a stack hint would
never be the right addition to their error path.

### Commands dispatched outside the usual path, and why

- `log-level` goes to `run_log_level` rather than straight to
  `exit_for_api`, because its exit code comes from two different error
  channels. A body that could not be fetched or parsed is a `LogLevelError`,
  not an `ApiError`, and must not go through `exit_for_api`, whose
  `--stack` hint would point at the node's ports for a failure that
  happened at a gist. Only the request itself is handed to `exit_for_api`.
  `resolve_spec` holds the fetch-preview-confirm sequence and
  `confirm_apply` prompts on **stderr**, not stdout, so `--json` still
  writes nothing but JSON to stdout; declining sends nothing and exits 0.
  Nothing is fetched and nothing is prompted for a level typed on the
  command line.
- `about` is dispatched inline in `run()` and returns `ExitCode::SUCCESS`
  directly rather than going through `exit_for`, since the handler cannot
  fail.
- `doctor` is dispatched inline for a related but distinct reason: its exit
  code is a function of the findings `doctor::evaluate` returns, not of a
  `Result`, so neither `exit_for` (which needs an `Err`) nor `exit_for_api`
  (which needs `ApiError` specifically, and would print a `--stack` hint
  doctor has already made moot by detecting the stack itself) can express
  it. `run_doctor` builds a `doctor::ProbeConfig`, calls `doctor::probe`
  then `doctor::evaluate`, and returns `ExitCode::FAILURE` iff any `Finding`
  is a `Fail` - see `exit_for_findings`.
- `autocomplete` is dispatched inline too. `run_autocomplete` holds
  everything environment-dependent (the `$SHELL` detection, `dirs_from_env`,
  the preview, the prompt) so `completions` itself stays pure.
- `logs` is the one command whose handler does **not** live here: `run_logs`
  and `supervise_pager` are in `logs.rs`, next to the `emit_records` loop and
  the buffer cap they drive. This file only parses the positional and flags
  (`--container`, `--stack`) and calls `logs::run_logs`.

### Flags and configuration

The repeated `--api-url`/`--bn-metric-url`/`--vc-metric-url`/`--json` flags
live in three flattened arg structs - `ApiArgs`, `MetricArgs`, and
`VcMetricArgs` - rather than being restated per command (`doctor` declares
its three metric flags directly instead of flattening `MetricArgs`, since
that struct also declares `stack` and `json` and flattening both would be a
duplicate arg id). `MetricArgs` carries both the beacon-node and
validator-client spellings and is `version`'s alone, the one command that
reads both processes. `VcMetricArgs` carries only `--vc-metric-url`, for
`duties` and `validators`: the beacon-node spellings name an endpoint those
two never read, so `MetricArgs` there would let them parse and do nothing -
`VcMetricArgs` makes clap reject them instead. All four - the three structs
and `doctor`'s direct flags - carry `--stack`, which `resolve_stack`,
`resolve_base_url`, `resolve_bn_metric_url` and `resolve_vc_metric_url` fold
into the URL a flag, an environment variable, or the config file didn't
already supply.
`resolve_bn_metric_url` has an extra rung the other three don't: the
deprecated `--metric-url`/`$TEKOPS_METRIC_URL`/`metric_url` trio, tried
after their `bn_`-prefixed replacements and before the config file's
`bn_metric_url` (see the `config.rs` entry for why that key still exists
and what it now means). `resolve_vc_metric_url` has no such rung - an
operator migrating off `metric_url` has to say `vc_metric_url` explicitly,
which is the point. The config sits below the environment in all four, and
each resolver takes the environment as a parameter rather than reading it
itself precisely so that ordering is testable.

The config is loaded once by `run()` **before dispatch**, so a malformed
file fails every command including `about` and `update`, which read none of
the ten keys: the file being broken is a fact about the installation rather
than about one command, and reporting it from whichever command the operator
runs first is the shortest path to fixing it. That failure prints directly
and does **not** go through `exit_for_api`, whose `--stack` hint would point
at the node's ports for something that happened in a file - the same
distinction `run_log_level` draws for a bad fetched body.

`logs` gets its own `--stack` on the `Logs` variant instead of an arg
struct, since there it narrows `docker ps` detection to one stack's naming
rather than setting a URL. Neither struct's fields mark `global = true` any
more: that existed only so trailing flags kept parsing on subcommands, and
with no subcommand tree left it was a no-op on every remaining command.

### Three version surfaces, all deliberate

`#[command(version)]` on `Cli` makes `tekops --version` report the tekops
build - a different question from `tekops version`, which reports the
running Teku's. `tekops about` is the third and overlaps `--version` on
purpose: it prints that same build number as part of a three-line credit,
and is the only command with no arg struct at all (no `--json`, no URL
flag - it fetches nothing, so there is nothing to point at or serialize).

### `docker ps` spawns

Every one in this file - `logs`'s `needs_detection` arm, `run_doctor`'s
detection rung and its container lookup, and `docker_stack_hint` - goes
through `docker_ps_names`, which reports a `docker ps` that *fails* only
when `should_report_unaskable_docker` says the stack already in force is one
that has containers. Docker missing from `$PATH` stays silent always; Docker
present and refusing (daemon down, not in the `docker` group) is news on
eth-docker or rocketpool, where doctor's container check is about to come up
empty because of it, and noise everywhere else. `docker_ps_names` used to
decide this itself, but it spawns *before* the stack ladder resolves and so
cannot tell the two apart: a bare-metal node with a stopped daemon got a
note about Docker printed directly above a report whose header says
bare-metal - issue #16. `tests/doctor_docker_note.rs` pins it end-to-end
with a `docker` stub on `$PATH`, since the defect lived in the wiring
between the two and no single function had both facts in hand to be wrong
about.

Spawning is gated so a fully-configured operator, or a bare-metal one, never
pays for it. `needs_detection` is a single extracted predicate with its own
doc comment and per-clause tests, called by both the `Logs` and `DumpLogs`
arms - it used to be two hand-inlined copies of the condition, which is two
places to forget a rung rather than one. It still has to track every branch
of `logs::resolve_log_sources`'s precedence ladder that fires before its
`detected` parameter, because it is that ladder's logical negation living in
a different file, and nothing ties the two together at compile time; the
extraction removes the duplication, not the drift risk, and the tie is a
comment cross-reference in each direction. The failure mode is silent - a
rung added to the ladder but not to the predicate costs a `docker ps` spawn
whose answer can never be used, which is not an error and not a wrong
result - so the per-clause tests exist to catch the predicate *shrinking*
even though nothing catches the ladder *growing*. `doctor` has its own
equivalent, `doctor_needs_docker_ps`, extracted for the same reason - but it
asks a different question. `logs` skips the spawn once anything has answered;
`doctor` skips it only for bare-metal stated outright, because it needs two
container *names* and neither is knowable from a stack alone.

### Doctor's stack ladder, and why it prints no hint

Every other command resolves the stack from `--stack`/`$TEKOPS_STACK`/the
config file only, and suggests `--stack` on an unreachable endpoint (see
`exit_for_api`). `run_doctor` instead applies the full `logs`-style ladder -
`--stack` > `$TEKOPS_STACK` > the config file > `docker ps` detection >
`Stack::BareMetal` - before it ever builds a `ProbeConfig`, because it is
already running `docker ps` for the container checks and detection costs
nothing extra at that point. The `Stack` that lands in `Facts` is therefore
already the detected one, so by the time an endpoint check could render
`Fail` there is nothing left for a hint to suggest.

It runs that ladder twice, once per process, from one `docker ps`:
`resolve_doctor_stack` for the beacon node and `resolve_doctor_vc_stack` for
the validator client. **The asymmetry between them is load-bearing, because
the two absences mean opposite things.** A validator container with no
consensus container beside it is a beacon node running somewhere else - Rocket
Pool calls it External Consensus Client mode - so the beacon node's ladder is
right to terminate in bare-metal, and dragging it onto the validator's stack
would probe `:5052` and `:9100` on a node serving `:5051` and `:8008`. A
consensus container with no validator beside it is the ordinary combined
deployment, where Teku runs both processes in one container: that container's
stack is the validator's stack too, which is why `resolve_doctor_vc_stack`
takes the consensus detection as its last rung before bare-metal. Terminating
in bare-metal there would hand an eth-docker node the bare-metal validator
metrics port (8010) instead of its own (8009).

Both are extracted and tested directly, because `doctor_probe_config` cannot
be tested without Docker and the failure mode is a silently wrong port rather
than an error.

### `command()`

Exposes the `clap::Command` for the whole CLI, and exists only so
`completions` can render a script from the same definition that parses
arguments.

## `http.rs`

Shared HTTP plumbing for the two `ureq`-backed clients.

`beaconapi.rs` and `metrics.rs` are both thin `ureq` wrappers with
identical failure modes, so the `ApiError` enum, `map_ureq_error` and
`agent()` live here rather than in whichever one needed them first. Error
text is endpoint-agnostic ("could not reach endpoint: ...") rather than
naming the Beacon API, precisely because `metrics.rs` reuses it.

**Both clients hold an `agent()` and must go through it - never call
`ureq`'s free functions (`ureq::get(...)`) directly.** `ureq` defaults
`timeout_read`/`timeout_write` to `None`, so a node that accepts a
connection and then never answers - an overloaded or wedged Teku, i.e.
exactly when these commands get run - hangs the CLI forever with no output
and no error. Reproduced against a black-hole socket. `agent()` is the
single place `REQUEST_TIMEOUT` is applied, and a call site that bypasses it
silently reintroduces the hang.

`map_ureq_error` runs the response body through `term::sanitize` before it
lands in an error, since that body is untrusted and gets printed to stderr.

**`ureq` is declared `default-features = false` in `Cargo.toml`, which
drops its TLS backend on purpose** - that is not an oversight to helpfully
undo. Every endpoint tekops talks to is a node-local `http://` URL, and
removing TLS takes `rustls`, `ring` and all C compilation out of the build:
3,114,960 bytes to 1,849,344, and 22 fewer dependencies. The cost is that
an `https://` URL fails with `cannot make HTTPS request because no TLS
backend is configured` and exit 1, which is clear enough that no bespoke
check was added for it. Re-enabling is a one-word change if a remote HTTPS
node ever becomes real. Everything that does need HTTPS goes through
`curl.rs` instead - read its entry below before concluding this left a gap.

## `beaconapi.rs`

`BeaconClient`: a typed wrapper around a curated set of Teku Beacon API
endpoints, backed by `ureq`.

`get_json`/`put_json` wrap the request and JSON-decode boilerplate (via
`http::map_ureq_error`). Every endpoint method goes through one of them
rather than hand-rolling a `ureq` call. `put_json` - used only by
`set_log_level` - discards the response body, since Teku's admin endpoints
return an empty one on success. There was a third, `post_json`, and it went
when its only caller did; a new POST endpoint should bring it back rather
than hand-roll one.

Wire response shapes - the nested `{"data": {...}}` envelopes and so on -
are private structs flattened into the public ones (`SyncingStatus`,
`BlockHeader`, `PeerInfo`, ...), so callers never see the wire nesting.

`LogLevelRequest`'s `log_filter` is `Option<Vec<String>>` with
`skip_serializing_if`, so a global level change omits the key from the body
entirely rather than sending `null` or `[]`. The tests assert the real wire
body with `mockito::Matcher::Json`, not merely that the call succeeded.

**No endpoint here takes a caller-supplied query value any more**, which is
why there is no `percent_encode` helper. The removed `validators()` had
one: it encoded each id individually and joined on a literal unencoded
comma (the Beacon API's own list separator), because interpolating ids raw
let an id containing `&` or `=` forge extra query parameters onto the
request. Any new method that puts caller input into a URL has to
reintroduce that rather than interpolate.

## `metrics.rs`

`MetricsClient`: reads a Prometheus text-exposition page and answers
questions about it locally.

The endpoint is Teku's own `/metrics` scrape page, not a Prometheus
server's `/api/v1/query` HTTP API, so there is no query engine behind it
and the parsing is done here by a small hand-rolled exposition parser
(`parse_exposition`/`Sample`, private).

Three aggregation helpers mirror what PromQL would do in Grafana:
`matching_value` sums samples matching an exact set of label matchers (used
by `duties()`, one call per method); `sum_by_label` groups-and-sums by a
label's value (used by `validators()` for the per-status counts) alongside
a plain `sum_all` for the total balance; and `distinct_label_values`
collects every distinct value of a label across matching samples (used by
`families()`, for "info"-style metrics whose value is always 1 and whose
label carries the real data). `validators()` converts the summed balance
from Gwei to ETH (`/ 1e9`).

`families()` reads both `beacon_teku_version_total` and
`validator_teku_version_total` off one scrape and returns whatever it found
of each, plus `has_validator_families` (whether the validator counts family
is present). It exists because `version` and `doctor` both need several
facts about one endpoint - which version(s) it exports, and whether it's a
validator client - and the per-question methods above (`duties()`,
`validators()`) each do their own `fetch()`, so getting the same answers out
of them would mean scraping the same URL more than once for one report.
`has_validator_families` rides along rather than living on its own because
it's what tells apart a dual-role process (exports both beacon and validator
families) from a validator client sitting where a beacon node was expected
(exports only the validator ones) - `doctor`'s "metrics layout" check reads
exactly that distinction.

Three parser details are load-bearing and were each a real bug:

- The closing `}` of a label set is found by `find_closing_brace`, which
  respects quotes and backslash escapes. A naive `find('}')` truncated the
  label set on a value like `version="teku/v25.1.0 {dev}"` and silently
  dropped the whole sample, making `tekops version` return an empty list
  with exit 0.
- `parse_labels` splits on commas with the same quote/escape awareness and
  runs values through `unquote`.
- `parse_line` **drops non-finite values**. `NaN`/`+Inf`/`-Inf` are legal
  exposition values that Rust's `f64` parser accepts happily, and one `NaN`
  balance turned a real 32 ETH total into `NaN` (table) / `null` (JSON)
  with exit 0.

Separately, `require_metric` makes `duties` and `validators` **fail when
the metric family is absent from the scrape entirely**, rather than summing
to a confident zero. Absent and present-but-zero are different answers, and
conflating them meant pointing at the beacon node's metrics port instead of
the validator client's rendered as "this validator published nothing" - the
alarm reading, from a healthy node. `families()` doesn't route through
`require_metric`: an absent version or validator family is exactly the fact
`version` and `doctor` are asking about, so `families()` reports emptiness
rather than erroring, and it's `cli.rs`/`doctor.rs` that decide what an
absence means for their own report.

## `curl.rs`

The HTTPS boundary, shelled out to `curl`.

tekops drops ureq's TLS backend on purpose (see the dependency comment in
Cargo.toml), so anything that has to speak HTTPS - GitHub Releases for
`update`, a gist for `log-level` - cannot use the ureq clients and needs a
transport from outside this binary. `curl` is documented as a runtime
dependency alongside `tail`, `less`, and `tar`.

This module exists for the same reason `http.rs` does: it was `update.rs`'s
private plumbing until a second command needed HTTPS, and transport policy
belongs in one place rather than in whichever module happened to need it
first. Re-enabling rustls to remove the curl dependency would undo the
measured 3,114,960 to 1,849,344 byte reduction and put ring's C and assembly
back into the musl cross-compile path.

`fetch` is the single boundary, the same way `http::agent()` is for the
ureq clients. `curl_argv` passes `--proto '=https'` so the redirect to
`objects.githubusercontent.com` (or `gist.githubusercontent.com`) that `-L`
follows cannot downgrade to http - verified against a real redirect, which
curl refuses with `Protocol "..." disabled (in redirect)`. curl's stderr
goes through `term::sanitize`.

**`curl_argv` carries a stall bound (`--connect-timeout`, `--speed-limit 1
--speed-time`) for the same reason `http::agent()` carries
`REQUEST_TIMEOUT`**: curl applies no timeout by default, so a host that
accepts the connection and never answers hung `tekops update` forever with
no output at all, since `-s` suppresses even the progress meter - reproduced
against a black-hole socket, exactly as on the ureq side. It is a stall
bound rather than `--max-time` because a release tarball is megabytes and a
slow-but-progressing download must not be killed by a wall clock.

### The write half

`post_argv`/`post_file` were added for `dump-logs --gist` and mirror
`curl_argv`/`fetch_with` deliberately - `--proto '=https'` and `stall_argv`
apply unchanged, and `a_post_keeps_the_stall_bound_and_the_protocol_pin` is
the sibling of the capped-GET test so neither invariant rests on review
attention. Three things differ from the GET and each is load-bearing:

- **A credential reaches curl only via `--config <path>`, never `-H`**,
  because `/proc/<pid>/cmdline` is world-readable and this binary runs on a
  machine running a validator. `a_post_never_carries_the_credential_in_argv`
  pins it.
- **`--data-binary @<path>`, never `-d @<path>`.** `-d` strips CR and LF out
  of `@file` content, which is usually survivable for JSON because those are
  inter-token whitespace, but not for a body built from arbitrary text - the
  same class as the naive `find('}')` truncation `metrics.rs` documents.
  Reading the body from a file also keeps a payload past `ARG_MAX` possible.
- **`-f` is deliberately omitted**, so curl exits 0 on a 4xx and the
  *caller* decides what a non-2xx means; `-f` discards the response body,
  which for an API is where the actionable message is. The status therefore
  travels in-band via `-w '\n%{http_code}'` as a final line the caller splits
  off, and `CurlError::PostFailed` exists rather than reusing `Failed`
  because that variant's wording is a download's. `--fail-with-body` would
  collapse this but needs curl >= 7.76, and Debian 11 ships 7.74.

### Why `fetch` is uncapped and `fetch_capped` is not

That asymmetry is the point. A release tarball has no knowable ceiling, so a
limit picked today would eventually fail an update for being right about the
wrong release. A fetched log-level body is a few hundred bytes, is held
entirely in memory, and is named by a URL an operator pasted - so it gets
`--max-filesize`, because tekops runs on the machine running the validator
and a mistyped URL must not cost that host its RAM. The cap is added to the
same argv every fetch gets rather than to a fresh one; building it
separately would drop the stall bound and the protocol pin, which is what
`a_capped_fetch_keeps_the_stall_bound_and_the_protocol_pin` exists to catch.
curl aborts on the declared `Content-Length` before transferring, so an
obvious mistake costs one round trip - confirmed against a real 119 KB gist,
which came back `size_download=0`.

## `output.rs`

Pure formatting: already-fetched data in, a `String` out. No I/O.

`comfy-table` backs all of them except `format_about`, which returns three
lines of prose - it is the one command whose output is a message rather than
fetched data, so a box around it would add nothing. Its version comes from
`env!("CARGO_PKG_VERSION")` rather than a literal, so a release bump cannot
leave `about` reporting a build that never shipped; a test asserts exactly
that, and hardcoding the number back is what it catches.

`format_health_table`, `format_head_table` and `format_duties_table` are
Field/Value tables, one row per field on the single fetched object.
`format_peers_table`, `format_validator_metrics_table` and
`format_version_table` print grouped summaries - direction/protocol with
counts, status with counts, and one row per distinct version - rather than
one row per underlying item; `format_validator_metrics_table` additionally
prints a second, separate Field/Value table for the total ETH figure.

`format_doctor_report` is deliberately plain aligned text rather than
`comfy-table`, since the Discord-paste case is a stated goal of that command
and box-drawing characters are noise there. A test asserts the rendered
report contains zero ESC bytes.

Its header (`doctor_header_summary`) collapses to one stack and one version
when the two processes agree, and names both when they don't. The split is
what makes a separated deployment legible - a Rocket Pool validator against a
bare-metal beacon node reads as plain `bare-metal` otherwise - and it is
driven by disagreement rather than by the stacks differing, so a half-finished
upgrade shows up on an all-bare-metal node too. **Neither process borrows the
other's version**: an earlier version preferred the beacon node's and fell
back to the validator's, which on a separated node states a fact nothing
measured. An endpoint that didn't answer reads `version unknown`, and a
combined deployment scrapes one endpoint into both slots, so it still agrees
with itself and still collapses.

### `--json` output

Every command's `--json` output is `#[derive(Serialize)]` plus
`serde_json::to_string` on typed structs - **never hand-built with
`format!`**. An earlier version did it by hand and it was both fragile
(invalid JSON on any field containing `"`) and a silent data-corruption bug
(an unusual or malicious field value could forge extra JSON keys). A new
command's `--json` output derives `Serialize` on its response struct.

## `logfmt.rs`

One log line in, one colorized line out.

A pure function with no I/O, so the whole of it is unit-testable in
isolation. `format_log_line` tries each known layout in turn, most specific
first: JSON, then Teku's console layout, then a sanitized passthrough.

The JSON is **not** a Teku default - it is a custom log4j2 config, so
whoever set the node up chose to log that way. The console layout is what
both Docker stacks actually run Teku with (`--log-destination=CONSOLE`),
which is why it exists at all: a bare-metal deployment only ever had to
handle the JSON side, and Docker made the console layout a real case rather
than a hypothetical one.

`parse_console`'s level check (one of `ERROR`/`WARN`/`INFO`/`DEBUG`/
`TRACE`/`FATAL`) is what stops it claiming arbitrary `" - "`-containing
prose as a log record. The level check alone would accept `some text INFO -
hello`, so it is paired with a second guard on the timestamp's shape.

Every extracted field goes through `term::sanitize` first, and so does the
raw line on the final passthrough path; the only escape sequences in the
output are the two colour codes `format_log_line` itself wraps the line in,
and a test asserts exactly that count. When writing tests here: raw control
bytes inside a JSON string are invalid JSON, so a test literal must spell
the escape out (a backslash followed by u001b) or it silently exercises
the passthrough path instead of the parsed-field path.

## `merge.rs`

Ordering two log sources into one timeline. Pure: no threads, no IO, no clock
of its own - every instant arrives as a parameter. `logs.rs` owns the threads
that feed it.

**The merge unit is a record, not a line.** A record is one timestamped line
plus the untimestamped lines that follow it from the same source. Teku logs a
stack trace as one timestamped line followed by continuations carrying no
timestamp, so sorting lines would scatter a trace through the other source's
output. This is a correctness requirement, not an optimisation: a line-level
merge is wrong on any node that logs an exception.

**Under `tekops logs` there is no EOF**, so anything a record holds is held
until something other than the producer releases it. Both producers follow
forever. Three things close a record, and the third exists because the first
two are not enough:

1. the next timestamped line from that source,
2. `Merger::eof`, which `dump.rs` reaches for real and `logs.rs` only when a
   producer dies,
3. `Merger::flush_stale`, once the source has been silent for `MERGE_WINDOW`.

Without (3) the newest line of every source was unreachable: a timestamped
line opens a record that only the *next* one can close, so the operator saw
everything except the thing they were watching for - a whole slot of lag on a
quiet beacon node, and permanent on a source that stopped writing. A stack
trace is unaffected because log4j writes its lines in one burst, which keeps
refreshing the source's clock; only genuine silence flushes.

The same hole, in its severe form, is why (1) alone is dangerous: if *no* line
parses, every line looks like a continuation and the whole source accumulates
in `pending`, showing nothing at all with no error anywhere. A stock bare-metal
Teku did exactly that, because `leading_timestamp` looked for `@timestamp`
while Teku writes `timestamp`. So `push_line` also emits an untimestamped line
immediately **until its source has produced a first recognised timestamp** - a
line that nothing precedes cannot be a continuation. An unparsed layout now
degrades to out-of-order output rather than to silence. Anything added here
that can hold a record back needs the same question asked of it: what releases
this when the producer never ends?

**The order is the times the processes printed, taken at face value - and
`Merger` says so when that stops meaning anything.** Neither of Teku's common
layouts states a timezone, so a bare-metal beacon node logging local time and a
validator container logging UTC produce stamps that cannot be compared: every
line of one sorts before every line of the other.

Correcting for that was built and then removed, and the reason is worth
keeping. `Merger` estimates each source's offset from this host's clock as
`min(arrival - stamp)` over its lines - the minimum because delivery delay is
never negative, which makes the smallest sample the least distorted and
incidentally rejects a backlog in favour of the first live line. But that
estimate **cannot distinguish a source whose clock is an hour behind from a
source whose newest line is an hour old.** A live session settles it, since the
next line written is a fresh sample; `dump-logs` has no live phase, and acting
on the estimate there reordered a dump that had been correct - caught only by a
pre-existing test. Ordering a node's logs on a guess is a worse failure than an
ordering the operator can explain, so the estimate now feeds one thing only:
`check_skew`, which reports.

`as_zone_offset` is the bar for reporting: at least fifteen minutes, and within
a second of a multiple of one, which every real timezone is and a stale source
is only by coincidence. Below it is delivery jitter. Above it, `take_skew_note`
hands `logs.rs` and `dump.rs` a single line naming both processes, the size of
the disagreement and the `%d{ISO8601}{UTC}` that fixes it - written into the
output rather than to a stderr the pager paints over.

`logs.rs` and `dump.rs` supply the arrival wall-clock for the estimate;
`Instant` cannot serve, being comparable only to itself.

**The `RecordBuilder`s live in `Merger`, not in the reader threads.** A reader
blocks on `read_line`, so it can only ever act when a line arrives - precisely
the wrong property for a record that needs releasing *because* no line has
arrived. The emitter already ticks every `EMIT_TICK`, so it calls
`flush_stale(now)` before each `drain_ready(now)`. Readers now hand over raw
lines (`merger.push_line(source, raw, at)`) and own no merge state at all.

**The rule.** Each source's own records are non-decreasing in time, so only
the head of each queue matters. Emit the earliest head when every *other*
source either has a head to compare against, or has been silent longer than
`MERGE_WINDOW`, or is at EOF. A source with an empty queue that is neither
idle nor finished may still deliver something older, and emitting past it is
the exact inversion this exists to prevent.

Silence is measured on **lines arriving** (`last_line`), not on records being
completed. A source part-way through a long stack trace has an empty queue and
nothing finished, but it is plainly not idle, and emitting past it would drop
the other source's output into the middle of the trace.

That one rule covers both the `-n` backlog and live follow with no mode
switch. `tail -n 500` and `docker logs --tail 500` deliver their backlog as an
immediate burst, both queues fill, and the same comparison sorts it exactly.

**`Merger::new` takes `started_at` rather than exposing a `start()` method**,
so a source's idle clock cannot be left unseeded. An unseeded source is never
idle, which would hang the other source's entire backlog forever on a quiet
node - a silent hang, the worst failure this type could have, so the type
makes it unrepresentable.

**Monotonicity is enforced, not assumed.** `RecordBuilder` clamps a timestamp
that moves backwards to its predecessor, and a record whose first line has no
parseable timestamp inherits the previous one's. Both keep the per-source
queue ordered, which is what the head-only comparison depends on; without the
clamp a clock change would corrupt the merge rather than misplace one line.
`Merger::push` is deliberately a plain `push_back` and does **not** sort
defensively - that would cost time on every record and, worse, would mask a
`RecordBuilder` that stopped clamping, making the invariant untestable.

Ties break toward the source declared first, so output is stable run to run
and a dump diffs against itself.

## `logs.rs`

`tekops logs`: resolving what to tail, streaming it, and the whole session's
process lifetime.

Holds `DEFAULT_TEKU_LOG` (the path the old bashrc function tailed),
`LogTarget` (`File` or `Container`), `LogSources` and `resolve_log_sources`,
`emit_records` and the buffer cap it drives, plus the command handler itself
(`run_logs`) and `supervise_pager`. The last two were moved out of `cli.rs`,
which had grown to hold the clap definitions, every command handler, *and*
the subtlest process-lifetime code in the tree; that lifetime is only
intelligible next to the emitter and `MAX_BUFFER_BYTES`, which is this file.
`cli.rs` keeps only the parsing.

### Two ladders, three rules

`resolve_log_sources` resolves one source per process into
`LogSources { bn, vc }`. Each slot runs the same seven-level ladder the single
source used to: the flag or positional path beats the environment variable,
which beats the config file, which beats `docker ps` detection, which beats
the hardcoded default; within a tier, naming a container beats naming a file.

The principle is flags beat environment beats config beats detection beats
hardcoded default. Config sits below the environment because a variable is
the more specific act - it was typed for this session - and above detection
because a value the operator wrote down beats one tekops guessed. Detection
sits below the positional path specifically so naming a file on a host that
also happens to run Docker still reads that file rather than tailing an
unrelated container, and above the hardcoded default because that default
path doesn't exist on a Docker host.

Three rules sit on top, and each stops a specific wrong answer:

1. **Naming one side by flag or environment variable yields that side only.**
   Without it, `tekops logs --container rocketpool_validator` would start
   printing a second stream on any host where detection also found a consensus
   container - a silent change to what an existing command prints. **The
   config file deliberately does not count here.** A flag or a variable is
   typed for this invocation; config is ambient and describes the node. Were
   config to gate this rule, the deployment the feature exists for - a
   bare-metal beacon node with `logs_file` configured, beside a Rocket Pool
   validator container - would silently show one stream and never the
   validator, with no flags set and nothing in the output to say so.
2. **The hardcoded default answers the bn slot when that file is actually
   there**, and otherwise only as a whole-command last resort. A bare-metal
   beacon node runs under no container, so `detect_stack` cannot see it: on a
   host running one beside a Rocket Pool validator, an operator with no config
   file has nothing that can fill the bn slot except the path existing. The
   existence gate is what still spares a pure Rocket Pool node a "file not
   found" for a path it never had, and the whole-command fallback is what keeps
   that message printing when nothing resolved at all. `cli.rs` does the read
   (`default_teku_log_present`) and passes the answer in, like every other
   input. Without the per-slot half, `tekops logs` on a separated deployment
   printed the validator alone and `tekops logs --bn` failed outright.
3. **`--bn`/`--vc` filter after both ladders run.** They name nothing, so they
   cannot interact with rule 1. Because rule 2 runs before rule 3, a selector
   can legitimately leave both slots empty; `cli::check_selection` catches that
   and names the missing process rather than opening a default path or
   presenting an empty session.

Every input arrives as a parameter rather than being read inside the
function, the same shape `completions::plan` uses for the same reason: the
whole ladder is testable with no environment races and no Docker installed.
`cli::needs_detection` is this ladder's logical negation, and since the split
the negation is **per-slot**: detection is skippable only when both slots are
answered. A configured beacon node alone must not suppress the spawn, because
the same `docker ps` is what finds the validator container beside it.

### One emitter, two producers

`run_logs` spawns one producer per resolved source, gives each a reader thread
feeding a shared `Merger`, and runs a single emitter thread writing the merged
result into the file `less` reads.

A reader hands over **raw lines** (`push_line`) and holds no merge state of its
own; the `RecordBuilder`s live in the `Merger`. That is deliberate and is
explained under `merge.rs`: a reader blocks on `read_line`, so it cannot be
what releases a record that is waiting on silence. **The emitter's tick has two
jobs**, `flush_stale(now)` then `drain_ready(now)`, and dropping the first
brings back the lag where the newest line of a quiet source is never shown.

**The emitter is the sole writer**, which is what keeps `MAX_BUFFER_BYTES`
bounding the session as a whole. Letting each producer write its own would
split the cap between them and quietly halve it - the same failure
`producer_argv`'s doc comment was already avoiding when it chose to merge
stderr in the shell rather than add a second writing thread.

The emitter polls rather than blocking on a pipe, so closing the producers'
stdout says nothing to it. Hence the `stop` flag, set by `supervise_pager`
after the kills and before the joins. A reader that dies on an IO error still
records EOF, so only a panicking one strictly needs the flag - but without it
the join would hang on a thread that never returns, with the terminal already
handed back.

`emit_records` tags a line `[bn]`/`[vc]` **only when more than one source is
active**, so a single-source session's bytes are exactly what they were before
any of this existed. Continuation lines are tagged too: a stack trace whose
second line lost its tag would read as the other process's output.

**A source that cannot start is recorded rather than fatal**, so a separated
node does not lose its working half to a beacon node whose default log path is
absent. The reasons are only promoted from `note:` to `error:` when no source
starts at all, which is what keeps a lone missing file reporting exactly the
message it always did.

### Scrollback depth

`-n`/`--lines` defaults to 500 and is interpolated straight into the `tail
-n` (or `docker logs --tail`) invocation, so `0` is legal and means "follow
only new output" rather than being a value to validate away. It is a `u32`,
so clap rejects negatives itself. Those pre-loaded lines pass through the
merger and `emit_records` into the temp buffer like any other, so they count
toward `MAX_BUFFER_BYTES` and are searchable from the start of the session -
at a few hundred bytes per Teku line, even a large `-n` is nowhere near the
cap. **`-n` is per source**: on a separated deployment it is the last N lines
of each, merged, so a two-source session buffers up to twice as much.

### Streaming and the buffer cap

`emit_records<W: Write>` is generic over its writer specifically so it is
testable with in-memory buffers instead of real subprocess pipes, and takes
`max_bytes` as a parameter for the same reason - the cap's behaviour is
pinned with an injected small limit rather than by pushing 256 MiB through a
real `tail`. `emit_loop` passes `MAX_BUFFER_BYTES` (256 MiB).

The cap exists because the buffer is a real file that only ever grows, and
`/tmp` is tmpfs - RAM - on most systemd distros, on the machine running the
validator. Stopping is the only bound that keeps the pager coherent:
truncating a file `less` holds offsets into corrupts what it displays.
Hitting the cap emits an in-band notice and returns `Ok`, since the session
still works as scrollback.

**The running byte count is carried across calls**, because the cap bounds the
session and not one batch. With two producers that is the whole point: a cap
applied per batch, or per producer, would silently stop bounding the thing it
names.

`producer_argv` builds the argv for whichever of `tail -F` or `docker logs
-f` the resolved target needs (the container arm goes via `sh -c` with
positional arguments, so a hostile container name cannot become shell code),
returned as data rather than a spawned `Command` so the shape is testable.
It takes a `Mode`: `Follow` is what `run_logs` needs, and its whole
process-lifetime design rests on the producer never reaching EOF; `Once`
drops `-F`/`-f` for `dump.rs`, whose producer must end on its own. The two
share one function rather than being two, because the container arm's
positional-argument shape (`"$1"`/`"$2"`) is the injection guard and a
second copy of it is a second place to get it wrong.

### Process lifetime (non-obvious, don't regress)

`run_logs` spawns one producer per resolved source, gives each a reader
thread feeding a shared `Merger`, and runs an emitter thread that writes the
merged colorized output into a real temp file (`tempfile::Builder`) rather
than into the pager's stdin directly - see below for why. `less` then opens
that temp file by path.

Neither `tail -F` nor `docker logs -f` reaches EOF by design, so no reader can
be what ends the process. They run on their own threads and the **main thread
blocks on `pager.wait()`** instead. When the pager exits, **every** producer
is killed before **any** thread is joined - a survivor holds its reader open
and the join hangs exactly as it would have with one - their stdout pipes hit
EOF, and the temp file (owned by a `NamedTempFile` local to `run_logs`) is
removed by its `Drop` as the function returns. Getting the wait/kill ordering
backwards - blocking the main thread on a reader, or joining before killing -
reintroduces a hang when quitting the pager on a small or quiet log. That was
a real regression caught in final review, not a hypothetical.

The emitter needs one thing more, because it polls the merger rather than
blocking on a pipe: closing the producers' stdout tells it nothing. The
`stop` flag, set after the kills and before the joins, is what ends it. A
reader that dies on an IO error still records EOF for its source, so only a
panicking one strictly needs the flag - but without it that join hangs on a
thread that will never return, with the terminal already handed back to the
user. Both hangs have their own deadline test.

That ordering lives in `supervise_pager` and has a regression test:
`supervise_pager_returns_once_the_pager_exits_even_if_the_log_is_silent`
drives real child processes (a `sleep` whose piped stdout never reaches EOF,
standing in for `tail -F`) and asserts the call returns within a deadline.
Inverting the wait/kill/join order makes it fail rather than hang the
suite - confirmed by actually inverting it.

`ctrlc::set_handler`'s failure is fatal rather than ignored: without the
handler the process reverts to the exact behaviour the handler exists to
prevent, so starting a session that cannot clean up after itself is worse
than refusing to start. `.expect()` on `tail.stdout` and `sink.reopen()` is
likewise avoided - both can fail for real (fd exhaustion, `/tmp` remounted
read-only) with the pager already on screen, where a panic would dump a
backtrace over a live `less` and skip cleanup.

#### Two gotchas behind the design

`less -R +F` already provides scrollback and search for free once you drop
out of follow mode - arrow keys and PgUp to scroll, `/pattern` to search
forward and `?pattern` backward - so no code is needed for any of that.
Both of the gotchas below were found by driving a real pty
(`expect`, not a raw shell pipe or Python's low-level `pty.fork()` - the
latter skips the foreground-process-group setup a real terminal does, giving
false hangs that don't count as a real repro):

1. **Ctrl+C.** The terminal delivers `SIGINT` to the whole foreground
   process group at once - tekops, `tail`, and `less`, not just whichever
   one the user "means" to interrupt. `less` catches it and drops out of
   follow mode, but a bare Rust binary has no handler and terminates
   immediately, so without a fix Ctrl+C killed tekops out from under the
   pager instead of just pausing the tail. `run_logs` calls
   `ctrlc::set_handler(|| {})` before spawning anything, and spawns `tail`
   with `.process_group(0)` so the same broadcast doesn't kill `tail` too -
   which would otherwise permanently break `less`'s `F` to resume
   following, since the source process would already be dead.

2. **Searching a pipe hangs forever on a miss.** This is why the temp file
   exists. Reading from a pipe (the original `tail -F | less` design),
   `less` cannot conclude "not found" when a search misses what is already
   buffered - `tail -F` never sends EOF, so more could always arrive - and
   it blocks indefinitely (`Waiting for data... (^X or interrupt to
   abort)`), even for a backward search, even on a small just-started
   session, even outside follow mode. It looks exactly like a frozen
   terminal. Feeding `less` a real file fixes this, since it can `stat()`
   to know the true current size: confirmed side-by-side, same search, same
   missing pattern - the pipe hangs, the real growing file returns `Pattern
   not found` in ~1ms.

**Ignoring `SIGINT` alone isn't enough once `tail` is in its own process
group.** `ctrlc` is pulled in with its `termination` feature, extending the
same ignore to `SIGTERM`/`SIGHUP`. Without it, a dropped SSH session
(`SIGHUP` to the whole foreground group, same broadcast mechanism as Ctrl+C)
would kill tekops immediately, skipping cleanup - and since `tail` was
deliberately moved out of that group, it would be orphaned permanently,
still tailing and writing to a temp file nobody will ever delete. One leaked
`tail` and one leaked temp file per dropped session: reproduced, and
confirmed fixed against a real `SIGHUP` broadcast. This works because `less`
has no special handling for `SIGTERM`/`SIGHUP` the way it does for
`SIGINT` - it just dies, `pager.wait()` returns, and the existing cleanup
runs.

The one caveat: `less` independently **ignores `SIGTERM` itself** (confirmed
directly, unrelated to anything tekops does), so a bare `kill <tekops-pid>`
no longer ends a session either, now that tekops also ignores it. That was
never a clean shutdown path anyway - a single-PID kill never reached `tail`,
in any version of this code, since it is a separate PID. Quitting via `less`
(`q`, or Ctrl+C then `q`) is the supported way to end a session;
`pkill -9 -f tekops` is the fallback for a truly stuck one.

## `dump.rs`

`tekops dump-logs`: a shareable, anonymised snapshot of the node's log.

Split the way `logfmt.rs` and `host.rs` are: `render_dump`, `format_utc` and
`default_filename` are pure and hold every formatting decision, and
`run_dump` holds all the I/O.

Four things are load-bearing:

**The pipeline is `term::sanitize` then `redact`, in that order.**
Sanitizing first stops a control byte hiding inside a token and dodging
classification. `logfmt::format_log_line` is deliberately *not* used, so the
file carries no ANSI and the lines stay byte-faithful to what the node
emitted.

**The header is rendered last**, because its `redacted:` line reports the
run's final total and cannot be built before the lines it counts. Its own
field values are redacted individually as they are gathered rather than by
running the finished header through the redactor, which would let the header
invalidate the number it has already printed.

**Every byte written passes through the redactor**, header and doctor report
included - the doctor report carries mount paths and node error text, and a
container name on Eth Docker derives from the directory the stack was cloned
into, so reasoning per-section about what is safe is how something gets
missed.

**A `--doctor` failure never fails the dump**: the command exists for the
case where the node is sick.

`logs::Mode::Once` means none of `run_logs`'s pager, signal-handler,
process-group or temp-file machinery applies here at all - that whole
problem is specific to `tail -F` never reaching EOF. `civil_from_days` is
Hinnant's algorithm rather than a `chrono` dependency; `docker.rs` declined
the same maths for a container uptime because `restart_count` already
carried the signal, but here a readable stamp *is* the field.

## `redact.rs`

Anonymises log content for `tekops dump-logs`.

Pure: no I/O, no environment reads, no `Command`. The whole engine is a
hand-rolled token scanner rather than a set of regexes, for the reason
`metrics.rs` hand-rolls a Prometheus exposition parser - this repo halved
its binary by dropping ureq's TLS backend and justifies every `Cargo.toml`
line in a comment, so a dependency for a handful of shapes is not a trade
it wants.

Values are replaced with *stable* placeholders (`<peer-1>`, `<ip-3>`)
rather than a flat marker. Flat redaction destroys the correlations that
make a log diagnosable: "peer 3 disconnected, then reconnected" is often
the whole finding. `Redactor` therefore carries a per-category `HashMap` and
numbers in first-seen order.

Four more things are load-bearing:

**Pubkeys and addresses are matched on *exact* hex length** (96 and 40).
`0x` plus 64 hex is a block or state root, appears on nearly every line, and
matching it is the single most destructive over-match available here;
`block_and_state_roots_are_preserved` pins it.

**`is_ipv6` requires either a `::` run or the full seven-colon form**,
because a Teku console time field is `01:16:54` - two colons, three groups,
all valid hex - so the obvious "colons plus hex" rule redacts every timestamp
in the file.

**URLs and `/home/<user>` paths are pre-passes, not tokenizer rules**, since
`/` has to stay a delimiter for multiaddrs and the tokenizer can therefore
never see either as one piece. The URL pre-pass in particular is what keeps
"long opaque string" from being a general shape rule, which would eventually
eat a hash that matters.

**`node` is deliberately not a host anchor** even though `host`/`hostname`
are - Teku writes "node" constantly in prose ("node is syncing"), so
anchoring on it would redact the next word line after line.

`diagnostic_values_are_never_redacted` is the contract for the other
direction: over-redaction and under-redaction are both failures of this
module, and only one of them is obvious.

## `gist.rs`

GitHub gist semantics for `tekops dump-logs --gist`.

Request shape, auth material and response parsing. **No transport policy
lives here**: tekops builds ureq with no TLS backend on purpose, so every
HTTPS call goes through `curl.rs`, and this module hands it a URL, a config
file, a body file and the headers it wants. `curl::post_argv` owns the
protocol pin, the stall bound, `--data-binary` and the absent `-f`.

Three things are load-bearing on this side:

**The token reaches curl only through the config file**, because
`/proc/<pid>/cmdline` is world-readable on the node and the node's other
tenant is a validator. Both the config file and the body file are 0600.
`validate_token` rejects a quote, backslash or control character, the last of
which would otherwise let a mis-set variable append a second directive to
curl's config.

**`parse_gist_response` owns what a non-201 means**, which is the direct
consequence of `curl.rs` omitting `-f`: curl exits 0 on a 4xx, and the body
it kept is where "Bad credentials" and the missing-scope message live. The
status arrives as a final in-band line from `-w '\n%{http_code}'` and gets
split back off.

**Gists are secret, with no `--public` flag** - unlisted and unindexed,
still link-reachable, which is what "paste this to a maintainer" needs.

## `loglevel.rs`

What `tekops log-level` is being asked to send, and where that came from.

The level is a positional argument (`tekops log-level info`), sent to the
API exactly as typed - no case normalization.

The level can be typed (`tekops log-level debug`) or fetched from a URL
holding the request body a Teku maintainer prepared - typically a gist,
since the whole point is handing an operator a set of logger names they
could not have worked out themselves.

Everything here except `load` is a pure function over its arguments, the
same split `logs::resolve_log_sources` and `completions::plan` use: the
argument handling, the URL rewriting and the body parsing are all testable
with no network and no environment.

`resolve_log_level_target` splits the one positional into a level or a URL,
discriminating on `://` because no log level contains one - the third
instance of the hand-matched-positional pattern `resolve_logs_target` and
`resolve_update_target` established, and for the same reason. It also
rejects `--filter` alongside a URL rather than picking a winner: the fetched
body carries its own `log_filter`, and honouring one while silently
discarding the other is the failure class this repo keeps designing against.

`raw_url` rewrites a gist **page** URL (which serves HTML) to its `/raw`
form, dropping any `#fragment` or `?query` first - a browser-copied link to
one file of a multi-file gist carries `#file-something-json`, and appending
`/raw` after that asks for a path that does not exist. **Only a recognised
gist URL is trimmed**; a query string on any other host may be the whole
point of the link, as it is on a signed URL. A multi-file gist's `/raw`
serves only its first file - verified against a real one - which is why
USAGE.md says to put one file in the gist.

`parse_spec` uses `deny_unknown_fields`, which is load-bearing: a body with
`log_filters` or `levl` in it would otherwise apply a change missing the very
thing it was written to carry and report success. An empty `log_filter` list
arrives as `None`, since `beaconapi::set_log_level` omits the key entirely
for a global change rather than sending `[]`.

Parse failures name **the URL the operator typed**, not the rewritten one
they have never seen, and their text is sanitized in
`LogLevelError::malformed` by construction (the `doctor::Finding::new`
shape), because serde quotes the offending key back and that key came off
the network.

## `doctor.rs`

`tekops doctor`: collect, then judge.

`probe` performs every I/O and records each outcome into `Facts`, where
nothing is an early return. `evaluate` is a pure function over `Facts`
holding every threshold and every opinion, with no I/O at all.

The split is what makes the diagnostic behaviour testable: "what does
doctor report for a Rocket Pool node whose consensus container is
restarting" is a unit test over a struct literal, on every stack, with
nothing installed. `probe` having no early return anywhere in it is part of
the contract: a doctor command that aborts on its first failure is useless
at exactly the moment an operator reaches for it.

`cli.rs` holds only `run_doctor` and the stack/data-dir resolution around
it, including why this command prints no `--stack` hint.

The judgement thresholds (`PEERS_WARN_BELOW`,
`DISK_WARN_BELOW`/`DISK_FAIL_BELOW`, `MEM_WARN_BELOW`, `RESTARTS_FAIL_AT`,
`LOAD_WARN_ABOVE_PER_CPU`/`LOAD_FAIL_ABOVE_PER_CPU`,
`FINALITY_WARN_ABOVE`/`FINALITY_FAIL_ABOVE`) are named constants at the top
of this file rather than literals inside `evaluate`, because they are
conservative estimates picked for a home-staker mainnet node and reviewed on
the explicit understanding that they're tunable. Three checks - `el_offline`,
an optimistic head, and a container that isn't running - are binary facts
rather than judgement calls, and are not gated by a constant.

### `Probe::Skipped` exists to bound a timeout budget

`http::REQUEST_TIMEOUT` is 10 seconds per call, and `probe` makes up to
eight calls against the node: four Beacon API (`health`, `syncing`,
`finality_checkpoints`, `peers`) and four metrics scrapes (`bn_families`,
`vc_families`, `duties`, `validators`) - three when `bn_metric_url` and
`vc_metric_url` are equal, since the all-in-one case below reuses
`bn_families`'s result instead of scraping the same endpoint twice. Run
naively and sequentially against a dead node, that's up to 80 seconds of
silence on the exact command an operator reaches for *because* the node
looks sick.

So `health()` runs first and gates the other three Beacon API calls, and
`vc_families()` gates `duties`/`validators` the same way: if the gating call
comes back unreachable, the rest are recorded as `Probe::Skipped("...
unreachable")` and never attempted. `bn_families()` is not gated by
anything and is always fetched, because `version` and `doctor` both need
the beacon node's own answer regardless of what the validator client's
looks like. That leaves three independent endpoints - beacon API, BN
metrics, VC metrics - each contributing at most one timeout, dropping the
worst case to roughly 30 seconds, or 20 when the two metric URLs are equal
and there are only two endpoints to time out against.

`Skipped` is a distinct variant from `Failed`, not a reuse of it, because
they mean different things and render differently.
`check_beacon_api`/`check_bn_metrics`/`check_vc_metrics` treat a `Skipped`
gating probe as `Fail` (the endpoint really is down), while every check
downstream of it (`check_syncing`, `check_finality`, `check_peers`,
`check_validators`, `check_duties`) treats both `Skipped` and `Failed` as
`Warn`, since the outage is already reported once as a `Fail` by the gating
check and repeating it per downstream row would be noise, not information.

### Two metrics endpoints, one scrape when they agree

`check_bn_metrics` and `check_vc_metrics` are both thin calls into
`check_one_metrics_endpoint`, passing a different name, `Probe`, URL and
version-family picker; the two render identically and differ only in which
endpoint and which family they're reading.

`probe` fetches `vc_metric_url` only when it differs from `bn_metric_url`;
when they're equal it clones `bn_families`'s outcome (`Ok`, `Failed` or
`Skipped`) into `vc_families` rather than scraping again, because an
all-in-one deployment is one process answering for both roles, and asking
it twice would double the timeout budget for no new information.

`check_metrics_layout` reads `bn_families` alone and tells apart the two
shapes a wrong metrics layout can take, using exactly the fact
`has_validator_families` exists to carry (see the `metrics.rs` entry): a
validator client sitting where the beacon node was expected
(`has_validator_families` true, `beacon_versions` empty), or an all-in-one
deployment (`has_validator_families` true, `beacon_versions` non-empty, and
`vc_families` separately unreachable). The two conditions are mutually
exclusive by construction, and `the_two_layout_diagnostics_are_mutually_exclusive`
pins that.

**Neither diagnostic repoints anything - both only ever warn and name what
to change by hand.** A heuristic that silently redirected `vc_metric_url` to
what looks like the validator client's real endpoint would be the same
class of mistake `metrics::require_metric` exists to prevent (see the
`metrics.rs` entry): a confident, silently-adjusted answer is worse than one
that names the problem and stops, and an operator who moves `metric_url`'s
value to `vc_metric_url` by hand knows what changed, while one whose config
was silently reinterpreted for them does not.

### Container checks are omitted on bare-metal, never rendered as "n/a"

`check_consensus_container` returns immediately, pushing nothing, when
`f.stack` is `Some(Stack::BareMetal)` or `None` - a bare-metal node has no
container to have a restart count, so the question does not apply and a
placeholder row would be actively misleading. This has to stay distinct from
two other states that *do* produce a finding: `Probe::Ok(vec![])` on a Docker
stack (Docker answered and named zero containers - the failure this command
exists to catch) and `Probe::Failed` (Docker was asked and didn't answer).
`probe` maps a bare-metal or unnamed-container config directly to
`Probe::Skipped(NO_CONTAINER)`, so the omission decision is really made
twice - once by what `probe` records, once by the check's early return - and
both have to agree, which is why both are tested per stack.

`check_container_restarts` is gated on the same question, per process, and
**on the stack rather than on the list being non-empty** - a bare-metal
operator must never see a restart row however the list they arrive with got
populated. It is one row for the whole deployment, not one per container: the
detail names the container it is talking about, so a second row would repeat
the label without adding a fact.

### A missing validator container is silence, not a failure

`check_validator_container` inverts the consensus check's reading of absent.
Every Docker deployment has a consensus container, so none found is a
reportable failure; but the commonest layout there is runs Teku's validator
*inside* that container, so no second container is the normal case and a row
saying otherwise would be a false alarm on a healthy node. `probe` therefore
records `Probe::Skipped(NO_VALIDATOR_CONTAINER)` rather than `Ok(vec![])`
when no validator container was detected, which is why it does not reuse
`inspect_for_stack` - this is the absent-is-not-zero pattern applied to a
container rather than a metric.

### An absent metric family is a wrong-port diagnosis, never a measured zero

`metrics::require_metric` already refuses to report present-but-absent as a
confident zero - it errors instead, naming the URL and the missing family.
`check_validators` and `check_duties` render that error as `Warn`, not
`Fail`, and pass its text straight through rather
than re-deriving a message, since conflating "absent" with "zero" is a
documented past bug in this repo and `doctor` is the command most likely to
reintroduce it - it reports on both the beacon-node and validator-client
metric families side by side, in one report, which is exactly the situation
that bug needs to hide in. `check_one_metrics_endpoint` draws the same
absent-versus-zero line for `check_bn_metrics`/`check_vc_metrics`, but
doesn't go through `require_metric` to do it: `families()` already reports
an empty version list rather than erroring (see the `metrics.rs` entry), so
the check renders that emptiness as its own `Warn` message directly.

### No ANSI, by construction

`Finding::new` runs `detail` through `term::sanitize` in its constructor, so
every `push(...)` call in `evaluate` is safe without discipline at the call
site. Everything that ends up in a finding's detail text originated outside
this binary - version strings from the scrape page, mount paths and
container names from `docker inspect`, error text from the node.
`output::format_doctor_report` is plain aligned text rather than
`comfy-table` for the same reason.

## `host.rs`

Host resource facts, for `tekops doctor`.

The parsing is pure and the I/O is a thin wrapper around it, the same split
`logfmt.rs` uses: `/proc` exists only on Linux (the node) and not on macOS
(the dev machine), so the interesting half has to be reachable from a test
that never touches a real file, on either platform.

Absent input is `None` on every parser, which `doctor::evaluate` renders as
an omitted check rather than an error - a host fact that could not be read
says nothing, not "fail".

**`collect` shells `df -Pk <path>`, never plain `df`.** POSIX's `-P` is what
guarantees each filesystem prints on exactly one line; the default format is
permitted to wrap a long device name onto its own line first, which shifts
every subsequent column over. `parse_df` reads the data line at a fixed
offset assuming the one-line form, so a wrapped line comes up short on
fields and must parse as `None` - misreporting free space is worse than
reporting none, and `df_rejects_the_wrapped_non_posix_form` pins this rather
than trusting the flag alone.

`parse_meminfo` reads `MemAvailable`, never `MemFree`: the page cache keeps
`MemFree` near zero on a healthy Linux box, so alarming on it would fire on
every correctly-running node. CPU count comes from
`std::thread::available_parallelism()`, not from parsing `/proc/cpuinfo` -
one more thing that would need a Linux-only parser for a number the standard
library already exposes portably.

## `docker.rs`

`docker inspect` for `tekops doctor`.

Argv is returned as data and the JSON is parsed by a pure function, the
same split `logs::producer_argv` and `stack::detect_stack` use: the shape
of what we ask Docker and what we make of the answer are both testable with
no Docker installed.

`parse_inspect` strips the leading `/` Docker puts on `Name`
(`/eth-docker-consensus-1` becomes `eth-docker-consensus-1`), since the
slash is not what an operator typed and would read as a typo echoed back in
a finding.

`started_at` is kept as the raw RFC3339 string rather than turned into an
uptime `Duration`: doing that needs either a new dependency or a hand-rolled
calendar, and neither earns its keep, because the restart-loop signal doctor
actually needs is carried by `restart_count` alone - which is why
`container-restarts` is a plain count with no uptime clause.

`data_mount` picks the mount whose source path is longest, not the first
one: containers routinely bind trivia like `/etc/localtime` alongside the
real data volume, and the longest source path is the most specific one
covering the data directory without needing per-client knowledge of which
destination path to look for.

## `stack.rs`

`Stack` (bare-metal / eth-docker / rocketpool) and `detect_stack`.

Owns the per-stack port defaults and container-naming knowledge, and nothing
else - no I/O of its own. The actual `docker inspect` work lives in
`docker.rs`.

Two things are load-bearing. **`bn_metric_url()` and `vc_metric_url()` are
separate accessors, not one that `duties`/`validators` override**, because
the two processes' ports genuinely differ per stack and `version`/`doctor`
need both at once - collapsing them back into one method would just move the
resolution problem into the caller. Teku's own `--metrics-port` default is
8008 for both processes; both Docker stacks keep that for the beacon node
(Rocket Pool moves it to 9100, `defaultBnMetricsPort`), so `bn_metric_url()`
tracks Teku's own default. `vc_metric_url()` doesn't: Eth Docker moves the
validator client to 8009 and Rocket Pool to 9101, and bare-metal's 8010 is a
**tekops convention, not a Teku default** - a separated bare-metal node has
necessarily repointed one of the two processes by hand already, so tekops
picking 8010 costs it nothing and gives every stack a distinct VC port. And
**`container_suffix` is a suffix, not a name**, because neither prefix is
knowable from the stack alone - Eth Docker's is the Compose project (the
directory it was cloned into) and Rocket Pool's is its configurable
`ProjectName`. `validator_container_suffix` is its counterpart
(`*-validator-1`, `*_validator`) and the reason a separated deployment can be
named at all: Rocket Pool's External Consensus Client mode supervises the
validator against a beacon node it did not start, so `_eth2` never exists and
only the validator suffix can tell tekops that Docker is involved.

`detect_stack` takes `docker ps` output as a parameter rather than running
`docker` itself, so the matching rule is testable with no Docker installed -
the same shape as `logs::resolve_log_sources`. It requires exactly one match:
zero is an error rather than a fallback and two is an error rather than a
guess, since tailing the wrong node's logs looks exactly like tailing the
right one until it matters.

`detect_validator_stack` applies that identical rule to the validator
container; both delegate to one private `detect_role` so they cannot drift on
whitespace, `only` narrowing, or the refusal to guess. They stay two public
functions rather than one with a `Role` parameter because the two answers are
independent facts about one host, and most callers want only the first:
`logs` and `dump-logs` ask the consensus question alone, since a log target
is one file or one container. `DetectError` carries the `Role` so a failure
names the container the caller asked about and suggests an escape hatch that
exists - `--container` points `logs` at a consensus container and has never
meant the validator.

`Stack`'s per-variant `#[serde(rename)]` matches its `#[value(name)]`
exactly, which is what lets `config.rs` deserialize straight to `Stack` and
keeps the config spelling and the flag spelling from drifting;
`serde_matches_clap_values_for_every_variant` pins it.

## `protocol.rs`

Classifies a peer's transport from its multiaddr.

`Protocol::Quic` if `last_seen_p2p_address` contains a `/quic` component,
`Tcp` otherwise. An earlier version decoded the peer's ENR instead (via the
`enr` and `k256` crates), but the Beacon API doesn't reliably populate that
field and it always classified as `Unknown` in practice - don't reintroduce
ENR-based classification.

## `config.rs`

The `~/.config/tekops/config.toml` file: what it can say, and where it is.

Same pure/IO split as `host.rs` and `completions.rs`. `parse` and `path`
are pure and take every input as a parameter, so the whole surface is
testable with no environment races and no files on disk; `load` is the only
function here that touches a filesystem.

The file carries exactly the ten settings that already have `$TEKOPS_*`
variables, and nothing else. Every key is a variable is a flag, which is
the one sentence that makes the feature explainable. Per-command defaults
(`lines`, `json`) are deliberately absent: they have no variable, so they
would break that rule, and they would need a way to tell "the operator
typed the default" from "the operator typed nothing", which clap's
`default_value_t` cannot express. A GitHub token is deliberately absent
too - `GITHUB_TOKEN`/`GH_TOKEN` are shared conventions with the `gh` CLI
rather than tekops settings, and `gist.rs` already works to keep that
credential out of `/proc/<pid>/cmdline` on a machine running a validator.

The log-source split to ten is the instructive contrast: `container` and
`logs_file` kept their spelling with **no** deprecated alias, because nothing
about their meaning changed. They named "the log source" and now name "the
beacon node's log source", which on an all-in-one node is the same container
and the same file. An alias is owed when a key's meaning changes, not when a
sibling appears beside it.

The count moved from six to eight when the metrics endpoint split in two, and
to ten when the log source did the same (`vc_container`, `vc_logs_file`):
`bn_metric_url` and `vc_metric_url` joined the struct, and the field they
replaced, `metric_url`, **stayed rather than being removed**. `Config`
derives `deny_unknown_fields`, so dropping a key outright would reject every
config file that still carries it - the same reasoning that keeps
`--metric-url` a hidden `clap` flag rather than a removed one (see the
`cli.rs` entry). `metric_url` also **changed what it means** in the
process: it used to resolve to the validator client, and now resolves to
the beacon node, matching `api_url`. `the_deprecated_metric_url_key_still_parses`
pins that the key keeps parsing; `cli.rs`'s
`the_deprecated_spelling_feeds_the_beacon_node_not_the_validator` pins the
meaning it resolves to now. `doctor`'s "metrics layout" check (see the
`doctor.rs` entry) is what catches an operator who set this key under its
old meaning and never moved it.

`path` returns `Option`, and a missing `$HOME` *and* `$XDG_CONFIG_HOME`
yields `None` rather than an error. That is why this module has its own path
resolver instead of reusing `completions::Dirs`, whose `home` is
non-optional and whose `CompletionError::NoHome` is correct for
`autocomplete` and wrong here: a config that cannot be located must not stop
a command that was never going to read it.

**TOML, not YAML.** The first-party YAML binding publishes as literally
`serde_yaml 0.9.34+deprecated` and rests on the unmaintained
`unsafe-libyaml`, and YAML would coerce a container named `on` to a boolean.
The `toml` crate is six pure-Rust Cargo-team crates costing a measured
137,104 bytes (2,317,872 to 2,454,976), about a tenth of what dropping
ureq's TLS backend saved. A hand-rolled `key = value` parser was the third
candidate, with real precedent here (`metrics.rs` and `redact.rs` both
hand-roll to avoid a dependency), and was rejected because a config file is
a user-facing contract where a familiar standard format outweighs saved
bytes.

`stack` deserializes to `Stack` rather than `String`, which is free because
`Stack`'s per-variant `#[serde(rename)]` already matches its
`#[value(name)]` exactly, so the config spelling and the flag spelling
cannot drift.

**A bad config file is fatal, unlike a bad `$TEKOPS_STACK`**, and
`deny_unknown_fields` is load-bearing the same way it is in
`loglevel::parse_spec`. The variable is ignored when unparseable because it
is set once in a shell rc and would otherwise break every command in the
session; a config file is a single deliberate artifact whose typo would
otherwise surface much later as a connection error against a port the
operator thought they had changed. Error text is sanitized in both
`ConfigError` variants and names the path, since the operator's next action
is opening that file.

### `config.example.toml`

Pinned by a test in this module that serializes a fully populated `Config`,
parses the sample, and asserts set *equality* between the two key sets in
both directions - the same trade `tests/ci_gates.rs` makes for
`check.sh`/`ci.yml`. The field list comes from the struct rather than a list
written in the test, because a hand-written list just moves the drift
problem. (That sample is also why `Config` derives `Serialize`, which the
binary itself never uses.)

The sample's keys are **commented out**, so copying the file verbatim is a
no-op rather than a silent commitment to whichever stack the example shows -
an operator who copied it and missed a line would otherwise get eth-docker
ports plus a container name that disables `docker ps` detection, which
surfaces as exactly the confusing wrong-port error the strict parsing exists
to prevent. A commented key is still checkable: `uncomment_keys` reads them
back, keyed on a one-character convention the sample holds to - a key is
`#key = value` with no space after the hash, prose is `# text` with one - and
a test pins that convention too, since switching the sample to `# key =
value` would silently start comparing against an empty set.
`the_sample_as_shipped_configures_nothing` pins the other half.

## `completions.rs`

`tekops autocomplete`: installs a bash/zsh/fish completion script.

Five things here are load-bearing:

**The script is generated from the live `clap::Command`, never checked in.**
`cli::command()` exists solely to hand it over, so the script and the parser
come from one definition. There is no generated file in the repo, no build
step, and no CI drift check, because there is nothing that *can* drift -
adding a subcommand changes the next invocation's output for free. Don't
"helpfully" add a checked-in copy or a regeneration task; that reintroduces
exactly the problem this shape removes.

**`Shell` is tekops's own three-variant enum, not `clap_complete::Shell`.**
Generating for elvish and powershell is free; *installing* is not, since
each needs a directory and an rc-equivalent nothing here has been run
against. Naming only the supported three lets clap reject the rest at parse
time with the valid values listed. Note this positional **is** a
`ValueEnum`, unlike `update`'s target, which is hand-matched because it
doubles as a subcommand name - this one doubles as nothing.

**bash gets an rc `source` line even though it installs into
`bash-completion`'s own auto-loading directory.** That directory only works
when the bash-completion v2 package is installed and sourced, which stock
macOS (system bash 3.2) does not have. The generated bash script was checked
and is self-contained - plain `COMP_WORDS`, no `_init_completion`, and it
branches on `BASH_VERSINFO` for 3.2 - so sourcing it directly works with or
without the package, and double-registration is harmless. The stanza goes to
`~/.bash_profile` on macOS and `~/.bashrc` elsewhere (`bash_rc_name`, which
takes the platform as a parameter so both branches are testable from either
machine): macOS Terminal starts bash as a *login* shell, which reads
`.bash_profile` and never `.bashrc`, so the obvious choice installs a
completion that silently never loads. zsh's stanza calls `compinit` a second
time on purpose - an appended stanza is reached after a framework's own
`compinit` has already run, so the directory is only searched if `compinit`
runs again after it joins `fpath`.

**`plan` is pure and takes every directory as a parameter** (`Dirs`), the
same shape and for the same reason as `logs::resolve_log_sources`: the whole
path computation is testable against a tempdir with no environment races.
`cli::run_autocomplete` holds the env reads, the preview and the prompt, and
`cli::dirs_from_env` is the one place `HOME`/`XDG_*` are read. The rc stanza
is append-once, guarded by the `MARKER` constant, so re-running never
duplicates it; the script file is always overwritten, since that is the
refresh path.

**`refresh_installed` runs the *newly installed* binary, not the in-process
generator.** Called from `cli::install_version` after `tekops update`
succeeds, it exists because the installed script is a snapshot: a release
that adds a command leaves it stale. Generating in-process would faithfully
write the *old* build's command set, which is the bug it is there to
prevent. It only rewrites files that already exist (their presence is the
user's opt-in) and never touches an rc file, and its failure is a warning
rather than an update failure - the binary is already in place and working
by then, so failing the update would be a lie.

One test convention worth knowing before adding here:
`every_generated_script_parses_under_its_own_shell` runs each generated
script through the real shell's own parser (`bash -n`, `zsh -n`, `fish
--no-execute`) and **skips shells that are not installed**, since CI's Linux
runner has neither zsh nor fish. It asserts it checked at least one, so it
cannot pass vacuously - but a green suite is not evidence that all three
were parse-checked on that host.

## `update.rs`

Self-update: replaces the running tekops binary with a build published to
GitHub Releases.

All HTTPS goes through `curl` rather than through `ureq`, for the reasons
`curl.rs` documents; this module owns only what is specific to fetching a
release, not the transport itself. `check` and `install` take the fetcher as
a parameter (`Fn(&str) -> Result<Vec<u8>, CurlError>`) so the whole install
path is testable against a canned release with no network, and
`UpdateError::Curl` delegates `Display` to `CurlError` so the transport's
messages are worded once. `ASSET_HINT`/`RELEASE_HINT` stay here rather than
in `curl.rs`, since which hint fits a failed hop is release knowledge, not
transport knowledge - a 404 on an asset points at the platform, a 404 on the
release API points at the repository, and hanging one hint off every curl
failure sends the operator looking in the wrong place. The smoke-tested
binary's stdout goes through `term::sanitize`.

**`check` and `latest` are hand-matched positional values, not clap
subcommands.** `resolve_update_target` mirrors the same hand-matching
`tekops logs` used to need, for the same "one slot, two meanings" reason:
`logs`'s first positional used to be typed as an enum of source names, so
clap rejected any value outside that set before the command ever ran, making
`tekops logs /var/log/x.log` fail with `invalid value for [SOURCE]`.

**The staging `TempDir` is deliberately not the target directory.**
Download, checksum, extraction and the smoke test all happen under the
system temp dir; only the verified binary is copied beside `current_exe()`
for an atomic same-filesystem `fs::rename`. Extracting into `/usr/local/bin`
would need privileges for work that does not need them, and would leave
debris there on failure. `probe_writable` runs *before* the download - it
writes a file rather than reading permission bits, since a path can be
unwritable for reasons the bits do not show - so a non-root invocation fails
in a second instead of after fetching a megabyte.

**The checksum is not a signature and the smoke test is not a security
control.** `SHA256SUMS` comes from the same release as the tarball, so
whoever can replace one can replace the other; it catches corruption and a
wrong-platform asset, and TLS is the actual channel guarantee. The smoke test
runs the downloaded binary before installing it, so a hostile asset executes
either way - its job is catching a build that cannot run before it replaces
one that can. Don't let either get described as more than that.

**Release assets are named `<arch>-<os>`, not by the cargo target triple**,
and the `TARGET` constants here must match the names
`scripts/build-release.sh` publishes exactly or `tekops update` 404s.
`target_matches_the_names_build_release_publishes` reads the script and
asserts they agree - without it the unit tests compare the constant against
itself and stay green through a rename. Releases up to v0.3.1 used triple
names, so `tekops update <those versions>` cannot reach them; the change was
made in the same release that introduced `tekops update`, when no shipped
binary could do a rollback anyway, because every later moment would strand
more releases.

There is deliberately **no automatic update check**: no startup probe, no
cache file, no staleness policy, no disable flag. Nothing touches the
network unless the user typed `update`.

## `term.rs`

Making untrusted text safe to print to a terminal.

Two channels carry data this binary didn't author all the way to the
operator's screen: log fields (peer identifiers, remote agent strings,
exception text) rendered by `less -R`, and an endpoint's error body echoed
to stderr. Printed verbatim, either can clear the screen, retitle the
window, or forge a red ERROR line that appears to come from tekops itself -
verified with `od` on both paths. Both channels are sanitized.

`sanitize` replaces control characters - keeping only tab and newline - with
U+FFFD.

This is the same bug class as the old hand-built `--json` `format!` output
(see `output.rs`); it was fixed on the JSON channel and originally missed on
the terminal one.
