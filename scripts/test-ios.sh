#!/usr/bin/env bash
# Every check of the iOS side.
#
#   scripts/test-ios.sh        the core as iOS builds it, orb-mobile, OrbKit's
#                              tests and the app built: a few minutes, and what
#                              CI runs (.github/workflows/ci.yml)
#   scripts/test-ios.sh --ui   the app's UI tests as well: ten minutes and more,
#                              for a change to the app itself (ios/); local
#                              only, as they share from Safari and wait for
#                              notification banners
#
# The tests run on simulators of their own, created on first use, because
# every UI test wipes the app's data: "Orb Tests iPhone", and with
# ORB_IOS_IPAD=1 "Orb Tests iPad" as well. ORB_IOS_SIMULATOR names another.
# A simulator this script boots is shut down again, however it ends.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
# No incremental caches for the iOS builds: each target, feature set and
# checkout keeps a cache of its own, some 800 MB of orb-core each, and the
# iOS side adds half a dozen of them (2026-11-02: 20 GB, the disk full).
export CARGO_INCREMENTAL=0
cd "$root"
scripts/prune-target.sh
ui=false
for arg in "$@"; do
  case "$arg" in
    --ui) ui=true ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done
simulator="${ORB_IOS_SIMULATOR:-Orb Tests iPhone}"
ipad="${ORB_IOS_IPAD:+Orb Tests iPad}"

# A simulator of the tests' own, made from a device type when missing.
ensure_simulator() {
  xcrun simctl list devices available | grep -qF "    $1 (" ||
    xcrun simctl create "$1" "$2" >/dev/null
}
ensure_simulator "Orb Tests iPhone" com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro
if [[ -n "$ipad" ]]; then
  ensure_simulator "Orb Tests iPad" com.apple.CoreSimulator.SimDeviceType.iPad-Pro-11-inch-M5-12GB
fi

step() { printf '\n==> %s\n' "$*"; }
# xcodebuild's own output, cut down to the failures and the verdict.
failures() { grep -E "error:|Test Case .* failed|✘ Test .* (recorded|failed)|\*\* (BUILD|TEST) (FAILED|SUCCEEDED)" || true; }

booted() { xcrun simctl list devices booted | grep -cF "$1 (" || true; }
was_booted="$(booted "$simulator")"
ipad_was_booted="$([[ -n "$ipad" ]] && booted "$ipad" || echo 1)"
shut_down() {
  if [[ "$was_booted" == 0 ]]; then
    xcrun simctl shutdown "$simulator" 2>/dev/null || true
  fi
  if [[ "$ipad_was_booted" == 0 ]]; then
    xcrun simctl shutdown "$ipad" 2>/dev/null || true
  fi
}
trap shut_down EXIT INT TERM

step "The core as iOS builds it"
cargo clippy --all-targets --no-default-features --features mobile -- -D warnings
cargo test --lib --no-default-features --features mobile

step "orb-mobile, the Swift API"
cargo clippy -p orb-mobile --all-targets -- -D warnings
cargo test -p orb-mobile

step "The XCFramework and the bindings"
scripts/build-xcframework.sh --debug

step "OrbKit's tests on $simulator"
(cd ios/OrbKit && xcodebuild test -scheme OrbKit \
  -destination "platform=iOS Simulator,name=$simulator" -derivedDataPath ../build/kit) 2>&1 | failures

if $ui; then
  step "The app and its UI tests on $simulator"
  (cd ios && xcodegen generate --quiet && xcodebuild test -project Orb.xcodeproj -scheme Orb \
    -destination "platform=iOS Simulator,name=$simulator" -derivedDataPath build/app) 2>&1 | failures
else
  step "The app (build only; --ui runs its UI tests)"
  (cd ios && xcodegen generate --quiet && xcodebuild build -project Orb.xcodeproj -scheme Orb \
    -destination "platform=iOS Simulator,name=$simulator" -derivedDataPath build/app) 2>&1 | failures
fi

if $ui && [[ -n "$ipad" ]]; then
  step "The app's UI tests on $ipad"
  (cd ios && xcodebuild test -project Orb.xcodeproj -scheme Orb \
    -destination "platform=iOS Simulator,name=$ipad" -derivedDataPath build/app) 2>&1 | failures
fi

step "All iOS checks passed"
