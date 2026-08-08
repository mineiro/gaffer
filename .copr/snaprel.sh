#!/bin/sh
# Print the RPM Release for a snapshot build: 0.<timestamp>.git<short sha>
#
# Derived from HEAD's *commit* timestamp rather than a commit count, which was
# the obvious choice but is wrong here: COPR clones with `--depth 500`, so
# `git rev-list --count` silently plateaus at 500 once the project passes that
# many commits, and the release would stop incrementing — reintroducing exactly
# the "repository looks frozen" bug this exists to prevent. A commit timestamp
# needs no history at all, so it survives any clone depth.
#
# Monotonic because `main` enforces linear history; with merge commits, HEAD's
# committer date could in principle go backwards.
set -eu

if ! sha=$(git rev-parse --short=7 HEAD 2>/dev/null); then
    # Not a git checkout (a release tarball, say). Fall back to a plain 1 so
    # the build still succeeds and sorts as a release rather than a snapshot.
    echo 1
    exit 0
fi

# A tagged release builds as a plain `1`, so it sorts *above* every snapshot
# that preceded it — which is the whole reason the snapshot form carries a
# leading zero. Without this the tag changed nothing about what COPR produced,
# and the ordering property the spec describes had nothing to order against.
#
# `--exact-match` deliberately: only the commit the tag points at is a release.
# The commit after it is a snapshot again, and `git describe` without this flag
# would happily call it `v0.2.0-1-gabc123` and ship it as a release.
#
# If tags are missing — COPR clones shallow, and a tarball has no git at all —
# this falls through to a snapshot. That is the safe direction to fail: a
# release mistakenly built as a snapshot is superseded by rebuilding it, while
# a snapshot mistakenly built as `1` outranks the real release for good.
if tag=$(git describe --exact-match --tags HEAD 2>/dev/null); then
    case "$tag" in
    v*)
        echo 1
        exit 0
        ;;
    esac
fi

stamp=$(git log -1 --format=%cd --date=format:%Y%m%d%H%M%S HEAD)
echo "0.${stamp}.git${sha}"
