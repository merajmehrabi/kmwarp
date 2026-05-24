#!/usr/bin/env bash
#
# Build, codesign, and notarize a universal-binary release of
# kmwarp-server, ready for distribution to other users.
#
# Prerequisites (one-time setup):
#
#   1. Apple Developer Program membership.
#   2. Both targets installed:
#        rustup target add aarch64-apple-darwin x86_64-apple-darwin
#   3. A "Developer ID Application" certificate in your login keychain.
#      Find its identity string with:
#        security find-identity -v -p codesigning
#      …and export it:
#        export DEVELOPER_ID_APPLICATION="Developer ID Application: Your Name (TEAMID)"
#   4. A `notarytool` keychain profile (avoids putting your app-specific
#      password on the command line). Set it up once with:
#        xcrun notarytool store-credentials kmwarp-notary \
#            --apple-id you@example.com \
#            --team-id TEAMID \
#            --password APP-SPECIFIC-PASSWORD
#
# Run:  scripts/build-mac.sh
#
# Output: target/universal/release/kmwarp-server  (signed + stapled)

set -euo pipefail
cd "$(dirname "$0")/.."

# ── Required env ────────────────────────────────────────────────────────────
: "${DEVELOPER_ID_APPLICATION:?Set DEVELOPER_ID_APPLICATION to your Developer ID Application identity}"

CRATE=kmwarp-server
NOTARY_PROFILE="${NOTARY_PROFILE:-kmwarp-notary}"

# ── Stage 1: release builds for both Mac architectures ─────────────────────
echo ">> building $CRATE (aarch64)…"
cargo build --release --target aarch64-apple-darwin -p "$CRATE"
echo ">> building $CRATE (x86_64)…"
cargo build --release --target x86_64-apple-darwin   -p "$CRATE"

# ── Stage 2: lipo into a universal binary ──────────────────────────────────
UNIVERSAL_DIR=target/universal/release
mkdir -p "$UNIVERSAL_DIR"
UNIVERSAL_BIN="$UNIVERSAL_DIR/$CRATE"

echo ">> lipo → $UNIVERSAL_BIN"
lipo -create \
    "target/aarch64-apple-darwin/release/$CRATE" \
    "target/x86_64-apple-darwin/release/$CRATE" \
    -output "$UNIVERSAL_BIN"
lipo -info "$UNIVERSAL_BIN"

# ── Stage 3: codesign ──────────────────────────────────────────────────────
echo ">> codesign with: $DEVELOPER_ID_APPLICATION"
codesign --force --sign "$DEVELOPER_ID_APPLICATION" \
    --options runtime \
    --timestamp \
    --entitlements scripts/entitlements.plist \
    "$UNIVERSAL_BIN"

# Sanity-check the signature.
codesign --verify --verbose=2 "$UNIVERSAL_BIN"

# ── Stage 4: notarize via xcrun notarytool ─────────────────────────────────
ZIP="$UNIVERSAL_BIN.zip"
echo ">> zipping for notarytool: $ZIP"
ditto -c -k --keepParent "$UNIVERSAL_BIN" "$ZIP"

echo ">> xcrun notarytool submit (profile: $NOTARY_PROFILE) — this can take a few minutes"
xcrun notarytool submit "$ZIP" --keychain-profile "$NOTARY_PROFILE" --wait

# ── Stage 5: staple the notarization ticket ────────────────────────────────
# `stapler` on a bare Mach-O binary returns an informational warning
# about the binary not being a bundle; the staple itself is no-op for
# command-line tools (Apple ships them with online notarization
# verification). The ZIP, however, can be stapled and shipped to users.
echo ">> stapler staple $ZIP"
xcrun stapler staple "$ZIP" || true

echo ""
echo "Signed + notarized:"
echo "  $UNIVERSAL_BIN"
echo "  $ZIP"
echo ""
echo "Verify locally with:"
echo "  spctl --assess --type execute -vv $UNIVERSAL_BIN"
