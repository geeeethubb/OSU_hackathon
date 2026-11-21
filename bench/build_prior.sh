#!/usr/bin/env bash
# build_prior.sh [TAG]
#
# bench/BEST_SHA names the best commit so far. When it exists and is not HEAD,
# this builds that commit's root Dockerfile as TAG (default ss-prior:latest) so
# bench-20m can time it against HEAD on the same machine and the same inputs.
#
# Fails closed: when bench/BEST_SHA names a commit that cannot be found, has
# no root Dockerfile or does not build, the step fails and with it the job,
# because a green run without the prior-versus-candidate pairs looks like
# evidence and is not (a mistyped hash once went unnoticed that way). Only a
# missing or empty file, or a baseline that is HEAD itself, means "no
# comparison" without an error. Writes status=, sha= and image= to
# $GITHUB_OUTPUT. status is one of: absent, empty, unknown-commit,
# same-as-head, no-dockerfile, build-failed, built.
set -uo pipefail

TAG=${1:-ss-prior:latest}
OUTPUT=${GITHUB_OUTPUT:-/dev/stdout}
emit() { echo "$1=$2" >> "$OUTPUT"; echo "prior: $1=$2"; }

file=bench/BEST_SHA
if [ ! -f "$file" ]; then emit status absent; exit 0; fi
# First word of the first line that is not blank or a # comment.
want=$(sed -e 's/#.*//' "$file" | awk 'NF { print $1; exit }')
if [ -z "$want" ]; then emit status empty; exit 0; fi

head=$(git rev-parse HEAD)
if ! git cat-file -e "$want^{commit}" 2>/dev/null; then
  git fetch --no-tags --quiet origin "$want" 2>/dev/null \
    || git fetch --no-tags --quiet origin '+refs/heads/*:refs/remotes/origin/*' 2>/dev/null || true
fi
if ! full=$(git rev-parse --verify --quiet "$want^{commit}"); then
  echo "::error::bench/BEST_SHA names '$want', which is not a commit this checkout can find"
  emit sha "$want"; emit status unknown-commit; exit 1
fi
emit sha "$full"
if [ "$full" = "$head" ]; then emit status same-as-head; exit 0; fi

tree=$(mktemp -d)
if ! git worktree add --detach "$tree/src" "$full" >/dev/null 2>&1; then
  echo "::error::could not check out $full"
  emit status unknown-commit; exit 1
fi
if [ ! -f "$tree/src/Dockerfile" ]; then
  echo "::error::bench/BEST_SHA commit $full has no root Dockerfile"
  emit status no-dockerfile; exit 1
fi
if docker buildx build --platform linux/amd64 --load -t "$TAG" "$tree/src"; then
  emit image "$TAG"; emit status built
else
  echo "::error::the image of bench/BEST_SHA commit $full did not build"
  emit status build-failed
  exit 1
fi
