#!/bin/sh
# Print the CHANGELOG.md section for one version, for use as release notes.
#
#     .github/release-notes.sh 0.2.0
#
# A script rather than a step buried in YAML so that a release can be rehearsed
# locally — the failure mode being guarded against is discovering at tag time
# that the notes come out empty, when the tag has already been pushed.
#
# Exits non-zero if the version has no section, which is the point: a release
# with no notes is a mistake, not something to paper over with a blank body.
set -eu

if [ $# -ne 1 ]; then
    echo "usage: $0 <version>   e.g. $0 0.2.0" >&2
    exit 2
fi

version=$1
changelog=$(dirname "$0")/../CHANGELOG.md

if [ ! -f "$changelog" ]; then
    echo "no CHANGELOG.md next to $0" >&2
    exit 1
fi

# Everything between this version's heading and the next `## ` heading. The
# heading itself is dropped: GitHub already shows the tag above the body.
notes=$(awk -v want="$version" '
    # Match "## [0.2.0] — date" or "## 0.2.0", tolerating either bracket style
    # so a hand-written heading does not silently produce empty notes.
    /^## / {
        line = $0
        gsub(/^## +\[?/, "", line)
        gsub(/\].*$/, "", line)
        gsub(/ .*$/, "", line)
        found = (line == want)
        next
    }
    found { print }
' "$changelog" |
    # Drop blank lines at the start and end while keeping the ones between
    # sections: a blank line before `### Fixed` is what makes it a heading
    # rather than part of the preceding list.
    awk 'NF { while (pending-- > 0) print ""; pending = 0; print; seen = 1; next }
         seen { pending++ }')

if [ -z "$(printf '%s' "$notes" | tr -d '[:space:]')" ]; then
    echo "CHANGELOG.md has no entry for $version" >&2
    exit 1
fi

printf '%s\n' "$notes"
