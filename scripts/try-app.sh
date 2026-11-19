#!/usr/bin/env bash
# Builds the release app signed with your Developer ID, installs it in /Applications and
# launches it. For testing on this Mac: no notarization and no update signing, so no
# passwords and no waiting on Apple. `cargo xtask release` makes the public download.
#
#   scripts/try-app.sh          build, install, launch
#   scripts/try-app.sh --dmg    also make a DMG and install from it, as a friend would
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_ID="org.commons.desktop"
DEST="/Applications/Commons.app"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
export PATH="$HOME/.cargo/bin:$PATH"

make_dmg=0
case "${1:-}" in
  "") ;;
  --dmg) make_dmg=1 ;;
  *) echo "usage: $0 [--dmg]" >&2; exit 2 ;;
esac

step() { printf '\n==> %s\n' "$*"; }

# Signing with the same Developer ID as releases keeps the app's identity stable, so the
# Keychain lets every rebuild read the saved sign-in without asking.
if [[ -z "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  APPLE_SIGNING_IDENTITY="$(security find-identity -v -p codesigning \
    | sed -n 's/.*"\(Developer ID Application: .*\)"/\1/p' | head -n 1)"
fi
if [[ -z "$APPLE_SIGNING_IDENTITY" ]]; then
  echo "No \"Developer ID Application\" identity in your keychain; see docs/release.md." >&2
  exit 1
fi
export APPLE_SIGNING_IDENTITY
# Without these Tauri signs but never notarizes.
unset APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID APPLE_API_KEY APPLE_API_ISSUER APPLE_API_KEY_PATH

step "Building (release, signed as: $APPLE_SIGNING_IDENTITY)"
(cd "$ROOT/apps/desktop" && cargo tauri build --bundles app)
BUILT="$ROOT/target/release/bundle/macos/Commons.app"
codesign --verify --deep --strict "$BUILT"

if pgrep -x commons-desktop >/dev/null; then
  step "Quitting the running Commons"
  osascript -e "tell application id \"$APP_ID\" to quit" >/dev/null 2>&1 || true
  for _ in $(seq 1 20); do pgrep -x commons-desktop >/dev/null || break; sleep 0.5; done
  if pgrep -x commons-desktop >/dev/null; then
    echo "Commons didn't quit; close it and run this again." >&2
    exit 1
  fi
fi

SRC="$BUILT"
if (( make_dmg )); then
  step "Making the DMG"
  STAGE="$(mktemp -d)"
  ditto "$BUILT" "$STAGE/Commons.app"
  ln -s /Applications "$STAGE/Applications"
  DMG="$ROOT/target/release/bundle/Commons-local.dmg"
  rm -f "$DMG"
  hdiutil create -quiet -volname Commons -srcfolder "$STAGE" -format UDZO "$DMG"
  rm -rf "$STAGE"
  MNT="$(mktemp -d)"
  hdiutil attach -quiet -nobrowse -readonly -mountpoint "$MNT" "$DMG"
  trap 'hdiutil detach -quiet "$MNT" 2>/dev/null || true' EXIT
  SRC="$MNT/Commons.app"
  echo "DMG: $DMG"
fi

step "Installing to $DEST"
rm -rf "$DEST"
ditto "$SRC" "$DEST"
# commons:// links should open this copy, not one left in a build folder.
"$LSREGISTER" -u "$BUILT" >/dev/null 2>&1 || true
"$LSREGISTER" -u "$ROOT/target/debug/bundle/macos/Commons.app" >/dev/null 2>&1 || true
"$LSREGISTER" -f "$DEST"

step "Launching"
open "$DEST"
echo "Commons $(defaults read "$DEST/Contents/Info" CFBundleShortVersionString) is running from $DEST."
