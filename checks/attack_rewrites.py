#!/usr/bin/env python3
"""
attack_rewrites.py: tries to break the four rewrites the optimized engine relies on.

The rewrites, modelled here exactly as the engine is meant to implement them:
  1. Sessions without sorting events. Per (person, 30-minute bucket), with the
     bucket index (ts - window_start) // 1,800,000 (NOT epoch-aligned), keep the
     min ts, max ts, event count and purchase sums. Walk the person's occupied
     buckets in order and continue the session only when the buckets are
     adjacent and next.min - previous.max <= 1,800,000.
  2. Credit by predecessor: the person's latest valid click at or before the
     session start, if it is within 7 days; a same-millisecond tie goes to the
     byte-wise smallest click_id.
  3. Assists by coverage: per (person, campaign) merge the closed intervals
     [t, t + 7 days] and count the session starts inside each merged interval,
     on each start's own UTC day.
  4. No click dedup at all.

They are compared with the rules applied literally (sort, then look at every
session/click pair), on:
  - named adversarial cases (bucket edges, exact gaps, touching intervals, ...);
  - EVERY input of a small universe, with the time constants scaled down, for
    every grid offset (exhaustive, so no generator can miss a corner);
  - adversarial random inputs with the real constants: timestamps on bucket
    edges, windows that do not start at midnight, clicks exactly 7 days apart,
    heavy people with hundreds of clicks.
Then every "slip" in SLIPS (a plausible wrong variant of a rewrite) is run
through the same inputs: each must be caught, and its smallest counterexample
is printed. The variants marked equivalent must survive.

Usage:
  python checks/attack_rewrites.py
        the kernel attack described above (about a minute)
  python checks/attack_rewrites.py --fixtures [DIR ...]
        the rewrites as a whole pipeline against the fixtures' expected tables
        (default: fixtures/ and extra_fixtures/), then which fixtures catch each slip
  python checks/attack_rewrites.py --data DIR [DIR ...] [--slips]
        the rewrites as a whole pipeline against checks/pyref.py on datasets;
        with --slips, also which slips each dataset would catch

Pure standard library. The pipeline modes reuse pyref's readers for links and
events (those stages are not what is under attack) and read clicks themselves,
without dedup.
"""
import bisect
import collections
import itertools
import os
import random
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
sys.path.insert(0, HERE)
import pyref  # noqa: E402
import pyref_fixtures  # noqa: E402

ORGANIC = "organic"


class Units:
    """The time constants. Scaled down for the exhaustive universes."""

    def __init__(self, gap, day, week):
        self.gap, self.day, self.week = gap, day, week


REAL = Units(1_800_000, 86_400_000, 604_800_000)

# --------------------------------------------------------------------------
# The rules, literally
# --------------------------------------------------------------------------
# An event is (ts, category or None, cents): a purchase has a category and
# cents >= 1, anything else has None and 0. A click is (ts, click_id, campaign).


def rules_sessions(events, units):
    """[(start, events, revenue, {category: cents})], by sorting and comparing neighbours."""
    sessions = []
    previous = None
    for ts, category, cents in sorted(events, key=lambda event: event[0]):
        if previous is None or ts - previous > units.gap:
            sessions.append([ts, 0, 0, {}])
        session = sessions[-1]
        session[1] += 1
        session[2] += cents
        if category is not None:
            session[3][category] = session[3].get(category, 0) + cents
        previous = ts
    return [tuple(session) for session in sessions]


def rules_ads(starts, clicks, units):
    """(credited campaign per start, {(campaign, start day): assists}), every pair checked."""
    unique = {}
    for ts, click_id, campaign in clicks:
        unique.setdefault(click_id, (ts, click_id, campaign))     # dedup on click_id
    credits, assists = [], collections.Counter()
    for start in starts:
        hits = [click for click in unique.values() if click[0] <= start and start - click[0] <= units.week]
        if hits:
            latest = max(click[0] for click in hits)
            credits.append(min((click for click in hits if click[0] == latest), key=lambda c: c[1])[2])
        else:
            credits.append(ORGANIC)
        for campaign in {click[2] for click in hits}:
            assists[campaign, start // units.day] += 1
    return credits, assists


# --------------------------------------------------------------------------
# The engine's rewrites, and their slips
# --------------------------------------------------------------------------

SLIPS = {
    # name: (equivalent to the rewrite?, what the slip is)
    "gap-only": (True, "sessions: continue on the gap test alone, ignoring adjacency"),
    "epoch-grid": (True, "sessions: buckets at ts // G instead of (ts - window_start) // G"),
    "merge-plus-1": (True, "assists: merge intervals when next.t <= end + 1 ms"),
    "split-at-G": (False, "sessions: a gap of exactly G starts a new session"),
    "adjacent-only": (False, "sessions: continue whenever the buckets are adjacent"),
    "gap-from-bucket-min": (False, "sessions: gap measured from the previous bucket's min"),
    "gap-from-session-start": (False, "sessions: gap measured from the session start"),
    "first-last-as-min-max": (False, "sessions: a bucket's first and last read ts used as its min and max"),
    "bucket-2G": (False, "sessions: buckets 60 minutes wide"),
    "break-at-midnight": (False, "sessions: a new session whenever the UTC day changes"),
    "stale-previous-max": (False, "sessions: previous.max is not advanced when a bucket joins the session"),
    "tie-largest-id": (False, "credit: a same-millisecond tie goes to the largest click_id"),
    "tie-first-read": (False, "credit: a same-millisecond tie goes to the click read first"),
    "tie-smallest-campaign": (False, "credit: a same-millisecond tie goes to the smallest campaign_id"),
    "week-exclusive": (False, "credit: a click exactly 7 days old does not count"),
    "start-exclusive": (False, "credit: a click exactly at the session start does not count"),
    "merge-strict": (False, "assists: intervals that only touch (next.t == end) are not merged"),
    "never-merge": (False, "assists: intervals are never merged (one assist per click)"),
    "merge-plus-2": (False, "assists: merge intervals when next.t <= end + 2 ms"),
    "interval-end-exclusive": (False, "assists: [t, t + 7 days) instead of [t, t + 7 days]"),
    "interval-start-exclusive": (False, "assists: (t, t + 7 days] instead of [t, t + 7 days]"),
    "tie-loser-dropped": (False, "assists: only the click that wins its millisecond keeps its interval"),
    "assist-credited-only": (False, "assists: only the credited campaign assists"),
    "assist-on-click-day": (False, "assists: counted on the UTC day of the interval's first click"),
    "merge-across-campaigns": (False, "assists: intervals merged per person, not per (person, campaign)"),
    "skip-last-covered-day": (False, "assists: starts on the interval's last UTC day are not counted"),
    "skip-first-covered-day": (False, "assists: starts on the interval's first UTC day are not counted"),
    "assist-once-per-person-day": (False, "assists: a campaign counted once per person and day, not per session"),
    "window-relative-day": (False, "days: 24 h slices counted from window_start instead of UTC dates"),
    # Aggregation slips (pipeline modes only).
    "category-of-session-winners": (False, "top category: the best of the sessions' own winners"),
    "purchases-on-event-day": (False, "revenue and categories counted on each event's own day"),
}
KERNEL_SLIPS = [name for name in SLIPS if name not in ("category-of-session-winners", "purchases-on-event-day")]
SESSION_SLIPS = {"gap-only", "epoch-grid", "split-at-G", "adjacent-only", "gap-from-bucket-min",
                 "gap-from-session-start", "first-last-as-min-max", "bucket-2G", "break-at-midnight",
                 "stale-previous-max"}


def engine_sessions(events, window_start, units, slip=None):
    """Rewrite 1. Same result shape as rules_sessions."""
    width = 2 * units.gap if slip == "bucket-2G" else units.gap
    origin = 0 if slip == "epoch-grid" else window_start
    buckets = {}                 # index -> [min ts, max ts, events, revenue, {category: cents}]
    for ts, category, cents in events:
        index = (ts - origin) // width
        bucket = buckets.get(index)
        if bucket is None:
            bucket = buckets[index] = [ts, ts, 0, 0, {}]
        elif slip == "first-last-as-min-max":
            bucket[1] = ts
        else:
            bucket[0] = min(bucket[0], ts)
            bucket[1] = max(bucket[1], ts)
        bucket[2] += 1
        bucket[3] += cents
        if category is not None:
            bucket[4][category] = bucket[4].get(category, 0) + cents

    sessions = []
    previous_index = previous_min = previous_max = None
    for index in sorted(buckets):
        low, high, count, revenue, categories = buckets[index]
        if previous_index is None:
            continues = False
        else:
            adjacent = index == previous_index + 1
            within = low - previous_max <= units.gap
            if slip == "gap-only":
                continues = within
            elif slip == "adjacent-only":
                continues = adjacent
            elif slip == "split-at-G":
                continues = adjacent and low - previous_max < units.gap
            elif slip == "gap-from-bucket-min":
                continues = adjacent and low - previous_min <= units.gap
            elif slip == "gap-from-session-start":
                continues = adjacent and low - sessions[-1][0] <= units.gap
            elif slip == "break-at-midnight":
                continues = adjacent and within and low // units.day == previous_max // units.day
            else:
                continues = adjacent and within
        if continues:
            session = sessions[-1]
            session[1] += count
            session[2] += revenue
            for category, cents in categories.items():
                session[3][category] = session[3].get(category, 0) + cents
        else:
            sessions.append([low, count, revenue, dict(categories)])
        previous_index, previous_min = index, low
        if not (slip == "stale-previous-max" and continues):
            previous_max = high
    return [tuple(session) for session in sessions]


def engine_credit(starts, clicks, units, slip=None):
    """Rewrite 2 (and 4: copies of a click are simply there twice). starts must ascend."""
    winner_at = {}               # ts -> (click_id, campaign) of the click that wins that millisecond
    for ts, click_id, campaign in clicks:
        current = winner_at.get(ts)
        if current is None:
            replace = True
        elif slip == "tie-largest-id":
            replace = click_id > current[0]
        elif slip == "tie-first-read":
            replace = False
        elif slip == "tie-smallest-campaign":
            replace = campaign < current[1]
        else:
            replace = click_id < current[0]
        if replace:
            winner_at[ts] = (click_id, campaign)
    timeline = sorted(winner_at)
    credits = []
    position = 0
    for start in starts:
        if slip == "start-exclusive":
            while position < len(timeline) and timeline[position] < start:
                position += 1
        else:
            while position < len(timeline) and timeline[position] <= start:
                position += 1
        if position == 0:
            credits.append(ORGANIC)
            continue
        age = start - timeline[position - 1]
        counts = age < units.week if slip == "week-exclusive" else age <= units.week
        credits.append(winner_at[timeline[position - 1]][1] if counts else ORGANIC)
    return credits, winner_at


def engine_assists(starts, clicks, credits, winner_at, window_start, units, slip=None):
    """Rewrite 3 (and 4). starts must ascend. {(campaign, UTC day of the start): assists}."""
    times_of = collections.defaultdict(list)
    for ts, click_id, campaign in clicks:
        if slip == "tie-loser-dropped" and winner_at[ts][0] != click_id:
            continue
        times_of[campaign].append(ts)
    assists = collections.Counter()
    if slip == "merge-across-campaigns":
        # One set of merged intervals for the whole person; every campaign with a click in
        # a merged interval is given all of it.
        merged = []              # [low, high, campaigns]
        for ts, campaign in sorted((ts, campaign) for campaign, times in times_of.items() for ts in times):
            if merged and ts <= merged[-1][1]:
                merged[-1][1] = ts + units.week
                merged[-1][2].add(campaign)
            else:
                merged.append([ts, ts + units.week, {campaign}])
        for low, high, campaigns in merged:
            for start in starts[bisect.bisect_left(starts, low):bisect.bisect_right(starts, high)]:
                for campaign in campaigns:
                    assists[campaign, start // units.day] += 1
        return assists
    counted = set()
    for campaign, times in times_of.items():
        times.sort()
        intervals = []           # disjoint closed [low, high], ascending
        for ts in times:
            if not intervals or slip == "never-merge":
                merge = False
            elif slip == "merge-strict":
                merge = ts < intervals[-1][1]
            elif slip == "merge-plus-1":
                merge = ts <= intervals[-1][1] + 1
            elif slip == "merge-plus-2":
                merge = ts <= intervals[-1][1] + 2
            else:
                merge = ts <= intervals[-1][1]
            if merge:
                intervals[-1][1] = ts + units.week
            else:
                intervals.append([ts, ts + units.week])
        for low, high in intervals:
            first = bisect.bisect_right(starts, low) if slip == "interval-start-exclusive" \
                else bisect.bisect_left(starts, low)
            last = bisect.bisect_left(starts, high) if slip == "interval-end-exclusive" \
                else bisect.bisect_right(starts, high)
            for position in range(first, last):
                start = starts[position]
                if slip == "assist-credited-only" and credits[position] != campaign:
                    continue
                if slip == "skip-last-covered-day" and start // units.day == high // units.day:
                    continue
                if slip == "skip-first-covered-day" and start // units.day == low // units.day:
                    continue
                if slip == "assist-once-per-person-day":
                    if (campaign, start // units.day) in counted:
                        continue
                    counted.add((campaign, start // units.day))
                if slip == "assist-on-click-day":
                    day = low // units.day
                elif slip == "window-relative-day":
                    day = window_start // units.day + (start - window_start) // units.day
                else:
                    day = start // units.day
                assists[campaign, day] += 1
    return assists


def engine_ads(starts, clicks, window_start, units, slip=None):
    credits, winner_at = engine_credit(starts, clicks, units, slip)
    return credits, engine_assists(starts, clicks, credits, winner_at, window_start, units, slip)


def agree(events, clicks, window_start, units, slip=None):
    """None if the rewrites give the rules' answer on this person, else a description."""
    expected = rules_sessions(events, units)
    actual = engine_sessions(events, window_start, units, slip)
    if expected != actual:
        return f"sessions: rules {expected} engine {actual}"
    starts = [session[0] for session in expected]
    expected_ads = rules_ads(starts, clicks, units)
    actual_ads = engine_ads(starts, clicks, window_start, units, slip)
    if expected_ads[0] != actual_ads[0]:
        return f"credit for starts {starts}: rules {expected_ads[0]} engine {actual_ads[0]}"
    if expected_ads[1] != actual_ads[1]:
        return f"assists for starts {starts}: rules {dict(expected_ads[1])} engine {dict(actual_ads[1])}"
    return None


# --------------------------------------------------------------------------
# Named adversarial cases (real constants)
# --------------------------------------------------------------------------

G, DAY, WEEK = REAL.gap, REAL.day, REAL.week
MIDNIGHT = 1790812800000            # 2026-10-01T00:00:00Z
OFFSETS = [0, 1, G - 1, 26_000_000, 123_457, DAY - 1]     # window_start = MIDNIGHT + offset
WINDOW = 14 * DAY


def view(ts):
    return (ts, None, 0)


def named_cases():
    """(name, window_start, events, clicks, expected session count or None)."""
    cases = []
    for offset in OFFSETS:
        w = MIDNIGHT + offset
        edge = w + 20 * G            # a bucket edge on this grid
        tag = f"offset {offset}"
        cases += [
            (f"exactly G, bucket start to bucket start ({tag})", w, [view(edge), view(edge + G)], [], 1),
            (f"exactly G, bucket end to bucket end ({tag})", w, [view(edge - 1), view(edge + G - 1)], [], 1),
            (f"adjacent buckets, gap 2G-1 ({tag})", w, [view(edge), view(edge + 2 * G - 1)], [], 2),
            (f"skipped bucket, the smallest gap G+1 ({tag})", w, [view(edge - 1), view(edge + G)], [], 2),
            (f"G+1 inside adjacent buckets ({tag})", w, [view(edge), view(edge + G + 1)], [], 2),
            (f"1 ms across a bucket edge ({tag})", w, [view(edge - 1), view(edge)], [], 1),
            (f"equal timestamps on both sides of an edge ({tag})", w,
             [view(edge - 1)] * 2 + [view(edge)] * 3 + [view(edge + G)] * 3 + [view(edge + 2 * G + 1)] * 2, [], 2),
            (f"three buckets chained by exact G ({tag})", w,
             [view(edge + 7), view(edge + 7 + G), view(edge + 7 + 2 * G)], [], 1),
            (f"min and max not first and last read ({tag})", w,
             [view(edge + 29 * 60000), view(edge + 60000), view(edge + 58 * 60000), view(edge + 31 * 60000),
              view(edge + 89 * 60000), view(edge + 88 * 60000)], [(edge + 15 * 60000, b"k", "A")], 1),
            (f"first and last instants of the window ({tag})", w,
             [view(w), view(w + WINDOW - 1)], [(w - WEEK, b"a", "A"), (w + WINDOW - 1, b"b", "B")], 2),
            (f"session across UTC midnight ({tag})", w,
             [view(MIDNIGHT + 3 * DAY - 600_000), (MIDNIGHT + 3 * DAY, "A", 5), (MIDNIGHT + 3 * DAY + G - 1, "B", 7),
              view(MIDNIGHT + 3 * DAY + 2 * G)], [(MIDNIGHT + 3 * DAY - 600_000, b"k", "A")], 2),
        ]
        t = MIDNIGHT + 2 * DAY - 1 - WEEK            # so that t + WEEK is a day's last millisecond
        for name, second, start, sessions in [
            ("same campaign exactly 7 days apart, start on the shared endpoint", t + WEEK, t + WEEK, 1),
            ("same campaign 7 days + 1 ms apart, start between", t + WEEK + 1, t + WEEK, 1),
            ("same campaign 7 days + 2 ms apart, start in the 1 ms hole", t + WEEK + 2, t + WEEK + 1, 1),
            ("same campaign 7 days + 1 ms apart, start on the second click", t + WEEK + 1, t + WEEK + 1, 1),
        ]:
            if start >= w:
                cases.append((f"{name} ({tag})", w, [view(start)], [(t, b"k1", "A"), (second, b"k2", "A")], sessions))
        cases.append((f"interval ending exactly at midnight ({tag})", w,
                      [view(MIDNIGHT + 9 * DAY), view(MIDNIGHT + 9 * DAY + G + 1)],
                      [(MIDNIGHT + 2 * DAY, b"k", "A")], 2))
        cases.append((f"tie between campaigns, the loser still assists ({tag})", w,
                      [view(MIDNIGHT + 6 * DAY)],
                      [(MIDNIGHT + 5 * DAY, b"k10", "A"), (MIDNIGHT + 5 * DAY, b"k9", "B"),
                       (MIDNIGHT + 4 * DAY, b"K", "A"), (MIDNIGHT + 5 * DAY, b"k10", "A")], 1))
    # A heavy person: an event every 2 h for 14 days, a click of A every hour for 21 days,
    # clicks of B every hour for two days, every click three times.
    w = MIDNIGHT
    events = [view(w + hour * 3_600_000) for hour in range(0, 14 * 24, 2)]
    clicks = [(w - WEEK + hour * 3_600_000, b"h%04d" % hour, "A") for hour in range(21 * 24)]
    clicks += [(w + 4 * DAY + hour * 3_600_000, (b"g%02d" if hour < 24 else b"i%02d") % hour, "B")
               for hour in range(48)]
    cases.append(("heavy person, 552 clicks written three times", w, events, clicks * 3, 168))
    return cases


def run_named(slip=None):
    """Returns [(name, description)] for the named cases the (slipped) engine gets wrong."""
    failures = []
    for name, window_start, events, clicks, sessions in named_cases():
        if slip is None and sessions is not None:
            assert len(rules_sessions(events, REAL)) == sessions, name
        problem = agree(events, clicks, window_start, REAL, slip)
        if problem:
            failures.append((name, problem))
    return failures


# --------------------------------------------------------------------------
# Exhaustive small universes (scaled constants)
# --------------------------------------------------------------------------

def exhaustive_sessions(slip=None, stop_at_first=False):
    """Every multiset of events (0, 1 or 2 per instant) on a short line, every grid offset.

    The day is 2 gaps long, so that sessions also cross "midnights".
    Returns (cases run, smallest counterexample or None).
    """
    cases, smallest = 0, None
    for gap, length in ((2, 9), (3, 10)):
        units = Units(gap, 2 * gap, 4 * gap)
        for multiplicity in itertools.product((0, 1, 2), repeat=length):
            events = []
            for ts, copies in enumerate(multiplicity):
                for copy in range(copies):
                    purchase = (ts + copy) % 3 == 0
                    events.append((ts, "AB"[(ts + copy) % 2] if purchase else None,
                                   1 + (ts * 7 + copy) % 5 if purchase else 0))
            events.reverse()                 # not in time order
            for offset in range(gap if slip != "bucket-2G" else 2 * gap):
                cases += 1
                expected = rules_sessions(events, units)
                actual = engine_sessions(events, -offset, units, slip)
                if expected != actual:
                    if smallest is None or len(events) < len(smallest[1]):
                        smallest = (units, events, -offset, expected, actual)
                    if stop_at_first:
                        return cases, smallest
    return cases, smallest


def exhaustive_ads(slip=None, stop_at_first=False):
    """Every set of starts and every list of up to 3 clicks (2 campaigns) on a short line.

    week = 4, day = 2. The click IDs follow the position in the list, so every assignment
    of IDs to (time, campaign) pairs occurs. Every 5th input is run again with each click
    written twice, in another order. Returns (cases run, smallest counterexample or None).
    """
    units = Units(1, 2, 4)
    line = range(0, 6)
    options = [(ts, campaign) for ts in range(-units.week - 1, 6) for campaign in "AB"]
    ids = (b"a", b"b", b"c")
    window_start = -1                # not on a day boundary
    cases, smallest = 0, None
    start_sets = [[s for s in line if mask >> s & 1] for mask in range(1, 1 << len(line))]
    for count in range(0, 4):
        for combo in itertools.product(options, repeat=count):
            clicks = [(ts, ids[i], campaign) for i, (ts, campaign) in enumerate(combo)]
            variants = [clicks]
            if cases % 5 == 0:
                variants.append(clicks[::-1] + clicks)
            for variant in variants:
                for starts in start_sets:
                    cases += 1
                    expected = rules_ads(starts, variant, units)
                    actual = engine_ads(starts, variant, window_start, units, slip)
                    if expected[0] != actual[0] or expected[1] != actual[1]:
                        size = len(starts) + len(variant)
                        if smallest is None or size < smallest[0]:
                            smallest = (size, units, starts, variant, expected, actual)
                        if stop_at_first:
                            return cases, smallest
        if smallest is not None and slip is not None:
            break                    # a slip caught with this many clicks: no need for longer lists
    return cases, smallest


# --------------------------------------------------------------------------
# Adversarial random inputs (real constants)
# --------------------------------------------------------------------------

TRICKY_IDS = [b"K", b"k", b"k1", b"k10", b"k9", b"k_", b"kA", b"ka", b" k", b"k ", b"0", b"-", b"~",
              b"\xc3\xa9", b"k\r", b"10", b"9", b"Z", b"a", b"_"]


def random_person(rng, heavy=False):
    """(window_start, events, clicks) for one person, built to sit on every edge."""
    window_start = MIDNIGHT + rng.choice(OFFSETS + [rng.randrange(DAY)])
    window_end = window_start + WINDOW
    events = []
    ts = window_start + rng.choice([0, 1, G - 1, G, rng.randrange(3 * DAY)])
    for _ in range(rng.randrange(1, 400 if heavy else 40)):
        step = rng.choice([0, 0, 1, G - 1, G, G + 1, 2 * G - 1, 2 * G, 2 * G + 1, rng.randrange(1, 3 * G),
                           rng.randrange(1, 60_000), DAY, rng.randrange(1, 2 * DAY)])
        ts += step
        if rng.random() < 0.3:               # snap onto this grid's bucket edge, or one ms either side
            ts = window_start + (ts - window_start) // G * G + rng.choice([-1, 0, 1, G - 1])
        if rng.random() < 0.1:               # or onto a UTC midnight
            ts = ts // DAY * DAY + rng.choice([-1, 0, 1])
        if ts >= window_end:
            ts = window_end - 1
        if ts < window_start:
            ts = window_start
        category = rng.choice([None, None, "A", "B", "a", "unknown"])
        events.append((ts, category, rng.randrange(1, 12) if category else 0))
    rng.shuffle(events)
    starts = [session[0] for session in rules_sessions(events, REAL)]

    clicks = []
    serial = 0
    for _ in range(rng.randrange(150, 700) if heavy else rng.randrange(0, 30)):
        anchor = rng.choice(starts) if not clicks or rng.random() < 0.6 else rng.choice(clicks)[0]
        ts = anchor + rng.choice([0, 0, 1, -1, 2, -2, -WEEK, -WEEK - 1, -WEEK + 1, WEEK, WEEK + 1, WEEK + 2, WEEK - 1,
                                  -rng.randrange(1, WEEK), -rng.randrange(1, 3_600_000), rng.randrange(1, DAY)])
        ts = max(window_start - WEEK, min(ts, window_end - 1))
        serial += 1
        click = (ts, rng.choice(TRICKY_IDS) + b"#" + str(serial).encode(), rng.choice(["A", "B", "C", "a"]))
        clicks.extend([click] * rng.choice([1, 1, 1, 2, 3]))
    rng.shuffle(clicks)
    return window_start, events, clicks


def run_random(slip=None, people=1500, heavy_people=40, seed=20261121):
    """Returns (people run, first failure or None)."""
    rng = random.Random(seed)
    for index in range(people + heavy_people):
        window_start, events, clicks = random_person(rng, heavy=index >= people)
        problem = agree(events, clicks, window_start, REAL, slip)
        if problem:
            return index + 1, (window_start, len(events), len(clicks), problem[:300])
    return people + heavy_people, None


# --------------------------------------------------------------------------
# Kernel attack
# --------------------------------------------------------------------------

def attack_kernels():
    started = time.time()
    failed = False

    cases = named_cases()
    failures = run_named()
    print(f"named cases: {len(cases) - len(failures)}/{len(cases)} agree with the rules")
    for name, problem in failures:
        failed = True
        print(f"  COUNTEREXAMPLE {name}: {problem}")

    count, smallest = exhaustive_sessions()
    print(f"exhaustive sessions: {count} inputs (0-2 events per instant, every grid offset): "
          f"{'all agree' if smallest is None else 'COUNTEREXAMPLE ' + str(smallest[1:])}")
    failed |= smallest is not None
    count, smallest = exhaustive_ads()
    print(f"exhaustive credit and assists: {count} inputs (every start set, up to 3 clicks, copies): "
          f"{'all agree' if smallest is None else 'COUNTEREXAMPLE ' + str(smallest[2:])}")
    failed |= smallest is not None
    count, failure = run_random()
    print(f"adversarial random people (real constants, shifted windows, heavy people): {count}: "
          f"{'all agree' if failure is None else 'COUNTEREXAMPLE ' + str(failure)}")
    failed |= failure is not None

    print("\nslips (wrong variants must be caught; equivalent ones must survive):")
    for slip in KERNEL_SLIPS:
        equivalent, what = SLIPS[slip]
        found = []
        named = run_named(slip)
        if named:
            found.append(f"named case '{named[0][0]}'")
        smallest = exhaustive_sessions(slip)[1] if slip in SESSION_SLIPS else None
        if smallest is not None:
            units, events, window_start, expected, actual = smallest
            found.append(f"smallest: G={units.gap}, window_start={window_start}, events at "
                         f"{sorted(event[0] for event in events)} -> rules {len(expected)} sessions "
                         f"{[s[:2] for s in expected]}, slip {len(actual)} {[s[:2] for s in actual]}")
        smallest = exhaustive_ads(slip)[1] if slip not in SESSION_SLIPS else None
        if smallest is not None:
            _, units, starts, clicks, expected, actual = smallest
            shown = [(ts, click_id.decode(), campaign) for ts, click_id, campaign in clicks]
            found.append(f"smallest: week={units.week}, day={units.day}, window_start=-1, starts {starts}, "
                         f"clicks {shown} -> "
                         f"rules credit {expected[0]} assists {dict(expected[1])}, "
                         f"slip credit {actual[0]} assists {dict(actual[1])}")
        _, failure = run_random(slip, people=300, heavy_people=5)
        if failure is not None:
            found.append("random people")
        if equivalent:
            verdict = "survives, as it should" if not found else "KILLED BUT SHOULD BE EQUIVALENT"
            failed |= bool(found)
        else:
            verdict = "caught" if found else "NOT CAUGHT"
            failed |= not found
        print(f"  {verdict:24s} {slip}: {what}")
        for line in found[:3]:
            if not equivalent or found:
                print(f"      {line}")
    print(f"\n{'FAIL' if failed else 'PASS'}: kernel attack in {time.time() - started:.0f}s")
    return 1 if failed else 0


# --------------------------------------------------------------------------
# The rewrites as a whole pipeline
# --------------------------------------------------------------------------

def read_clicks_without_dedup(data_dir, window_start, window_end, campaigns):
    """Rewrite 4: every well-formed, valid click line, copies included."""
    clicks = []
    for path in pyref.csv_files(os.path.join(data_dir, "clicks")):
        for line in pyref.data_lines(path):
            fields = pyref.parse_line(line, pyref.CLICK_FIELDS)
            if fields is None:
                continue
            click_id, user_id, campaign_id, ts_text = fields
            ts = pyref.parse_ts(ts_text)
            if click_id == b"" or user_id == b"" or ts is None or campaign_id not in campaigns:
                continue
            if not (window_start - WEEK <= ts < window_end):
                continue
            clicks.append((user_id, ts, click_id, campaign_id))
    return clicks


def load_inputs(data_dir):
    """Everything before the rewrites: the window, the campaigns, each person's surviving
    events as (ts, category or None, cents) and valid clicks (copies included)."""
    window_start, window_end = pyref.read_window(data_dir)
    products = pyref.read_lookup(os.path.join(data_dir, "products.csv"), pyref.PRODUCT_FIELDS, "products.csv")
    campaigns = pyref.read_lookup(os.path.join(data_dir, "campaigns.csv"), pyref.CAMPAIGN_FIELDS, "campaigns.csv")
    name_of = pyref.read_people(data_dir)

    events_of = collections.defaultdict(list)
    for user_id, ts, event_type, product_id, price in pyref.read_events(data_dir, window_start, window_end):
        category = products.get(product_id, pyref.UNKNOWN) if event_type == b"purchase" else None
        events_of[name_of.get(user_id, user_id)].append((ts, category, price))
    clicks_of = collections.defaultdict(list)
    for user_id, ts, click_id, campaign_id in read_clicks_without_dedup(data_dir, window_start, window_end, campaigns):
        clicks_of[name_of.get(user_id, user_id)].append((ts, click_id, campaign_id))
    return window_start, campaigns, events_of, clicks_of


def engine_tables(data, slip=None):
    """Both tables, built with the rewrites. Rows as pyref writes them.

    data: a dataset folder, or what load_inputs returned for it.
    """
    window_start, campaigns, events_of, clicks_of = load_inputs(data) if isinstance(data, str) else data
    user_daily = collections.defaultdict(lambda: [0, 0, 0, collections.Counter(), []])
    campaign_daily = collections.defaultdict(lambda: [0, 0, 0, 0])
    for person, events in events_of.items():
        sessions = engine_sessions(events, window_start, REAL, slip)
        starts = [session[0] for session in sessions]
        clicks = clicks_of.get(person, [])
        credits, assists = engine_ads(starts, clicks, window_start, REAL, slip)
        for (start, count, revenue, categories), campaign in zip(sessions, credits):
            if slip == "window-relative-day":
                day = window_start // DAY + (start - window_start) // DAY
            else:
                day = start // DAY
            row = user_daily[person, day]
            row[0] += 1
            row[1] += count
            row[2] += revenue
            row[3].update(categories)
            if categories:
                highest = max(categories.values())
                row[4].append((highest, min(c for c, cents in categories.items() if cents == highest)))
            credited = campaign_daily[campaign, day]
            credited[0] += 1
            credited[1] += 1 if revenue > 0 else 0
            credited[2] += revenue
        for (campaign, day), count in assists.items():
            campaign_daily[campaign, day][3] += count
        if slip == "purchases-on-event-day":
            # Redo revenue and categories by each purchase's own UTC day.
            days = {start // DAY for start in starts}
            for day in days:
                user_daily[person, day][2] = 0
                user_daily[person, day][3] = collections.Counter()
            for ts, category, cents in events:
                if category is not None and ts // DAY in days:
                    row = user_daily[person, ts // DAY]
                    row[2] += cents
                    row[3][category] += cents

    def day_text(day):
        return pyref.utc_day(day * DAY)

    user_rows = []
    for (person, day), (sessions, events, revenue, categories, winners) in user_daily.items():
        if slip == "category-of-session-winners":
            top = min(winners, key=lambda w: (-w[0], w[1]))[1] if winners else None
        elif categories:
            highest = max(categories.values())
            top = min(category for category, cents in categories.items() if cents == highest)
        else:
            top = None
        user_rows.append((person, day_text(day), sessions, events, revenue, top))
    campaign_rows = [(campaign if campaign != ORGANIC else pyref.ORGANIC,
                      campaigns[campaign] if campaign != ORGANIC else pyref.ORGANIC, day_text(day), *row)
                     for (campaign, day), row in campaign_daily.items()]
    return sorted(user_rows, key=lambda r: (r[0], r[1])), sorted(campaign_rows, key=lambda r: (r[0], r[2]))


def tables_as_text(user_rows, campaign_rows):
    """The two tables read back from CSV the way pyref_fixtures reads expected files."""
    with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as folder:
        pyref.write_csv(os.path.join(folder, "user_daily.csv"),
                        b"user_id,day,sessions,events,revenue_cents,top_category", user_rows)
        pyref.write_csv(os.path.join(folder, "campaign_daily.csv"),
                        b"campaign_id,channel,day,sessions,purchasing_sessions,revenue_cents,assisted_sessions",
                        campaign_rows)
        return {table: pyref_fixtures.read_table(os.path.join(folder, f"{table}.csv"), header)
                for table, header in pyref_fixtures.TABLES.items()}


def fixture_cases(folders):
    for folder in folders:
        for case in sorted(os.listdir(folder)):
            if os.path.isdir(os.path.join(folder, case, "input")):
                yield f"{os.path.basename(folder)}/{case}", os.path.join(folder, case)


def attack_fixtures(folders):
    folders = folders or [os.path.join(REPO, "fixtures"), os.path.join(REPO, "extra_fixtures")]
    cases = list(fixture_cases(folders))
    expected = {name: {table: pyref_fixtures.read_table(os.path.join(path, "expected", f"{table}.csv"), header)
                       for table, header in pyref_fixtures.TABLES.items()} for name, path in cases}

    def failing(slip):
        return [name for name, path in cases
                if tables_as_text(*engine_tables(os.path.join(path, "input"), slip)) != expected[name]]

    wrong = failing(None)
    print(f"the rewrites as a pipeline: {len(cases) - len(wrong)}/{len(cases)} fixtures match their expected tables")
    for name in wrong:
        print(f"  DIFFERS {name}")
    failed = bool(wrong)
    print("\nwhich fixtures catch each slip:")
    for slip, (equivalent, what) in SLIPS.items():
        killers = failing(slip)
        official = [name.split("/")[1] for name in killers if name.startswith("fixtures/")]
        extra = [name.split("/")[1] for name in killers if not name.startswith("fixtures/")]
        if equivalent:
            status = "survives, as it should" if not killers else "KILLED BUT EQUIVALENT"
            failed |= bool(killers)
        else:
            status = "caught" if killers else "NOT CAUGHT"
            failed |= not killers
        print(f"  {status:24s} {slip}: {what}")
        if killers:
            print(f"      official: {' '.join(official) or '-'}")
            print(f"      extra:    {' '.join(extra) or '-'}")
    print(f"\n{'FAIL' if failed else 'PASS'}: fixture attack")
    return 1 if failed else 0


def attack_data(folders, with_slips=False):
    failed = False
    for folder in folders:
        started = time.time()
        expected = pyref.build_tables(folder)
        inputs = load_inputs(folder)
        actual = engine_tables(inputs)
        same = True
        for table, expected_rows, actual_rows in zip(("user_daily", "campaign_daily"), expected, actual):
            only_expected = set(expected_rows) - set(actual_rows)
            only_actual = set(actual_rows) - set(expected_rows)
            if only_expected or only_actual or len(expected_rows) != len(actual_rows):
                same = False
                print(f"  {table}: pyref rows {len(expected_rows)}, rewrites rows {len(actual_rows)}; "
                      f"only in pyref {len(only_expected)}, only in rewrites {len(only_actual)}")
                for row in sorted(only_expected, key=repr)[:3]:
                    print(f"    pyref    {row}")
                for row in sorted(only_actual, key=repr)[:3]:
                    print(f"    rewrites {row}")
        print(f"{'MATCH ' if same else 'DIFFER'} {folder}: user_daily {len(expected[0])} rows, "
              f"campaign_daily {len(expected[1])} rows ({time.time() - started:.0f}s)")
        failed |= not same
        if with_slips:
            print("  slips this dataset catches (rows that differ from pyref: user_daily, campaign_daily):")
            for slip, (equivalent, what) in SLIPS.items():
                slipped = engine_tables(inputs, slip)
                wrong = [len(set(rows) ^ set(reference)) for rows, reference in zip(slipped, expected)]
                if equivalent:
                    status = "survives, as it should" if not any(wrong) else "DIFFERS BUT EQUIVALENT"
                    failed |= any(wrong)
                else:
                    status = "caught" if any(wrong) else "NOT CAUGHT here"
                print(f"    {status:24s} {wrong[0]:7d} {wrong[1]:7d}  {slip}: {what}")
    return 1 if failed else 0


def main():
    sys.stdout.reconfigure(errors="backslashreplace")
    arguments = sys.argv[1:]
    if arguments[:1] == ["--fixtures"]:
        return attack_fixtures(arguments[1:])
    if arguments[:1] == ["--data"] and len(arguments) > 1:
        return attack_data([folder for folder in arguments[1:] if folder != "--slips"],
                           with_slips="--slips" in arguments)
    if arguments:
        sys.exit(__doc__)
    return attack_kernels()


if __name__ == "__main__":
    sys.exit(main())
