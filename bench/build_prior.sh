#!/usr/bin/env bash
# build_prior.sh [TAG]
#
# bench/BEST_SHA names the best commit so far. When it exists and is not HEAD,
# this builds that commit's root Dockerfile as TAG (default ss-prior:latest) so
# bench-20m can time it against HEAD on the same machine and the same inputs.
#
# Never fails the job: a missing or unbuildable prior only means there is no
# comparison, which the report says. Writes status=, sha= and image= to
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
  echo "::warning::bench/BEST_SHA names '$want', which is not a commit this checkout can find"
  emit sha "$want"; emit status unknown-commit; exit 0
fi
emit sha "$full"
if [ "$full" = "$head" ]; then emit status same-as-head; exit 0; fi

tree=$(mktemp -d)
if ! git worktree add --detach "$tree/src" "$full" >/dev/null 2>&1; then
  echo "::warning::could not check out $full"
  emit status unknown-commit; exit 0
fi
if [ ! -f "$tree/src/Dockerfile" ]; then
  echo "::warning::bench/BEST_SHA commit $full has no root Dockerfile"
  emit status no-dockerfile; exit 0
fi
if docker buildx build --platform linux/amd64 --load -t "$TAG" "$tree/src"; then
  emit image "$TAG"; emit status built
else
  echo "::warning::the image of bench/BEST_SHA commit $full did not build"
  emit status build-failed
fi
