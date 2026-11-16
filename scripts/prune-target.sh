#!/usr/bin/env bash
# Keep target/ bounded. Cargo never deletes build output it no longer uses,
# and in this project two kinds of it grow without end:
#
# - the incremental caches, one of ~800 MB per crate for every feature set,
#   build target and checkout path (desktop, mobile, uniffi, each worktree);
#   they are a pure cache, and filled the disk twice;
# - test binaries replaced by a rebuild, each with its debug info.
#
# Run by start.sh and test-ios.sh, and first in the pre-commit checklist.
#
#   scripts/prune-target.sh             prune
#   ORB_INCREMENTAL_LIMIT_MB=2048 ...   a different cap for the caches

set -euo pipefail

target="$(cd "$(dirname "$0")/.." && pwd)/target"
limit_mb=${ORB_INCREMENTAL_LIMIT_MB:-4096}
[[ -d "$target" ]] || exit 0

size_mb() { du -sm "$1" 2>/dev/null | cut -f1; }

# Incremental caches: those unused for two days go; if the rest is still over
# the cap, all of it goes. The next build of a crate is then a full one.
for dir in "$target"/*/incremental; do
  [[ -d "$dir" ]] || continue
  find "$dir" -mindepth 1 -maxdepth 1 -type d -mtime +2 -exec rm -rf {} +
  if (( $(size_mb "$dir") > limit_mb )); then
    echo "prune-target: ${dir#"$target"/} over ${limit_mb} MB, removed" >&2
    rm -rf "$dir"
  fi
done

# Test binaries: the two newest of each name stay (a crate is tested with two
# feature sets), the older ones go with their debug info and dep-info.
for deps in "$target"/*/deps; do
  [[ -d "$deps" ]] || continue
  find "$deps" -maxdepth 1 -type f -perm +111 ! -name '*.*' -print0 |
    xargs -0 stat -f '%m %N' |
    sed -E 's/^([0-9]+) (.*)-([0-9a-f]{16})$/\2 \1 \2-\3/' |
    sort -k1,1 -k2,2nr |
    awk '{ if ($1 == name) n++; else { name = $1; n = 1 } if (n > 2) print $3 }' |
    while IFS= read -r stale; do
      rm -rf "$stale" "$stale.dSYM" "$stale.d"
    done
done
