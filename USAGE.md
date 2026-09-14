# tekops usage

Every command. See the [README](README.md) for installing and building.

    tekops logs [path]
    tekops peers
    tekops health
    tekops head
    tekops doctor
    tekops duties
    tekops validators
    tekops version
    tekops log-level <LEVEL> [--filter=org.example ...]
    tekops log-level <URL> [-y]
    tekops beacon validators <index-or-pubkey>...
    tekops beacon duties attester --epoch=N <index>...
    tekops beacon duties proposer --epoch=N
    tekops about
    tekops autocomplete [bash|zsh|fish]
    tekops update [check|latest|<version>]

## logs

    tekops logs                  # /var/log/teku/teku.log, or $TEKOPS_LOGS_FILE, or a detected container
    tekops logs /var/log/x.log   # tails that path instead
    tekops logs -n 2000          # 2000 lines of scrollback instead of the default 500
    tekops logs --lines 0        # skip existing output, follow only new lines
    tekops logs --container rocketpool_eth2   # read a container's logs

`logs` opens with the last 500 lines already in the buffer and then follows
new output. `-n`/`--lines` changes that count; it is passed straight to
`tail -n`, so `0` means "show nothing existing, follow only what arrives
next". The pre-loaded lines go into the scrollback buffer, not just on
screen, so searching covers all of them from the moment the session starts.

**`tekops logs teku` no longer works.** The `[teku|besu]` source argument was
removed: the first positional is now always a path. Typing `teku` there reads
as a file named `./teku` in the current directory, which almost never exists,
so it fails with a bare "no such file" error that gives no hint why. Drop the
word - `tekops logs` alone already tails the Teku log (or a detected
container). Execution-client logs (Besu, or any other EL) are not supported;
see issue #3 for that decision.

Under Docker the source is a container rather than a file. See
[Docker deployments](#docker-deployments).

### In the pager

`logs` opens `less` in follow mode, so everything `less` does is available:

| Key | Effect |
| --- | --- |
| `q` | quit the session |
| `Ctrl+C` | stop following, leaving the buffer on screen |
| `F` | resume following after `Ctrl+C` |
| arrows, `PgUp`/`PgDn` | scroll back through everything buffered this session |
| `/pattern`, `?pattern` | search forward, search backward |

`Ctrl+C` does not kill tekops. The terminal delivers it to the whole
foreground process group, so `less` catches it and drops out of follow mode
while tekops and the log producer keep running - which is what makes `F` able
to resume. Quitting with `q` is how a session ends.

`tekops` deliberately ignores `SIGTERM` and `SIGHUP` too, so that a dropped
SSH connection cannot kill it mid-session and orphan the process feeding the
pager. One consequence: `kill <tekops-pid>` will not end a session either.
Use `q`, or `pkill -9 -f tekops` for one that is genuinely stuck.

A session buffers at most 256 MiB of colourized output. On reaching that it
stops following and says so in-band:

    *** tekops: 256 MiB buffer limit reached, stopped following.
    *** Scrollback and search still work. Quit and rerun to resume.

The buffer is a real file rather than a pipe, which is what lets `less`
answer a failed search immediately instead of blocking forever waiting for
output that may never arrive.

## Beacon API commands

`peers`, `health`, `head`, `log-level`, and every `beacon` subcommand read the
Beacon API. They accept `--api-url` (or `$TEKOPS_API_URL`) to point at a
non-default endpoint (default: `http://localhost:5051`).

### peers

Prints peer counts grouped by direction and protocol, plus the total, rather
than one row per peer:

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

### health, head

`health` reports health and sync status. `head` reports the chain head slot
and root plus the finality checkpoints.

### doctor

    tekops doctor
    tekops doctor --stack eth-docker
    tekops doctor --data-dir /mnt/teku-data
    tekops doctor --json

`health` answers one narrow question - what does `/eth/v1/node/health` say,
plus the syncing endpoint. `doctor` is the comprehensive pass: it runs a
series of checks across the Beacon API, the Prometheus metrics page, Docker
container state, and host resources (disk, memory, load), then prints a
pass/warn/fail report. It's built for two situations: an operator
self-assessing a node, and an operator pasting the output into a chat when
asking for help - which is why the report is plain aligned text rather than a
boxed table.

It accepts `--api-url` and `--metric-url` (or `$TEKOPS_API_URL` /
`$TEKOPS_METRIC_URL`), `--stack` (or `$TEKOPS_STACK`), `--data-dir` (or
`$TEKOPS_DATA_DIR`), and `--json`, the same as the commands above and below
it.

**`doctor` applies `docker ps` detection to the port defaults; the other API
commands don't.** Every other command here reads only `--stack`/`$TEKOPS_STACK`
and, if the endpoint turns out to be unreachable, suggests `--stack` in a hint
afterward. `doctor` is already running `docker ps` for the container checks, so
it applies that detection up front to `--api-url` and `--metric-url` as well -
on an Eth Docker host with no flags given, it probes `:5052` and `:8009`
straight away rather than failing against the bare-metal defaults first. One
consequence: `doctor` never prints the `--stack` hint, because detection has
already had its say by the time an endpoint would be judged unreachable.

`--data-dir` only matters on bare-metal. The disk-free check resolves its
target as `--data-dir` > `$TEKOPS_DATA_DIR` > `/var/lib/teku`. On the two
Docker stacks it ignores that default entirely and instead reads the
consensus container's own mounts, picking the most specific one; `--data-dir`
(or the environment variable) still overrides that if you pass it.

**Container checks don't appear in the report at all on bare-metal** - not as
a greyed-out "n/a" row, just absent, since a bare-metal node has no restart
count to show.

**`consensus container` and `container restarts` inspect only the consensus
client's own container** - `eth-docker-consensus-1`, `rocketpool_eth2` - the
same one `tekops logs` detects. A dead execution or validator-client
container is not checked directly; it is usually caught indirectly instead
(`execution layer` if Teku notices the execution client is offline,
`metrics endpoint` if the validator client stops answering its scrape).

#### Exit code

`0` when every check passes or the worst outcome is a warning. `1` when any
check fails. Unlike `tekops update check`, whose `0` means only that the
check itself completed, doctor's exit code is a direct function of the
findings - script against it (`tekops doctor || alert`) rather than assuming
`0` just means the command ran.

#### Checks

The thresholds below are conservative defaults picked for a home-staker
mainnet node, not measurements taken from a specific setup - treat them as a
starting point.

| Check | Pass | Warn | Fail |
| --- | --- | --- | --- |
| beacon api | ready | syncing | not ready, unreachable |
| metrics endpoint | reachable, versions found | reachable but the metric family is absent (wrong port) | unreachable |
| sync status | synced | syncing | - |
| execution layer | online | - | offline (`el_offline`) |
| optimistic head | verified | - | unverified |
| finality lag | <= 3 epochs behind | 4-6 epochs | > 6 epochs |
| peer count | >= 20 peers | 1-19 peers | 0 peers |
| validator keys | any `active_ongoing` | 0 `active_ongoing` (none loaded, or only other statuses) | - |
| duties published | any of blocks/attestations/sync messages/aggregates > 0 | all four are 0 | - |
| consensus container (Docker stacks only) | the consensus container is up | - | the consensus container is not running, or none found |
| container restarts (Docker stacks only) | 0 restarts | 1-4 restarts | >= 5 restarts |
| disk free | >= 50 GiB | < 50 GiB | < 20 GiB |
| memory available | >= 1 GiB | < 1 GiB | - |
| load average (1 min) | <= number of CPUs | > CPUs, up to 2x | > 2x CPUs |

`duties published` and `memory available` never fail, only warn: a validator
client that restarted moments ago legitimately reports zero duties, and
Teku's heap is preallocated, so low available memory (as distinct from low
*total* memory) is a healthy node's normal steady state, not a leak.

#### Sample output

    tekops doctor

      eth-docker · teku/v25.1.0 · linux x86_64

      ✔  beacon api           ready at http://localhost:5052
      ✔  metrics endpoint     teku/v25.1.0 at http://localhost:8009/metrics
      ✔  sync status          synced, head 8891234
      ✔  execution layer      online
      ✘  optimistic head      head unverified by the execution client; duties will not be performed
      ✔  finality lag         1 epoch
      ⚠  peer count           12 peers (want >= 20)
      ✔  validator keys       142 active_ongoing, 4544.00 ETH
      ⚠  duties published     none yet (normal if the validator client restarted recently)
      ✔  consensus container  eth-docker-consensus-1 up
      ✔  container restarts   0
      ✔  disk free            412.0 GiB on /var/lib/docker/volumes/eth-docker_consensus-data/_data
      ⚠  memory available     0.8 GiB of 31.3 GiB
      ✔  load average         1.20 (8 cpus)

    1 failure, 3 warnings.

No ANSI escape sequences are ever emitted, and every value interpolated into a
finding (versions, mount paths, container names, node error text) is
sanitized first, the same as everything else tekops prints from data it
didn't author.

### beacon validators, beacon duties

    tekops beacon validators <index-or-pubkey>...
    tekops beacon duties attester --epoch=N <index>...
    tekops beacon duties proposer --epoch=N

`beacon validators` looks up individual validators' on-chain status via the
Beacon API. Not to be confused with `tekops validators`, which reads local
validator-client metrics instead.

### log-level

    tekops log-level info                                  # global change
    tekops log-level debug --filter=tech.pegasys.teku.sync # scoped to loggers
    tekops log-level https://gist.github.com/someone/abc123 # a prepared body
    tekops log-level https://gist.github.com/someone/abc123 -y

Sends a `PUT` to `/teku/v1/admin/log_level` to change the node's runtime log
level - the only mutating command `tekops` has. The level is a positional
argument, e.g. `tekops log-level info`, sent to the API exactly as typed (no
case normalization). Repeat `--filter` to scope the change to one or more
logger names (e.g. `org.hyperledger.besu`, or a fully-qualified class); omit
it entirely to change the global log level - `log_filter` is left out of the
request body in that case rather than sent as `null` or `[]`.

#### Applying a prepared body from a URL

Working out which Teku packages or classes to turn up is not something an
operator can reasonably be expected to know. So the same positional also takes
an **https URL** serving the request body, which is how a Teku maintainer can
hand over exactly the right set of loggers: put the body in a gist, share the
link, and the operator runs one command.

The body is the same JSON the endpoint takes, and nothing else:

```json
{
  "level": "DEBUG",
  "log_filter": [
    "tech.pegasys.teku.networking.eth2",
    "tech.pegasys.teku.sync"
  ]
}
```

`log_filter` may be omitted for a global change. Any other key is an error
rather than something quietly ignored, so a body with `log_filters` in it
fails instead of applying a change missing the filters it was written to
carry. Requires `curl` on the host, like `tekops update` does.

Before anything is sent, tekops prints what the body would apply and asks:

    $ tekops log-level https://gist.github.com/someone/abc123
    fetched from https://gist.github.com/someone/abc123
      level:  DEBUG
      filter: tech.pegasys.teku.networking.eth2
              tech.pegasys.teku.sync
    apply? [y/N]

Answer anything but `y`/`yes` and nothing is sent. Pass `-y` to skip the
prompt when scripting. The preview and the prompt go to stderr, so `--json`
still writes nothing but JSON to stdout.

Notes on the URL:

- A gist **page** URL works; tekops rewrites it to its `/raw` form before
  fetching, dropping any `#file-...` fragment first. A
  `gist.githubusercontent.com` URL is used as-is.
- **Put one file in the gist.** A gist's `/raw` URL serves only its first
  file, so a second file is silently not what gets applied.
- Any https URL serving the body works - a gist is just the convenient one.
  Plain `http` is refused.
- `--filter` cannot be combined with a URL: the fetched body carries its own
  `log_filter`, and honouring one while discarding the other silently is worse
  than saying so.

## Metrics commands

`duties`, `validators`, and `version` read the validator client's own
Prometheus `/metrics` page instead of the Beacon API. They accept
`--metric-url` (or `$TEKOPS_METRIC_URL`) to point at a non-default metrics
endpoint (default: `http://localhost:8010/metrics`).

If the endpoint responds but doesn't export the metric being asked for, these
fail with an error rather than reporting a confident zero - pointing at the
beacon node's metrics port instead of the validator client's would otherwise
render as "this validator published nothing".

`duties` and `validators` are a local equivalent of Grafana panel queries like
`sum(validator_beacon_node_requests_total{method="...",outcome="success"})`.
`tekops` fetches the raw exposition text and filters/sums locally, since a
single node's `/metrics` page has no query engine behind it (and no `instance`
label to filter on - that's added by Prometheus at scrape time).

- **`duties`** prints published-duty counts: blocks, attestations, sync
  committee messages, aggregates.
- **`validators`** prints two tables: validator key counts grouped by status (a
  local equivalent of `sort_desc(sum by (status)
  (validator_local_validator_counts{instance=~"$system"}))`), and total
  locally-stated ETH balance (summed from `validator_local_validator_balances`,
  reported in Gwei and converted to ETH).
- **`version`** prints the running Teku version, read from the `version` label
  on whichever of `beacon_teku_version_total` or `validator_teku_version_total`
  is present on the scrape (only one exists at a time, depending on whether
  `--metric-url` points at a beacon node's or a validator client's `/metrics`
  endpoint).

## Docker deployments

### Finding the container

`tekops logs` reads a container's logs when one is named or detected. With
nothing configured it runs `docker ps` and looks for an Eth Docker
(`*-consensus-1`) or Rocket Pool (`*_eth2`) consensus container, using it only
if exactly one matches. Anything you state yourself wins over that: a path, a
`--container` name, `$TEKOPS_CONTAINER`, or `$TEKOPS_LOGS_FILE`.

If `docker ps` finds more than one consensus container, tekops cannot guess
which one you mean; it prints a note naming the candidates and falls through
to the rest of the precedence chain (`$TEKOPS_LOGS_FILE`, then the hardcoded
default path) rather than failing the session outright.

On a host that runs both a bare-metal node and Docker, `--stack bare-metal`
(or `TEKOPS_STACK=bare-metal`) turns container detection off entirely and
restores the plain file-path behaviour, since narrowing to a stack with no
container suffix can never match anything `docker ps` returns.

### Stack profiles

`--stack` sets the port defaults for a known deployment, since both Docker
stacks differ from a bare-metal node and from each other:

| | Beacon API | Metrics (validator client) |
| --- | --- | --- |
| bare-metal | 5051 | 8010 |
| eth-docker | 5052 | 8009 |
| rocketpool | 5052 | 9101 |

    tekops logs --stack rocketpool     # narrow autodetection
    tekops health --stack eth-docker   # take that stack's port defaults

Neither stack publishes those ports to the host by default, so exposing them is
a change you make in the stack itself: Eth Docker needs `cl-shared.yml` added to
`COMPOSE_FILE`, and Rocket Pool needs its "Expose API Port" setting changed from
the default of closed.

When a command can't reach an endpoint and no stack was named, the error
suggests one based on what `docker ps` shows.

### Unrecognized stacks

A stack tekops does not recognize is fully supported; it just gets no defaults.
Name the pieces directly and skip `--stack` entirely:

    export TEKOPS_CONTAINER=mynode-teku
    export TEKOPS_API_URL=http://localhost:5099
    export TEKOPS_METRIC_URL=http://localhost:9109/metrics

Every value a stack profile would supply is independently overridable, and a
profile is a layer in the precedence chain rather than a mode that locks the
rest: `tekops health --stack rocketpool --api-url http://localhost:5099` uses
the explicit API URL instead of Rocket Pool's 5052 default, and `tekops duties
--stack rocketpool` takes the Rocket Pool metrics default with nothing else
typed.

### Log colourizing under Docker

Under Docker, Teku logs in its console layout rather than the JSON a bare-metal
node is usually configured to write, and `tekops logs` colourizes both.

## Common behaviour

### --json

Every `beacon` subcommand, plus `peers`, `health`, `head`, `log-level`,
`duties`, `validators`, and `version`, accepts `--json` to print a
JSON-serialized version of the parsed response instead of the table. This is
not a raw passthrough of the wire response - e.g. `peers --json` includes a
`protocol` field that's derived locally, not sent by the API.

### Versions

`tekops --version` reports the `tekops` build itself, which is a different
question from `tekops version` (the running Teku's version). `tekops about`
prints the same build version alongside the project link, and takes no flags.

### Timeouts and TLS

Every network command gives up after 10 seconds rather than waiting forever
on a node that accepts the connection but never answers.

`tekops` is built without a TLS backend, so `--api-url` and `--metric-url`
must be `http://` URLs. It is meant to run on the node against its own local
endpoints, and dropping TLS removes `rustls` and `ring` (and all C
compilation) from the build, cutting the binary roughly in half. An `https://`
URL fails immediately with `cannot make HTTPS request because no TLS backend
is configured` rather than doing anything surprising.

## Shell completion

    tekops autocomplete           # detect the shell from $SHELL, show the plan, confirm
    tekops autocomplete zsh       # install for a named shell (bash, zsh, or fish)
    tekops autocomplete zsh -y    # skip the confirmation (--yes also works)
    tekops autocomplete zsh --print   # write the script to stdout, install nothing

Nothing is written until you have seen exactly what will be written and said
yes. Re-running is safe: the completion script is rewritten, and the rc stanza
is added once and then recognized and left alone.

What lands where, per shell:

| Shell | Completion script | rc file |
|-------|-------------------|---------|
| bash  | `~/.local/share/bash-completion/completions/tekops` | one guarded `source` line |
| zsh   | `~/.zfunc/_tekops` | `fpath` + `compinit` stanza |
| fish  | `~/.config/fish/completions/tekops.fish` | none needed |

`$XDG_DATA_HOME` and `$XDG_CONFIG_HOME` are honoured where they apply. On
macOS the bash stanza goes to `~/.bash_profile` rather than `~/.bashrc`,
because Terminal starts bash as a login shell and a login bash never reads
`.bashrc`.

bash gets a `source` line even though its directory is `bash-completion`'s own
auto-loading one, because that package is not installed by default on macOS,
whose system bash is 3.2. The generated script is self-contained, so sourcing
it directly works either way.

Start a new shell to pick the completions up.

To remove them, delete the completion script and the three marked lines from
your rc file. There is no uninstall command.

The script is generated from the CLI definition at the moment you run the
command, so it can never drift from the commands tekops actually has. It is
still a snapshot on disk, so `tekops update` re-renders any completion script
you already have installed, using the newly installed binary.

## Updating

    tekops update              # check, show current -> latest, confirm, install
    tekops update check        # check only, never installs
    tekops update latest       # install the latest release, no prompt
    tekops update 0.3.0        # install that exact release, no prompt
    tekops update -y           # take the latest without confirming (--yes also works)

An explicit version installs exactly that, older or newer, so
`tekops update <previous-version>` is also the rollback.

`tekops update check --json` prints
`{"current":"0.3.1","latest":"0.4.0","update_available":true}` and exits 0
whether or not an update exists - 0 means the check succeeded.

The download is verified against the release's `SHA256SUMS` and the new binary
is run once with `--version` before it replaces anything, so a failed update
leaves the working binary in place. Note what the checksum proves: it is
fetched from the same release as the tarball, so it catches corruption and a
wrong-platform asset, not a compromised repository. The channel guarantee is
TLS.

If tekops lives in a root-owned directory such as `/usr/local/bin`, run the
update under `sudo`. It checks for write access before downloading anything,
so the wrong invocation fails immediately.

After a successful install, any shell completion script you already have is
re-rendered by running the new binary, so a release that adds a command does
not leave you completing the old set. Nothing is installed that was not there
before, and no rc file is touched. If that step fails the update still
succeeded - it prints a warning naming `tekops autocomplete` as the fix.
