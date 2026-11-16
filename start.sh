#!/usr/bin/env bash
# Compile and start the app from this checkout, without installing it.
#
#   ./start.sh             debug build, as an app bundle
#   ./start.sh --release   optimised build: slower to compile, faster to run
#   ./start.sh --dev       the bare binary, with the frontend reloading on save
#
# Only a bundle may use the notification centre, so --dev schedules no
# reminders and announces no new mail.
#
# Runs against ~/.config/orb/config.yml unless ORB_CONFIG names another one:
#
#   ORB_CONFIG="$PWD/.claude/orb/config.yml" ./start.sh

set -euo pipefail

cd "$(dirname "$0")/app"
../scripts/prune-target.sh

if [[ ! -d node_modules ]]; then
  pnpm install --frozen-lockfile
fi

profile=debug
case "${1:-}" in
  --dev) shift; exec pnpm tauri dev "$@" ;;
  --release) shift; profile=release ;;
esac

# Signed with your own signing identity, so the permissions macOS
# granted Orb hold for this bundle too; an ad-hoc signature would be asked
# for them again after every build.
identities=$(security find-identity -v -p codesigning 2>/dev/null)
identity=""
for kind in "Developer ID Application" "Apple Development"; do
  identity=$(sed -n "s/.*\"\($kind: [^\"]*\)\".*/\1/p" <<<"$identities" | head -n 1)
  [[ -n "$identity" ]] && break
done
if [[ -n "$identity" ]]; then
  export APPLE_SIGNING_IDENTITY="$identity"
fi

build=(pnpm tauri build --bundles app)
[[ "$profile" == debug ]] && build+=(--debug)
"${build[@]}" "$@"

# The executable inside the bundle rather than `open`, so the app keeps this
# terminal for its output and ORB_CONFIG reaches it.
bundle="../target/$profile/bundle/macos/Orb.app"
executable=$(defaults read "$PWD/$bundle/Contents/Info.plist" CFBundleExecutable)
exec "$bundle/Contents/MacOS/$executable"
