#!/usr/bin/env bash
# Signs one macOS binary with a Developer ID certificate and notarizes it with
# Apple, in place, before build-release.sh packages it.
#
# Usage: scripts/sign-macos.sh <path-to-binary>
#
# Configured entirely through the environment, because the only caller that has
# the material is the release workflow:
#
#   MACOS_CERT_P12       base64 of the "Developer ID Application" .p12 export
#   MACOS_CERT_PASSWORD  the password that .p12 was exported with
#   APPLE_API_KEY_P8     base64 of the App Store Connect API key (.p8)
#   APPLE_API_KEY_ID     that key's id
#   APPLE_API_ISSUER     that key's issuer uuid
#
# With MACOS_CERT_P12 unset this prints a note and exits 0, leaving the binary
# as the linker produced it. That is every build outside the release workflow,
# including every one on the dev machine, and it has to keep working: this
# script sits in build-release.sh's aarch64-apple-darwin arm, which is the same
# script CI runs. A *partially* configured environment is a different thing and
# fails loudly, since it can only mean the workflow's secrets drifted.
#
# An App Store Connect API key rather than an Apple ID plus app-specific
# password: nothing here is bound to a personal account with 2FA on it, and the
# key is revocable on its own without touching anything else.
#
# The binary is signed and notarized but NOT stapled, because a bare Mach-O
# executable cannot carry a stapled ticket - `stapler` handles .app, .dmg and
# .pkg only. Gatekeeper therefore looks the ticket up with Apple on first run of
# a quarantined copy, which needs the machine to be online. Only a browser
# download sets quarantine; curl and `gh release download` do not, so the
# install path README.md documents never reaches that check at all.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

binary="${1:-}"
if [ -z "$binary" ]; then
  echo "usage: sign-macos.sh <path-to-binary>" >&2
  exit 1
fi
[ -f "$binary" ] || { echo "no binary at $binary" >&2; exit 1; }

if [ -z "${MACOS_CERT_P12:-}" ]; then
  echo "macos signing: no MACOS_CERT_P12, leaving $binary unsigned"
  exit 0
fi

for var in MACOS_CERT_PASSWORD APPLE_API_KEY_P8 APPLE_API_KEY_ID APPLE_API_ISSUER; do
  if [ -z "${!var:-}" ]; then
    echo "macos signing: MACOS_CERT_P12 is set but $var is not" >&2
    exit 1
  fi
done

work="$(mktemp -d)"
keychain="$work/signing.keychain-db"
# Restoring the search list matters even though the runner is ephemeral: this
# script also runs on a real machine the moment someone exports the variables
# locally, and leaving a deleted keychain in the user's search list breaks every
# later `security` call with a file-not-found.
original_keychains="$(security list-keychains -d user | tr -d '"' | xargs)"
cleanup() {
  # shellcheck disable=SC2086
  security list-keychains -d user -s $original_keychains >/dev/null 2>&1 || true
  security delete-keychain "$keychain" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

keychain_password="$(openssl rand -base64 24)"
security create-keychain -p "$keychain_password" "$keychain"
# Without an explicit setting a new keychain locks after 5 minutes of idle, and
# notarization can wait longer than that with the keychain still needed after.
security set-keychain-settings -lut 21600 "$keychain"
security unlock-keychain -p "$keychain_password" "$keychain"

printf '%s' "$MACOS_CERT_P12" | base64 -d > "$work/cert.p12"
# -x: the private key cannot be exported back out of the keychain. -T: codesign
# may use it without a UI prompt, which nothing on a runner could answer.
security import "$work/cert.p12" -k "$keychain" -P "$MACOS_CERT_PASSWORD" \
  -T /usr/bin/codesign -x
rm -f "$work/cert.p12"
# The ACL added by -T is still gated behind a confirmation dialog until the
# partition list says otherwise. Without this codesign hangs rather than fails.
security set-key-partition-list -S apple-tool:,apple: -s -k "$keychain_password" \
  "$keychain" >/dev/null

# codesign searches the keychain search list, not a keychain named on the
# command line, so the throwaway one has to join it. It goes first, and the
# trap puts the list back.
# shellcheck disable=SC2086
security list-keychains -d user -s "$keychain" $original_keychains

# The identity is read back out of the keychain rather than passed in as a
# sixth secret: this keychain holds exactly the one certificate that was just
# imported, so anything else means the .p12 was not what it claimed to be.
identities="$(security find-identity -v -p codesigning "$keychain" \
  | grep '"Developer ID Application:' || true)"
identity_count="$(printf '%s' "$identities" | grep -c . || true)"
if [ "$identity_count" != "1" ]; then
  echo "macos signing: expected 1 Developer ID Application identity, found $identity_count" >&2
  exit 1
fi
identity="$(printf '%s' "$identities" | awk '{print $2}')"
echo "macos signing: identity $identity"

# --options runtime (hardened runtime) and --timestamp (a secure timestamp from
# Apple's server, so this needs network) are both notarization requirements, not
# optional hardening: without either, notarytool rejects the submission.
codesign --force --timestamp --options runtime --sign "$identity" "$binary"
codesign --verify --strict --verbose=2 "$binary"

printf '%s' "$APPLE_API_KEY_P8" | base64 -d > "$work/key.p8"
# notarytool takes .zip, .dmg or .pkg only, so the zip is a submission vehicle
# and nothing more - the tarball build-release.sh goes on to write is what
# ships, carrying this same signed binary.
ditto -c -k "$binary" "$work/submission.zip"

echo "macos signing: submitting for notarization"
submit_output="$(xcrun notarytool submit "$work/submission.zip" \
  --key "$work/key.p8" \
  --key-id "$APPLE_API_KEY_ID" \
  --issuer "$APPLE_API_ISSUER" \
  --wait --timeout 30m 2>&1)" || true
rm -f "$work/key.p8"
echo "$submit_output"

# The status is checked out of the output rather than trusted to the exit code:
# older notarytool exits 0 on an Invalid submission that it waited for, and a
# release that silently ships an unnotarized binary is the whole failure this
# script exists to prevent.
if ! printf '%s' "$submit_output" | grep -q "status: Accepted"; then
  echo "macos signing: notarization was not accepted" >&2
  exit 1
fi

# -R "=notarized" asks whether a ticket for this exact code is visible. On an
# unstapled binary there is nothing on disk to read, so it is resolved over the
# network and then cached locally - which makes it a *propagation* check, not
# the authority. `status: Accepted` above is the authority: that is Apple
# stating the ticket was issued.
#
# The lag between the two is real and was measured here, not assumed: minutes
# after a submission came back Accepted this failed on the built binary, and
# passed on the same unchanged bytes shortly after. A cold CI runner checking
# seconds after Accepted is the likeliest case of all to see it.
#
# So: retry, and then warn rather than fail. Failing a release over propagation
# lag would block it in exactly the case where everything worked, and the thing
# this script exists to prevent - shipping a binary that was never notarized -
# is already caught by the Accepted check above, which cannot lag.
notarized=""
for attempt in 1 2 3 4 5; do
  if codesign --verify --strict -R "=notarized" --verbose=2 "$binary"; then
    notarized="yes"
    break
  fi
  if [ "$attempt" != "5" ]; then
    echo "macos signing: ticket not visible yet (attempt $attempt of 5), retrying in 15s"
    sleep 15
  fi
done

if [ -n "$notarized" ]; then
  echo "macos signing: $binary is signed and notarized (unstapled by design)"
else
  echo "macos signing: WARNING - notarization was Accepted but the ticket is not yet" >&2
  echo "macos signing: WARNING - visible to this machine. The binary is signed and the" >&2
  echo "macos signing: WARNING - submission succeeded; Gatekeeper resolves the ticket" >&2
  echo "macos signing: WARNING - online on first run. Re-check with:" >&2
  echo "macos signing: WARNING -   codesign --verify --strict -R \"=notarized\" -vv <binary>" >&2
  echo "macos signing: $binary is signed and notarized (ticket not yet visible here)"
fi
