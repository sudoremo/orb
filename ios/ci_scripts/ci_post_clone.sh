#!/bin/zsh
# Xcode Cloud runs this after cloning, before it resolves packages and builds.
# The checkout carries neither the Xcode project nor OrbKit's binary, both of
# which are generated (ios/.gitignore), and the build machine has no Rust; this
# puts all of it in place. See "Releasing" in README.md for the workflow setup.
#
# Expects the environment variable ORB_DEVELOPMENT_TEAM, set on the workflow in
# App Store Connect, so the team stays out of the repository like everybody's
# Local.xcconfig does.
set -euo pipefail

cd "$CI_PRIMARY_REPOSITORY_PATH"

# A tag that does not say the app's version is a mistake, caught before the
# build (release.yml does the same for the Mac).
if [[ -n "${CI_TAG:-}" ]]; then
  version=$(sed -nE 's/^ *MARKETING_VERSION: "(.*)"/\1/p' ios/project.yml | head -n1)
  if [[ "v$version" != "$CI_TAG" ]]; then
    echo "tag $CI_TAG, but ios/project.yml says $version" >&2
    exit 1
  fi
fi

if ! command -v cargo >/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --profile minimal --no-modify-path
fi
source "$HOME/.cargo/env"
rustup target add aarch64-apple-ios aarch64-apple-ios-sim

command -v xcodegen >/dev/null || brew install xcodegen

if [[ -n "${ORB_DEVELOPMENT_TEAM:-}" && ! -e ios/Local.xcconfig ]]; then
  printf 'DEVELOPMENT_TEAM = %s\n' "$ORB_DEVELOPMENT_TEAM" > ios/Local.xcconfig
fi

scripts/build-xcframework.sh
(cd ios && xcodegen generate --quiet)
