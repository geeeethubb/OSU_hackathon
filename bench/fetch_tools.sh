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
# The archives as republished on the track page on 2026-11-22 (about 01:30 UTC;
# the first ones, of 2026-11-21, were 13724cad... and 7eb8e1f0...). A different hash
# means the organizers republished: read the track page before trusting old numbers.
KNOWN_GENERATOR=78f973f75d14b48e792628284f19c75de71fa597d20952e16a9ced93c78bee4a
KNOWN_ANSWER=c2439e0e2e7456d721852814e9928e077648803d5c57a1760a40c7d3b0864d20

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
