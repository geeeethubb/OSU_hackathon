from bisect import bisect_left, bisect_right
from collections import Counter, defaultdict
from itertools import groupby
from random import Random

GAP = 1_800_000
DAY = 86_400_000
WEEK = 7 * DAY

# Inputs for ONE canonical person, after event deduplication and validation.
# An event is (millisecond, category or None, purchase cents).
def reference_sessions(events):
    sessions = []
    for t, category, cents in sorted(events, key=lambda row: row[0]):
        if not sessions or t - sessions[-1][1] > GAP:
            sessions.append([t, t, 0, 0, Counter()])
        current = sessions[-1]
        current[1] = t
        current[2] += 1
        current[3] += cents
        if category is not None:
            current[4][category] += cents
    return sessions

def bucket_sessions(events):
    buckets = {}
    for t, category, cents in events:
        b = t // GAP
        if b not in buckets:
            buckets[b] = [t, t, 0, 0, Counter()]
        item = buckets[b]
        item[0] = min(item[0], t)
        item[1] = max(item[1], t)
        item[2] += 1
        item[3] += cents
        if category is not None:
            item[4][category] += cents
    out = []
    for b in sorted(buckets):
        lo, hi, count, revenue, cats = buckets[b]
        if not out or lo - out[-1][1] > GAP:
            out.append([lo, hi, count, revenue, cats.copy()])
        else:
            out[-1][1] = hi
            out[-1][2] += count
            out[-1][3] += revenue
            out[-1][4].update(cats)
    return out

# A valid click is (millisecond, click-ID bytes, campaign).
def reference_ads(starts, clicks):
    unique = {cid: (t, cid, campaign) for t, cid, campaign in clicks}
    credit, assists = [], Counter()
    for s in starts:
        hits = [r for r in unique.values() if s - WEEK <= r[0] <= s]
        credit.append(min(hits, key=lambda r: (-r[0], r[1]))[2] if hits else "organic")
        for campaign in {r[2] for r in hits}:
            assists[campaign, s // DAY] += 1
    return credit, assists

def compact_ads(starts, clicks):
    credit = []
    # Smallest byte-wise ID wins equal click times; duplicates are harmless.
    timeline = [(t, min(group, key=lambda r: r[1])[2])
                for t, group in groupby(sorted(clicks), key=lambda r: r[0])]
    j = 0
    for s in starts:
        while j < len(timeline) and timeline[j][0] <= s:
            j += 1
        credit.append(timeline[j-1][1] if j and s - timeline[j-1][0] <= WEEK
                      else "organic")

    by_day, by_campaign = defaultdict(list), defaultdict(list)
    for s in starts:
        by_day[s // DAY].append(s)
    for t, _, campaign in clicks:
        by_campaign[campaign].append(t)
    assists = Counter()
    for campaign, times in by_campaign.items():
        spans = []
        for t in sorted(times):
            if spans and t <= spans[-1][1]:
                spans[-1][1] = max(spans[-1][1], t + WEEK)
            else:
                spans.append([t, t + WEEK])
        for lo, hi in spans:
            for day in range(lo // DAY, hi // DAY + 1):
                v = by_day.get(day, ())
                n = bisect_right(v, hi) - bisect_left(v, lo)
                if n:
                    assists[campaign, day] += n
    return credit, assists

def check(events, clicks):
    sessions = reference_sessions(events)
    assert sessions == bucket_sessions(events)
    starts = [s[0] for s in sessions]
    assert reference_ads(starts, clicks) == compact_ads(starts, clicks)

def test():
    check([], [])
    for gap, expected in [(GAP-1, 1), (GAP, 1), (GAP+1, 2)]:
        events = [(DAY-1, None, 0), (DAY-1+gap, "x", 1)]
        assert len(bucket_sessions(events)) == expected
        check(events, [])
    for age, expected in [(WEEK-1, "X"), (WEEK, "X"), (WEEK+1, "organic"),
                          (0, "X"), (-1, "organic")]:
        t = 10 * DAY
        clicks = [(t-age, b'a', "X")]
        assert compact_ads([t], clicks)[0] == [expected]
        check([(t, None, 0)], clicks)
    tie = [(0, b'z', "A"), (0, b'B', "B"), (0, b'B', "B")]
    assert compact_ads([0], tie) == (["B"], Counter({("A", 0): 1, ("B", 0): 1}))
    check([(0, "unknown", 3), (0, "A", 3)], tie)
    for seed in range(1000):
        rng = Random(seed)
        t = rng.choice([0, DAY-1, GAP-1])
        events = []
        for _ in range(rng.randrange(1, 100)):
            t += rng.choice([0, 1, GAP-1, GAP, GAP+1, DAY, rng.randrange(2*GAP)])
            category = rng.choice([None, None, None, "A", "a", "unknown"])
            events.append((t, category, rng.randrange(1, 11) if category else 0))
        rng.shuffle(events)
        clicks = []
        for i in range(rng.randrange(65)):
            t = rng.choice(events)[0] + rng.choice([0, 1, -1, -WEEK, -WEEK-1, -WEEK+1])
            click = (t, str(i).encode(), rng.choice(["A", "B", "a", "b"]))
            clicks.extend([click] * rng.randrange(1, 4))
        rng.shuffle(clicks)
        check(events, clicks)
    print("PASS: fixed boundary checks and 1,000 seeded differential cases")

if __name__ == "__main__":
    test()
