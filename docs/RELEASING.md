# Releasing

`.github/workflows/release.yml` takes a version, bumps `Cargo.toml`, runs the
suite, builds three targets, tags, and publishes tarballs plus `SHA256SUMS`.
That part needs nothing from you beyond the version number - see
[Cutting a release](#cutting-a-release).

The macOS binary is additionally signed with a Developer ID certificate and
notarized by Apple, so it runs without the "developer cannot be verified"
prompt. That part needs five repository secrets, and this document is how they
get created. You will need it again roughly every five years, when the
certificate expires.

Everything below is a one-time setup. Skip to [Cutting a release](#cutting-a-release)
if the secrets are already in place.

## Prerequisites

- An Apple Developer Program membership (paid, $99/year). There is no free path
  to a Developer ID certificate.
- Xcode installed, signed in to that Apple ID.
- `gh`, authenticated with write access to the repository.

## 1. Create the Developer ID Application certificate

Do this on the machine you want to hold the private key. The key is generated
locally and never leaves it except as the `.p12` you export in a moment.

1. Xcode, Settings, Accounts. Select your Apple ID, then **Manage Certificates…**
2. Click **+**, choose **Developer ID Application**. This option only appears if
   you are the Account Holder or an Admin on the team. On an individual
   membership you are the Account Holder.
3. Close the dialog. The certificate and its private key are now in your login
   keychain.

Then export it:

4. Open **Keychain Access**, select the **login** keychain, **My Certificates**.
5. Find `Developer ID Application: <your name> (<team id>)`. Expand the
   disclosure triangle and **confirm a private key sits underneath it**. If
   there is no key there, the export will not contain one and signing fails
   later with `found 0` rather than anything that names the real cause.
6. Right-click the certificate, not the key. **Export…**, format **Personal
   Information Exchange (.p12)**, save as `cert.p12`, set an export password,
   and keep that password.

**Do not build this file with `openssl pkcs12 -export`.** OpenSSL 3 writes a MAC
that Apple's own parser rejects, and `security import` fails with
`MAC verification failed during PKCS12 import (wrong password?)`, which points
at the wrong thing. Keychain Access writes the format `security import` accepts.
If you have no choice but to use openssl, pass `-legacy`.

## 2. Create the App Store Connect API key

An API key rather than an Apple ID plus app-specific password: nothing is bound
to a personal account with 2FA on it, and the key can be revoked on its own
without touching anything else.

1. Go to App Store Connect, **Users and Access**, the **Integrations** tab,
   **App Store Connect API**, **Team Keys**.
2. **+**, name it something like `notarization`, access role **Developer**.
3. Download the `AuthKey_<key id>.p8`. **It downloads exactly once.** Store it
   wherever you keep the `.p12`; you will want both again at renewal time.
4. Note two values from that page: the **Key ID** from the row, and the
   **Issuer ID** shown above the table, which is a uuid.

## 3. Dry run locally before touching the repository

`scripts/build-release.sh` calls `scripts/sign-macos.sh`, which is configured
entirely through the environment. That means you can run exactly what CI runs,
with the real certificate against Apple's real notarization service, on your own
machine. Do this before setting any secrets: it turns the first tagged release
from the first test of the signing path into a confirmation of it.

Keep both credential files **outside the repository** and refer to them by
absolute path. `.gitignore` covers `*.p12` and `*.p8` as a backstop, but the
reliable version of that is never putting them in the tree at all.

    export MACOS_CERT_P12="$(base64 -i ~/secure/cert.p12)"
    export MACOS_CERT_PASSWORD='<the export password from step 1>'
    export APPLE_API_KEY_P8="$(base64 -i ~/secure/AuthKey_<key id>.p8)"
    export APPLE_API_KEY_ID='<key id>'
    export APPLE_API_ISSUER='<issuer uuid>'

    scripts/build-release.sh aarch64-apple-darwin

Success is `macos signing: identity <hash>`, then a notarytool wait of anywhere
from thirty seconds to a few minutes, then `status: Accepted`, then
`macos signing: ... is signed and notarized (unstapled by design)`, then the
tarball line. Anything else, see [Troubleshooting](#troubleshooting).

Afterwards, `unset` those five variables so they do not leak into later shells,
and remove the `dist/` directory the run produced.

## 4. Set the five repository secrets

Piped rather than passed as arguments, so no credential reaches your shell
history:

    base64 -i ~/secure/cert.p12            | gh secret set MACOS_CERT_P12   --repo lucassaldanha/tekops
    base64 -i ~/secure/AuthKey_<key id>.p8 | gh secret set APPLE_API_KEY_P8 --repo lucassaldanha/tekops

    gh secret set MACOS_CERT_PASSWORD --repo lucassaldanha/tekops
    gh secret set APPLE_API_KEY_ID    --repo lucassaldanha/tekops
    gh secret set APPLE_API_ISSUER    --repo lucassaldanha/tekops

The last three prompt for the value rather than reading a file. Then confirm all
five arrived:

    gh secret list --repo lucassaldanha/tekops

**The names must match `release.yml` exactly.** A misspelled name makes GitHub
substitute an empty string, and `sign-macos.sh` reads a wholly empty environment
as "not configured" and ships an unsigned binary without failing. That
asymmetry is deliberate, because the same script runs on a dev machine that has
none of this material, and a misspelled secret name is the one way it can bite
you. The `gh secret list` check is what closes that gap.

## The self-hosted builder

The `aarch64-apple-darwin` leg of the `build` job runs on the self-hosted
runner labelled `self-hosted, macOS`, not on a GitHub-hosted macOS runner.
GitHub bills macOS minutes at a 10x multiplier and that job is the only one
that needs a Mac, so it is most of what a release costs. Everything else -
`verify`, both Linux legs, the `release` job and all of `ci.yml` - stays on
GitHub-hosted runners, which are always available and cheap.

**If the builder is offline the job queues** until the machine comes back.
GitHub cancels a job that has waited 24 hours. Nothing else in the run is
blocked until `release`, which needs every build to have finished.

The five signing secrets are handed to the builder the same way they are to a
GitHub-hosted runner. `sign-macos.sh` was already written for a real machine -
it saves and restores the keychain search list in an `EXIT` trap, rather than
assuming a throwaway filesystem - so nothing about it changes here.

### What has to be installed on it

A GitHub-hosted macOS runner arrives with Xcode, rustup and git already on it.
The builder is a machine someone set up, so every one of those is a thing that
has to be there and stay there. This is the whole list.

1. **The runner software, labelled `self-hosted` and `macOS`.** Those two
   labels are what `release.yml`'s matrix matches on. `macOS` is applied
   automatically by the runner's installer; `self-hosted` likewise. The `X64`
   label the current builder also carries is deliberately not matched, so
   replacing the machine with an Apple Silicon one needs no change to the
   workflow.

2. **The Xcode command line tools**, for `codesign`, `spctl`,
   `xcrun notarytool`, `ditto`, `security`, `git`, and the linker `cargo`
   invokes.

       xcode-select --install

   The full Xcode app is not needed on the builder. It is needed once, on your
   own machine, to create the certificate in step 1 of this document - a
   different machine and a different job. `notarytool` arrived in the Xcode 13
   tools; anything older has only the retired `altool`, which this repo does
   not use.

3. **rustup, and nothing else from Rust.**

       curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

   That is the entire Rust setup. `rust-toolchain.toml` names the channel, the
   components and the targets, so the first `cargo` invocation inside the
   checkout installs 1.98.0 and the `aarch64-apple-darwin` standard library on
   its own. **Do not run `rustup target add` by hand** - the manifest already
   covers it, and a target added outside the pinned toolchain is a second place
   for it to go stale. **Do not `brew install rust`.** A Homebrew Rust ignores
   `rust-toolchain.toml` in silence, which is the exact condition
   `scripts/check-toolchain.sh` fails the release on, and on an Intel Mac it
   installs into `/usr/local/bin`, which is on launchd's default `PATH` and can
   therefore end up ahead of the rustup shim.

   `brew install rustup` is a real rustup and does work, but it is the worse
   path here: the formula is keg-only because it conflicts with `rust`, so it
   symlinks nothing and needs `$(brew --prefix rustup)/bin` on `PATH` on top of
   the `~/.cargo/bin` the toolchains still install into. Two entries to get
   right instead of one, for no gain.

4. **rustup in `~/.cargo/bin`, which is where its own installer puts it.**
   Nothing else on the builder has to be configured for this, because
   `release.yml` adds that directory to the job's `PATH` itself - but the
   reason it has to is worth knowing, since it is the sharp edge here and it
   produces a failure that reads as if Rust were missing when it is installed
   and working:

       rustup is not installed, so rust-toolchain.toml's '1.98.0' pin does nothing here
       this host would build with: no rustc at all

   rustup's installer appends a line to `~/.zprofile` or `~/.bash_profile`. The
   runner is not a login shell and never reads either, so a LaunchAgent runner
   inherits launchd's `PATH` and sees no `rustc`. `rustup --version` in your
   terminal proves nothing about what the job sees.

   The runner can be told this on the machine - it reads a `.path` file from
   its own root directory and uses it as `PATH` for every job - and that is
   still how a proxy or any other variable gets in (via a `.env` file in the
   same directory). It is the wrong place for this one: it is state on a
   machine nobody can inspect from the repo, and reinstalling the runner
   silently takes it away again.

   So if you see that message *after* this change, `PATH` is not the cause.
   Either rustup is not installed (step 3), or it is installed somewhere other
   than `~/.cargo/bin` - `brew install rustup` being the way that happens.
   `check-toolchain.sh` tells the two apart: it adds a `note:` line naming
   `~/.cargo/bin/rustup` when the binary is there and only the `PATH` was
   wrong.

5. **A logged-in user session.** `sign-macos.sh` creates a throwaway keychain
   and imports the certificate into it. Keychain operations need a user
   session, so run the runner as a LaunchAgent (`./svc.sh install`) or from a
   terminal - a LaunchDaemon with no session fails at the import.

6. **Outbound HTTPS to Apple.** Three separate steps need it and each fails
   differently: `codesign --timestamp` fetches a secure timestamp,
   `notarytool submit` uploads and waits, and `spctl --assess` performs a live
   ticket lookup. A builder that can reach GitHub but not Apple gets through
   the compile and fails in signing.

The builder does **not** need Docker, `jq`, or anything else this repo shells
out to - `read-version.sh` and `check-toolchain.sh` are deliberately plain
`awk`. It does not need Apple Silicon either: nothing in `build-release.sh` or
`sign-macos.sh` ever *executes* the binary it produces, which is what makes
building, signing and notarizing an arm64 binary on an Intel Mac work.

### Verifying the builder

Run this on the builder, from a checkout, after setting it up and after any
change to the machine:

    xcode-select -p
    git --version
    xcrun notarytool --version
    scripts/check-toolchain.sh

`check-toolchain.sh` is the one that matters and is the same check the `build`
job runs first. Expect `toolchain '<version>' is active`. Both of its failure
messages name what they found, so neither needs interpreting - but read step 4
above before concluding from `no rustc at all` that Rust is missing.

That sequence confirms the tools. To confirm the build path itself, run the
real thing:

    scripts/build-release.sh aarch64-apple-darwin

Without the five signing variables exported, `sign-macos.sh` prints
`no MACOS_CERT_P12, leaving ... unsigned` and exits 0, so this exercises the
compile, the cross-target link and the packaging without touching Apple. With
them exported it is the full dry run from
[step 3](#3-dry-run-locally-before-touching-the-repository), which is worth
doing on the builder once rather than only on your own machine.

Both of those run in *your* environment, not the runner's, so they cannot catch
a `PATH` problem. Dispatching a release is what proves that end to end, and the
`build` job fails in about a second when it is wrong.

## Cutting a release

Run the workflow and give it the version. Nothing is edited, committed or
tagged by hand.

    gh workflow run release.yml -f version=X.Y.Z

Or from the **Actions** tab: **Release**, **Run workflow**, type the version.
Either way the run:

1. Rejects the version if it is not `X.Y.Z`, if that tag already exists, or if
   it does not sort above the version `Cargo.toml` currently declares.
2. Bumps `Cargo.toml` and `Cargo.lock` and pushes `chore(release): vX.Y.Z` to
   the branch the workflow was dispatched from.
3. Runs the suite and clippy against that commit.
4. Builds, signs and notarizes the three targets.
5. Creates the tag and publishes the release with generated notes, the three
   tarballs and `SHA256SUMS`.

**The tag is created last, by `gh release create --target`.** So a failing
test or a broken macOS signing leg leaves a bump commit on the branch and no
tag - revert the commit, or fix forward and dispatch the same version again.
A tag that had already been pushed would have to be deleted first, and a
deleted tag that someone has already fetched is worse than a revert.

A hand-pushed `vX.Y.Z` tag still triggers the same workflow and skips the
prepare step, for re-running a release or cutting one from a commit that is
not the branch head. On that path `Cargo.toml` has to already declare the
matching version, exactly as before - `scripts/check-version.sh` fails the run
at its first step otherwise.

`scripts/test-prepare-release.sh` exercises the bump script against a
throwaway clone with a bare repository standing in for origin, so the
validation rules can be changed without a real release as the test. It is a
standalone script, like `scripts/test-check-version.sh` - neither is part of
`scripts/check.sh`, so neither runs on pre-push.

Then:

1. Watch the `aarch64-apple-darwin` leg of the `build` job. It is the only one
   that signs, and it is where a lapsed certificate or a revoked key surfaces.
2. Confirm the published asset independently:

       spctl --assess -vv -t open --context context:primary-signature tekops

   Expect `accepted` and `source=Notarized Developer ID`. This is Gatekeeper's
   own assessment and performs a live ticket lookup against Apple, so it is the
   same question an end user's machine asks.

   **Do not use `codesign -R "=notarized"` for this.** It reads only local state
   (a stapled ticket, or a cached earlier assessment) and never does the lookup,
   so on a machine that has not already assessed that exact binary it reports
   failure for one that is genuinely notarized. It will mislead you on a fresh
   download. Running the `spctl` command above first is what makes a subsequent
   `codesign` check pass, which makes the failure look like a timing problem
   when it is not one.

   `spctl -t exec` is also wrong here: on a bare command-line binary it reports
   `rejected (the code is valid but does not seem to be an app)` regardless of
   notarization, because it expects a bundle.

## Building and the local gate

These two are day-to-day rather than release-day, but they are the same
machinery and the reasoning belongs next to it.

### `scripts/check.sh`

Work happens directly on `master` pre-1.0 - worktrees to keep parallel work
separate, not branches-plus-PRs - so nothing between the editor and `origin`
reviews a change. `check.sh` is that review: it runs the same three gates as
`ci.yml`, in the same order, and `.githooks/pre-push` runs it on every push.
`git push --no-verify` skips it.

The hook is versioned in `.githooks/` rather than living in `.git/hooks/`, so it
travels with the repo. `git config core.hooksPath .githooks` is what activates
it and has to be run once per clone. A push that only deletes refs exits early
rather than building anything.

`tests/ci_gates.rs` is what keeps the promise honest. `check.sh` is only useful
if passing it locally means CI passes too, and the two files are in different
languages with nothing reading the other, so the test parses the cargo
invocations out of both and asserts they are the same list in the same order. A
gate added, removed or reworded on one side fails there rather than as a
surprise red build after a push that was supposed to be pre-verified. Its
failure message prints both lists, so the drift is visible without opening
either file.

### Cross-compiling for the node

    scripts/build-release.sh x86_64-unknown-linux-musl
    scp dist/tekops-v*-x86_64-linux.tar.gz <node>:

Don't hand-roll a `docker run` for this; the script is what CI runs, and a
hand-typed variant produces a binary that won't hash the same.

**The dev machine's Rust is Homebrew-installed, not `rustup`.** There is no
`rustup target add` there, and the `cross` tool doesn't work either - it shells
out to `rustup toolchain list` even though the actual build runs in Docker -
which is why the Linux targets build inside a container at all. The script
forces `--platform linux/amd64`, since that machine is Apple Silicon and Docker
would otherwise pick an `aarch64` image.

Both Linux binaries are statically linked with no runtime deps on the node
(`file` confirms `static-pie linked` for x86_64 and `statically linked` for
aarch64), so nothing but that one file has to reach it.

## How the workflows are shaped

Everything above is what to type. This section is why the pieces are arranged
the way they are, for whoever next changes `release.yml`, `ci.yml` or the
scripts they call.

### `prepare` is a job, not steps on `verify`

A dispatched run bumps the version first; a hand-pushed `v*` tag has already
had that done. `prepare` is the one place the two paths differ, so it exists as
a job that both resolve through, emitting the same pair of outputs (`tag`,
`sha`) that every later job checks out and publishes. Nothing downstream
special-cases the trigger.

Three things about that shape are load-bearing:

- **The tag is created last**, by `gh release create --target <sha>` in the
  `release` job, so a failed build leaves a revertable bump commit rather than a
  tag pointing at a release that was never published - and the same version can
  be dispatched again without deleting anything.
- **Every job checks out `needs.prepare.outputs.sha` explicitly.** A
  `workflow_dispatch` run's default ref is the branch as it stood when the run
  started, which is the commit *before* the bump. The default checkout would
  build the old version and fail `check-version.sh`.
- **The bump commit is pushed with the default `GITHUB_TOKEN`**, which by design
  does not trigger workflows, so `prepare` pushing to master cannot set off a
  second release run. If that push is ever switched to a PAT, it will.

`prepare-release.sh` refuses a version that is not `X.Y.Z`, one whose tag
already exists, or one that does not `sort -V` above the current manifest - the
typo case, which the tag check cannot see because that tag genuinely does not
exist yet. It also runs `cargo metadata --locked` before pushing, so a
`Cargo.lock` that `--locked` would reject fails there rather than in `verify`,
after the commit has landed.

### Pins

- **Container images are pinned by `@sha256:` digest, never by tag.**
  `clux/muslrust:stable` moves whenever Rust ships, so the same tag yields a
  different compiler and the published checksums stop meaning anything. Refresh
  a digest with `docker pull` followed by
  `docker inspect --format='{{index .RepoDigests 0}}'`.
- **`rust-toolchain.toml` pins an exact version, never `stable`.** It is a
  rustup feature, and the dev machine runs Homebrew Rust, so it is silently
  ignored on a bare host build and governs only CI and the containers - verified:
  the image bundles Rust 1.96.1 but `rustc --version` inside it reports the
  pinned 1.98.0. Local reproducibility checks must therefore go through
  `scripts/build-release.sh`, not a host `cargo build`. Neither workflow
  installs a toolchain of its own, precisely so this file stays the only source
  of truth.

  That silence is why `scripts/check-toolchain.sh` exists and why the macOS
  build calls it. A GitHub-hosted runner ships rustup, so the pin binds for free
  and the check can only pass there; the self-hosted builder's Rust is whatever
  was installed on it, and the macOS leg is the only job that compiles on the
  host rather than inside the pinned container. An unpinned compiler produces a
  working binary and reports nothing, so without the check the drift has no
  symptom at all - it just ships. The check reads the effective
  `rustc --version` rather than `rustup show`, since something ahead of the shim
  on `PATH` is what cargo would actually run. On the dev machine it fails by
  design, and the version it names (Homebrew's 1.98.1 against the pinned
  1.98.0) is the drift it is built to catch.
- **Every build passes `--locked`**, so a drifted `Cargo.lock` fails the build
  instead of quietly resolving a different dependency graph.

### `release` depends on every build job

A macOS failure means no release at all, including the Linux binaries. Assets
that disagree about which platforms a version supports are worse than no assets.

That is also why the signing step is **not** allowed to soft-fail. The
certificate expires (Developer ID certificates last five years) and the API key
can be revoked; either one lapsing blocks the Linux assets too, which is the
same trade this line already makes deliberately. A release whose macOS asset is
silently unsigned is the assets-disagree failure in a quieter form.

### What `sign-macos.sh` guarantees

It runs from `build-release.sh`'s `aarch64-apple-darwin` arm, between the
`cargo build` and the packaging, so the tarball carries the signed binary and
`SHA256SUMS` - computed later, in the `release` job - covers it with no change.

- **An absent `MACOS_CERT_P12` is a no-op with a printed note, but a *partial*
  environment is a hard failure.** The script sits in the same
  `build-release.sh` that runs on the dev machine, which has none of the
  material and must keep building that target. A half-set environment, by
  contrast, can only mean the workflow's secrets drifted, and quietly shipping
  an unsigned binary is the exact outcome the script exists to prevent.
- **`release.yml` names the secrets in a second, `if:`-gated build step rather
  than on the shared one.** Both steps run the same command. Naming the secrets
  once unconditionally would put the signing key in the environment of the two
  Linux runners as well, which sign nothing.
- **The notarization result is read out of notarytool's output, not from its
  exit code.** Older `notarytool submit --wait` exits 0 on a submission it
  waited for and that came back `Invalid`. The script greps for
  `status: Accepted`, which is the authority here: that is Apple stating a
  ticket was issued, and it cannot lag.
- **The assessment is `spctl -t open --context context:primary-signature`.** The
  reasoning, including why `codesign -R "=notarized"` and `-t exec` are both
  wrong for it, is under [Cutting a release](#cutting-a-release) - it was got
  wrong twice before settling. `spctl` performs a live lookup, so it needs
  network and is retried for blips only; a settled rejection is fatal.
- **The signing identity is read back out of the throwaway keychain, not passed
  in as a sixth secret.** That keychain holds exactly the certificate just
  imported, so "not exactly one Developer ID Application identity" means the
  `.p12` was not what it claimed to be, and is an error rather than a guess.
  `security set-key-partition-list` is what keeps `codesign` from hanging on a
  confirmation dialog no runner could answer, and the keychain search list is
  saved and restored by the `EXIT` trap because the script can also run on a
  real machine.

### `ci.yml`

`cargo fmt --all -- --check`, then tests, then clippy, on every push and PR. The
formatting gate goes first because it is the cheapest to fail, and it exists
because without it the tree silently drifted to 166 rustfmt hunks across 11
files. `rustfmt` is listed in `rust-toolchain.toml`'s components so that step
does not depend on the runner image happening to ship it.

### What "reproducible" covers

The claim covers the **binary**, not the tarball, and Linux only. tar metadata
differs between GNU tar in the container and bsdtar on macOS, and the macOS
runner image (Xcode, SDK) drifts on GitHub's schedule and cannot be pinned.

The macOS binary is additionally **not reproducible by construction**, not
merely in practice: `codesign --timestamp` embeds a secure timestamp fetched
from Apple's server at signing time, so two builds of the same commit differ in
bytes no matter what else is pinned.

### Asset names

**Release assets are named `<arch>-<os>`, not by the cargo target triple.**
`scripts/build-release.sh` still *takes* a triple - it has to, it passes it to
`cargo build --target` - but each arm of its `case` also sets `asset_target`, so
`x86_64-unknown-linux-musl` ships as `tekops-v<version>-x86_64-linux.tar.gz`.
The triple's vendor field ("unknown") carries no information, and "musl" is
implied because every Linux build here is static. The strings in that `case`
must match `src/update.rs`'s `TARGET` constants exactly or `tekops update` 404s;
see that module's entry in `docs/ARCHITECTURE.md` for the rest.

## Troubleshooting

| Message | Cause |
| --- | --- |
| `MAC verification failed during PKCS12 import` | Wrong `MACOS_CERT_PASSWORD`, or a `.p12` built by OpenSSL 3 instead of exported from Keychain Access. See step 1 |
| `expected 1 Developer ID Application identity, found 0` | The `.p12` carries no private key (step 1.5), or holds an Apple Development certificate rather than a Developer ID Application one, or the certificate has expired |
| `expected 1 Developer ID Application identity, found 2` | More than one identity was exported into the `.p12` |
| `MACOS_CERT_P12 is set but <VAR> is not` | One secret is missing or misspelled |
| `notarization was not accepted` | The notarytool output printed directly above says why |
| `Gatekeeper did not accept ... as notarized` | The `spctl` assessment settled on a rejection after three tries. Check network reachability to Apple from the runner first; a genuine rejection means the ticket was never issued for those bytes |
| Build succeeds, log reads `no MACOS_CERT_P12, leaving ... unsigned` | The secret is not reaching the runner. Check the name against `release.yml` |
| `rustup is not installed ...` / `this host would build with: no rustc at all` | Either rustup is not on the builder, or it is and `~/.cargo/bin` is not on the runner's `PATH`. See [the builder's step 4](#what-has-to-be-installed-on-it) |
| `rust-toolchain.toml pins '<a>' but rustc reports '<b>'` | Something is ahead of the rustup shim on the runner's `PATH`, usually a Homebrew Rust |

## Why the macOS binary is not stapled

`xcrun stapler` attaches a notarization ticket to `.app`, `.dmg` and `.pkg`
only. A bare Mach-O executable cannot carry one, and no flag changes that. macOS
therefore resolves the ticket online, and only for a copy carrying the
`com.apple.quarantine` attribute, which a browser download sets and `curl` and
`gh release download` do not. The install path `README.md` documents never
reaches that check at all.

Shipping a stapled `.pkg` would work offline, at the cost of changing the asset
shape that `src/update.rs`'s `TARGET` constants and `download_url` both encode.

## Notes on secrets in a public repository

- `release.yml` runs on `workflow_dispatch` and on `push` of a `v*` tag. Both
  require write access to the repository, so a pull request cannot reach the
  signing secrets. Do not add a `pull_request` trigger to this workflow.
- `ci.yml` uses `pull_request`, not `pull_request_target`, so a fork's pull
  request runs without repository secrets. Do not change that trigger.
- The **Team ID is not a secret.** It is embedded in every signed binary and
  readable with `codesign -dv`. Only the `.p12`, its password and the `.p8` are
  credentials.
- Rotate by repeating steps 1 through 4. Revoke the old certificate at
  developer.apple.com and the old key in App Store Connect once a release has
  gone out on the new ones.
