#!/usr/bin/env bash
# Makes a public release in one go: bump the version (optional), build the signed and
# notarized app, install it from the DMG like a new user would, then publish to GitHub
# once you've tried it and typed "publish".
#
#   scripts/release.sh            release the current version (the first release, or a rebuild)
#   scripts/release.sh patch      0.1.0 -> 0.1.1; also minor, major or an exact x.y.z
#
# It needs a clean `main` that matches GitHub's and passed CI. It asks for the update key's
# password near the end of the build. Nothing goes public until you type "publish"; stopping
# or failing before that undoes the version bump.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REPO="digitaltracer/commons"
APP_ID="org.commons.desktop"
DEST="/Applications/Commons.app"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
export PATH="$HOME/.cargo/bin:$PATH"
cd "$ROOT"

step() { printf '\n==> %s\n' "$*"; }
die() { echo "error: $*" >&2; exit 1; }

[[ "$(git branch --show-current)" == main ]] || die "release from main"
# A release is built from what's committed, so it matches what was tested and pushed.
if [[ -n "$(git status --porcelain)" ]]; then
  git status --short
  die "commit or stash the changes above first"
fi
gh auth status >/dev/null 2>&1 || die "gh isn't signed in; run: gh auth login"

# Release only what GitHub has and CI passed: the version bump is the one commit on top.
git fetch -q origin main
BASE="$(git rev-parse HEAD)"
[[ "$BASE" == "$(git rev-parse origin/main)" ]] \
  || die "local main isn't the same as GitHub's; push or pull first, so CI has checked what you release"
ci="$(gh run list -R "$REPO" --workflow ci.yml --commit "$BASE" --limit 1 \
  --json status,conclusion -q '.[0] // empty | "\(.status) \(.conclusion)"')"
case "$ci" in
  "completed success") ;;
  "") die "CI hasn't run on ${BASE:0:7} yet" ;;
  completed*) die "CI failed on ${BASE:0:7}: fix it before releasing" ;;
  *) die "CI is still running on ${BASE:0:7}; try again when it's green" ;;
esac

# A bump that never reaches GitHub is undone on any exit, so the next run doesn't skip a version.
pushed=0
undo_bump() {
  if (( pushed == 0 )) && [[ "$(git rev-parse HEAD)" != "$BASE" ]]; then
    git reset -q --keep "$BASE" && echo "Undid the unpublished version bump; main is back at ${BASE:0:7}."
  fi
}
trap undo_bump EXIT

step "Building (the update key's password is asked for near the end)"
cargo xtask release "$@"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' apps/desktop/Cargo.toml | head -n 1)"
TAG="v$VERSION"
OUT="target/release-mac/$VERSION"
DMG="$OUT/Commons_${VERSION}_universal.dmg"
TARBALL="$OUT/Commons_${VERSION}_universal.app.tar.gz"
MANIFEST="$OUT/latest.json"
for f in "$DMG" "$TARBALL" "$MANIFEST"; do [[ -f "$f" ]] || die "missing $f"; done
if gh release view "$TAG" -R "$REPO" >/dev/null 2>&1; then
  die "GitHub already has a release $TAG"
fi

step "Installing Commons $VERSION from the DMG"
if pgrep -x commons-desktop >/dev/null; then
  osascript -e "tell application id \"$APP_ID\" to quit" >/dev/null 2>&1 || true
  for _ in $(seq 1 20); do pgrep -x commons-desktop >/dev/null || break; sleep 0.5; done
  pgrep -x commons-desktop >/dev/null && die "Commons didn't quit; close it and run this again"
fi
MNT="$(mktemp -d)"
hdiutil attach -quiet -nobrowse -readonly -mountpoint "$MNT" "$DMG"
rm -rf "$DEST"
ditto "$MNT/Commons.app" "$DEST"
hdiutil detach -quiet "$MNT"
"$LSREGISTER" -f "$DEST"
spctl -a -t exec "$DEST" || die "Gatekeeper rejects the installed app"
open "$DEST"

step "Try it"
echo "Commons $VERSION is open from $DEST: sign in and look around."
read -r -p "Type \"publish\" to release $TAG to everyone, anything else to stop: " answer
if [[ "$answer" != publish ]]; then
  echo "Stopped. Nothing was published; the files stay in $OUT."
  exit 0
fi

step "Pushing main"
git push origin main
pushed=1

step "Publishing $TAG"
gh release create "$TAG" -R "$REPO" \
  --target "$(git rev-parse HEAD)" \
  --title "Commons $VERSION" \
  --generate-notes \
  "$DMG" "$TARBALL" "$MANIFEST"

echo
echo "Published: https://github.com/$REPO/releases/tag/$TAG"
echo "New users download the DMG there; installed apps offer the update within 6 hours."
