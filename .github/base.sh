#!/usr/bin/env bash
# What a mutation run is a change *against*, printed as the arguments `git diff` needs for it.
#
# **Two events reach this and each has a different base.** A pull request's is the merge base with what it
# merges into; a push's is the commit that was there before it, which GitHub gives as `github.event.before`
# and which is all zeroes for a branch's first push. A dispatch never gets here: it names its files instead,
# because the code it exists to answer is in no change.
#
# Written as a script rather than inline so the two jobs cannot drift: they mutate different platforms and
# have to mutate the same change.
set -euo pipefail

case "${GITHUB_EVENT_NAME:-}" in
  pull_request)
    # `--merge-base` against the first parent: the base the merge commit merges into
    echo "--merge-base HEAD^1"
    ;;
  push)
    before="$(jq -r '.before // ""' "$GITHUB_EVENT_PATH")"
    # All zeroes is a branch's first push, and a force-push can name a commit this clone does not hold;
    # either way the previous commit is the next best subject
    if [ -z "$before" ] || [ "$before" = "0000000000000000000000000000000000000000" ] ||
      ! git cat-file -e "$before^{commit}" 2>/dev/null; then
      echo "HEAD^1"
    else
      echo "$before"
    fi
    ;;
  *)
    echo "base.sh: no base for $GITHUB_EVENT_NAME" >&2
    exit 1
    ;;
esac
