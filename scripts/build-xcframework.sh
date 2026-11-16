#!/usr/bin/env bash
# Build orb-mobile for the iPhone and its simulator, generate the Swift
# bindings, and put both into ios/OrbKit, the Swift package the app links.
#
#   scripts/build-xcframework.sh            # release
#   scripts/build-xcframework.sh --debug
#
# Nothing it writes is committed (ios/OrbKit/.gitignore).
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
# No incremental caches for the iOS builds: each target, feature set and
# checkout keeps a cache of its own, some 800 MB of orb-core each, and the
# iOS side adds half a dozen of them (2026-11-02: 20 GB, the disk full).
export CARGO_INCREMENTAL=0
cd "$root"

profile=release
cargo_profile=(--release)
if [[ "${1:-}" == "--debug" ]]; then
  profile=debug
  cargo_profile=()
fi

target_dir="$(cargo metadata --format-version 1 --no-deps | sed -E 's/.*"target_directory":"([^"]*)".*/\1/')"
targets=(aarch64-apple-ios aarch64-apple-ios-sim)
# The C in the dependencies is compiled for the app's deployment target
# (ios/project.yml), not for the SDK's.
export IPHONEOS_DEPLOYMENT_TARGET=26.1

for target in "${targets[@]}"; do
  cargo build -p orb-mobile --lib "${cargo_profile[@]+"${cargo_profile[@]}"}" --target "$target"
done

kit="$root/ios/OrbKit"
generated="$kit/Sources/OrbKit/Generated"
headers="$(mktemp -d)"
trap 'rm -rf "$headers"' EXIT

# The bindings, read from the library's own metadata.
rm -rf "$generated"
mkdir -p "$generated"
cargo run -q -p orb-mobile --features bindgen --bin uniffi-bindgen -- \
  generate --library "$target_dir/aarch64-apple-ios-sim/$profile/liborb_mobile.a" \
  --language swift --out-dir "$headers"
mv "$headers"/*.swift "$generated/"

# One module map for every crate's C header, as an XCFramework takes one.
cat "$headers"/*.modulemap > "$headers/module.modulemap"
rm "$headers"/*FFI.modulemap

rm -rf "$kit/OrbFFI.xcframework"
xcodebuild -create-xcframework \
  -library "$target_dir/aarch64-apple-ios/$profile/liborb_mobile.a" -headers "$headers" \
  -library "$target_dir/aarch64-apple-ios-sim/$profile/liborb_mobile.a" -headers "$headers" \
  -output "$kit/OrbFFI.xcframework" >/dev/null

echo "ios/OrbKit: OrbFFI.xcframework ($profile) and $(ls "$generated" | wc -l | tr -d ' ') Swift files"
