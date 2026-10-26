#!/bin/sh
# Print the CHANGELOG.md section of one version, without its heading:
#   contrib/release-notes.sh 0.2.0 [CHANGELOG.md]
# Used by `just release-notes` and the release workflow (the GitHub Release
# notes). Fails if the version has no `## [X.Y.Z] - date` section.
set -eu

version=${1:?usage: release-notes.sh VERSION [CHANGELOG]}
version=${version#v}
changelog=${2:-CHANGELOG.md}

# $(…) drops trailing blank lines; sed drops leading ones.
notes=$(awk -v want="## [$version] - " '
    index($0, "## [") == 1 { if (found) exit; if (index($0, want) == 1) { found = 1; next } }
    found { print }
    END { if (!found) exit 1 }
' "$changelog") || {
    echo "error: no \"## [$version] - <date>\" section in $changelog" >&2
    exit 1
}
printf '%s\n' "$notes" | sed -e '/./,$!d'
