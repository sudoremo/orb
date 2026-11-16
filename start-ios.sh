#!/usr/bin/env bash
# Build the iOS app from this checkout and start it in the Simulator.
#
#   ./start-ios.sh                  keeping what the app already holds
#   ./start-ios.sh --fresh          from the setup screen, nothing left over
#   ./start-ios.sh --test-account   set up on the test account of
#                                   .claude/orb/config.yml, no password typed
#
# ORB_IOS_SIMULATOR names the simulator (default: iPhone 17 Pro). The UI tests
# use simulators of their own, so what is set up here stays.

set -euo pipefail

root="$(cd "$(dirname "$0")" && pwd)"
# No incremental caches for the iOS builds: each target, feature set and
# checkout keeps a cache of its own, some 800 MB of orb-core each, and the
# iOS side adds half a dozen of them (2026-11-02: 20 GB, the disk full).
export CARGO_INCREMENTAL=0
cd "$root"
simulator="${ORB_IOS_SIMULATOR:-iPhone 17 Pro}"
fresh=false
test_account=false
for arg in "$@"; do
  case "$arg" in
    --fresh) fresh=true ;;
    --test-account) fresh=true; test_account=true ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

# The test account's server, user and password file. .claude/ is ignored by
# git, so a worktree reads the main checkout's.
launch_env=()
if $test_account; then
  config="$root/.claude/orb/config.yml"
  if [[ ! -f "$config" ]]; then
    main="$(git worktree list --porcelain | sed -n '1s/^worktree //p')"
    config="$main/.claude/orb/config.yml"
  fi
  [[ -f "$config" ]] || { echo "no test config at $config" >&2; exit 1; }
  value() { sed -n "s/^ *$1: *//p" "$config" | head -1; }
  password_file="$(value imap_password_command | sed -n 's/^cat //p')"
  launch_env=(
    "SIMCTL_CHILD_ORB_SETUP=$(value imap_server):$(value imap_port):$(value imap_username):Tasks"
    "SIMCTL_CHILD_ORB_SETUP_PASSWORD_FILE=$password_file"
  )
fi

echo "==> Building"
scripts/build-xcframework.sh --debug >/dev/null
(cd ios && xcodegen generate --quiet && xcodebuild build -quiet -project Orb.xcodeproj -scheme Orb \
  -destination "platform=iOS Simulator,name=$simulator" -derivedDataPath build/app)

echo "==> Starting $simulator"
xcrun simctl boot "$simulator" 2>/dev/null || true
open -a Simulator
xcrun simctl bootstatus "$simulator" -b >/dev/null

app=ios/build/app/Build/Products/Debug-iphonesimulator/Orb.app
bundle="$(/usr/libexec/PlistBuddy -c 'Print CFBundleIdentifier' "$app/Info.plist")"
xcrun simctl terminate "$simulator" "$bundle" 2>/dev/null || true
if $fresh; then
  xcrun simctl uninstall "$simulator" "$bundle"
fi
xcrun simctl install "$simulator" "$app"
env ${launch_env[@]+"${launch_env[@]}"} xcrun simctl launch "$simulator" "$bundle" >/dev/null
echo "==> Orb is running on $simulator. Stop it with: xcrun simctl shutdown \"$simulator\""
