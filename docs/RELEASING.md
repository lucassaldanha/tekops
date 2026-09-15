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

The builder needs three things:

1. **`rustup`, not a Homebrew or package-manager Rust.** `rust-toolchain.toml`
   is a rustup feature; without rustup it is ignored in silence and the
   release binary is built by whatever compiler is installed. The `build` job
   runs `scripts/check-toolchain.sh` before compiling, which fails the release
   rather than shipping that. Run it by hand on the builder to check the setup
   before a release depends on it:

       scripts/check-toolchain.sh

   Expect `toolchain '<version>' is active`. Both failure messages name what
   they found, so neither needs interpreting.

2. **The Xcode command line tools**, for `codesign`, `spctl` and
   `xcrun notarytool`.

3. **A logged-in user session.** `sign-macos.sh` creates a throwaway keychain
   and imports the certificate into it. Keychain operations need a user
   session, so run the runner as a LaunchAgent (`./svc.sh install`) or from a
   terminal - a LaunchDaemon with no session fails at the import.

The builder does **not** need Docker unless the Linux legs are moved onto it
too; those run on GitHub's Linux runners, which is where their pinned
container is pulled.

The five signing secrets are handed to the builder the same way they are to a
GitHub-hosted runner. `sign-macos.sh` was already written for a real machine -
it saves and restores the keychain search list in an `EXIT` trap, rather than
assuming a throwaway filesystem - so nothing about it changes here.

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
validation rules can be changed without a real release as the test.

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
