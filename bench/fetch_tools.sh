#!/usr/bin/env bash
# fetch_tools.sh DIR
#
# Puts the organizers' generator (built) and the published dev answer in DIR:
#
#   DIR/generate                         the generator binary
#   DIR/dev-answer/user_daily.csv        the answer for `--seed 1 --events 1000000 --clicks 250000`
#   DIR/dev-answer/campaign_daily.csv
#   DIR/SOURCES.txt                      where they came from, and the archives' SHA-256
#
# DIR is what CI caches, so a run only needs the track's site when the cache is
# empty. Nothing here is ever written into the repository: the generator source
# and the answers are the organizers' to publish, not ours.
set -euo pipefail

DIR=${1:?usage: fetch_tools.sh DIR}
BASE=${SS_FILES_URL:-https://www.joinbuilders.org/buildathon/tracks/shopping-sessions/files}
# The archives as first fetched for this harness (2026-11-21). A different hash
# means the organizers republished: read the track page before trusting old numbers.
KNOWN_GENERATOR=13724cadbac5a99d847e27cea911cd2e338ff972b46c2039da87452011bb101e
KNOWN_ANSWER=7eb8e1f0b1f431d178d28ac9437ce85809c44eb6393fdd42fb59857f30834fa9

mkdir -p "$DIR"
if [ -x "$DIR/generate" ] && [ -s "$DIR/dev-answer/user_daily.csv" ] && [ -s "$DIR/dev-answer/campaign_daily.csv" ]; then
  echo "tools: already in $DIR"
  cat "$DIR/SOURCES.txt" 2>/dev/null || true
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for name in generator.zip dev-answer.zip; do
  echo "tools: fetching $BASE/$name"
  curl -fsSL --retry 5 --retry-delay 3 --retry-all-errors -A "Mozilla/5.0" -o "$tmp/$name" "$BASE/$name"
done
generator_hash=$(sha256sum "$tmp/generator.zip" | cut -d' ' -f1)
answer_hash=$(sha256sum "$tmp/dev-answer.zip" | cut -d' ' -f1)
[ "$generator_hash" = "$KNOWN_GENERATOR" ] || echo "::warning::generator.zip changed on the track page (sha256 $generator_hash, expected $KNOWN_GENERATOR)"
[ "$answer_hash" = "$KNOWN_ANSWER" ] || echo "::warning::dev-answer.zip changed on the track page (sha256 $answer_hash, expected $KNOWN_ANSWER)"

(cd "$tmp" && unzip -q generator.zip && unzip -q dev-answer.zip)
(cd "$tmp/generator" && cargo build --release)

rm -rf "$DIR/generate" "$DIR/dev-answer"
mkdir -p "$DIR/dev-answer"
cp "$tmp/generator/target/release/generate" "$DIR/generate"
cp "$tmp/dev-answer/user_daily.csv" "$tmp/dev-answer/campaign_daily.csv" "$DIR/dev-answer/"
{
  echo "source: $BASE"
  echo "fetched: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "generator.zip sha256: $generator_hash"
  echo "dev-answer.zip sha256: $answer_hash"
  echo "built with: $(rustc --version)"
} > "$DIR/SOURCES.txt"
cat "$DIR/SOURCES.txt"
