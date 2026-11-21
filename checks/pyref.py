#!/usr/bin/env python3
"""
pyref.py: an independent reference implementation of the "Shopping Sessions" rules.

Written from rules.md alone, in pure standard-library Python. It is meant to be
slow and obviously correct: every step below mirrors one sentence of the rules,
in the order the rules give them, and nothing is clever.

Usage:  python checks/pyref.py DATA_DIR OUT_DIR

Writes OUT_DIR/user_daily.csv and OUT_DIR/campaign_daily.csv in the format of
the fixtures' expected/ files and of the official dev answer (a header row,
`day` as YYYY-MM-DD, an empty `top_category` for null), so scripts/compare.py
can diff the folder against another CSV folder or against a Parquet output.

Every ID and value is handled as raw bytes: files are read in binary mode and
nothing is ever decoded, trimmed, normalized or case-folded. Python compares
bytes objects byte by byte, which is the order the rules ask for.

Counters for every step go to stderr; so do warnings when the data breaks a
promise the rules make (the result would then depend on the implementation).
"""
import collections
import datetime
import json
import os
import sys

# An F3 timestamp is "just a number", however long: lift Python's limit on the
# number of digits int() accepts, so that int() is exact for any digit string.
# (Python before 3.11 has no such limit and no such function.)
if hasattr(sys, "set_int_max_str_digits"):
    sys.set_int_max_str_digits(0)

MINUTE_MS = 60 * 1000
HOUR_MS = 60 * MINUTE_MS
DAY_MS = 24 * HOUR_MS
SESSION_GAP_MS = 30 * MINUTE_MS      # 1,800,000 ms: a longer gap starts a new session
LATE_MS = 24 * HOUR_MS               # ingest_ts - ts over this drops the event
CLICK_REACH_MS = 7 * DAY_MS          # 604,800,000 ms: how far back a click still counts
MAX_PRICE = 10_000_000

EPOCH_ORDINAL = datetime.date(1970, 1, 1).toordinal()

ORGANIC = b"organic"
UNKNOWN = b"unknown"
EVENT_TYPES = (b"view", b"cart", b"purchase")

EVENT_FIELDS = 7     # event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents
CLICK_FIELDS = 4     # click_id,user_id,campaign_id,ts
LINK_FIELDS = 2      # user_id_a,user_id_b
PRODUCT_FIELDS = 2   # product_id,category
CAMPAIGN_FIELDS = 2  # campaign_id,channel

stats = collections.Counter()
warnings = collections.Counter()


# --------------------------------------------------------------------------
# CSV dialect
# --------------------------------------------------------------------------

def data_lines(path):
    """The data lines of one CSV file: every line after the header row.

    Only `\\n` ends a line. A `\\r` is an ordinary character and stays where it
    is. Text after the last `\\n` is a line too (a last line without a trailing
    newline); nothing after the last `\\n` is not a line.
    """
    with open(path, "rb") as handle:
        content = handle.read()
    lines = content.split(b"\n")
    if lines[-1] == b"":
        lines.pop()          # the file ended with "\n" (or is empty): no line there
    return lines[1:]         # every CSV file has a header row: the first line


def csv_files(folder):
    """Every *.csv file directly in the folder, in a fixed order."""
    if not os.path.isdir(folder):
        return []
    names = sorted(name for name in os.listdir(folder) if name.endswith(".csv"))
    return [os.path.join(folder, name) for name in names if os.path.isfile(os.path.join(folder, name))]


def parse_line(line, field_count):
    """The line's field values, or None if the line is malformed.

    Every comma separates fields. A field may be wrapped in double quotes,
    which are syntax and not part of the value. A quote anywhere except around
    a whole field makes the line malformed, and so does a wrong number of
    fields (which covers the empty line: it has one field).
    """
    fields = line.split(b",")
    if len(fields) != field_count:
        return None
    values = []
    for field in fields:
        if b'"' in field:
            wrapped = len(field) >= 2 and field[:1] == b'"' and field[-1:] == b'"'
            if not wrapped or b'"' in field[1:-1]:
                return None
            field = field[1:-1]
        values.append(field)
    return values


# --------------------------------------------------------------------------
# Timestamps
# --------------------------------------------------------------------------

def parse_ts(value):
    """Epoch milliseconds (UTC) of an F1, F2 or F3 timestamp, or None if bad.

    bytes.isdigit() is true only for a non-empty run of the ASCII bytes 0-9.
    """
    # F3: digits only, epoch milliseconds, leading zeros allowed, any length.
    if value.isdigit():
        return int(value)

    # F1 `YYYY-MM-DDTHH:MM:SSZ` or F2 `YYYY-MM-DDTHH:MM:SS+HH:MM` / `-HH:MM`.
    if len(value) == 20 and value[19:20] == b"Z":
        offset_ms = 0
    elif (len(value) == 25 and value[19:20] in (b"+", b"-") and value[20:22].isdigit()
          and value[22:23] == b":" and value[23:25].isdigit()):
        offset_hours, offset_minutes = int(value[20:22]), int(value[23:25])
        if offset_hours > 23 or offset_minutes > 59:
            return None
        offset_ms = offset_hours * HOUR_MS + offset_minutes * MINUTE_MS
        if value[19:20] == b"-":
            offset_ms = -offset_ms
    else:
        return None

    if not (value[0:4].isdigit() and value[4:5] == b"-" and value[5:7].isdigit() and value[7:8] == b"-"
            and value[8:10].isdigit() and value[10:11] == b"T" and value[11:13].isdigit()
            and value[13:14] == b":" and value[14:16].isdigit() and value[16:17] == b":"
            and value[17:19].isdigit()):
        return None
    year, month, day = int(value[0:4]), int(value[5:7]), int(value[8:10])
    hour, minute, second = int(value[11:13]), int(value[14:16]), int(value[17:19])
    if hour > 23 or minute > 59 or second > 59:
        return None
    try:
        # Raises for an impossible date: month 13, February 30, February 29
        # outside a leap year, day 00. (Also for year 0000, far outside any window.)
        days = datetime.date(year, month, day).toordinal() - EPOCH_ORDINAL
    except ValueError:
        return None
    local_ms = days * DAY_MS + hour * HOUR_MS + minute * MINUTE_MS + second * 1000
    return local_ms - offset_ms      # subtract the offset to get UTC


def utc_day(ms):
    """The UTC date of an epoch-millisecond instant, as b'YYYY-MM-DD'."""
    return datetime.date.fromordinal(EPOCH_ORDINAL + ms // DAY_MS).isoformat().encode("ascii")


def valid_price(value):
    """A purchase price: digits only, with a value from 1 to 10,000,000."""
    return value.isdigit() and 1 <= int(value) <= MAX_PRICE


# --------------------------------------------------------------------------
# Inputs
# --------------------------------------------------------------------------

def read_window(data_dir):
    with open(os.path.join(data_dir, "manifest.json"), "rb") as handle:
        manifest = json.loads(handle.read().decode("utf-8"))
    window_start = parse_ts(manifest["window_start"].encode("utf-8"))
    window_end = parse_ts(manifest["window_end"].encode("utf-8"))
    if window_start is None or window_end is None:
        sys.exit(f"pyref: cannot read the window from manifest.json: {manifest!r}")
    return window_start, window_end


def read_lookup(path, field_count, what):
    """products.csv or campaigns.csv: {id: second column}. The rules call them clean."""
    lookup = {}
    for line in data_lines(path):
        fields = parse_line(line, field_count)
        if fields is None:
            warnings[f"{what}: malformed line in a file the rules call clean (line ignored)"] += 1
            continue
        key, value = fields
        if key in lookup:
            warnings[f"{what}: repeated ID in a file the rules call unique (first kept)"] += 1
            continue
        lookup[key] = value
    return lookup


def read_people(data_dir):
    """Linked IDs: {id: name of its person} for every ID in a remaining link line."""
    neighbours = collections.defaultdict(set)
    for path in csv_files(os.path.join(data_dir, "links")):
        for line in data_lines(path):
            stats["links: lines"] += 1
            # 1. Parse. Drop malformed lines and lines where either field is empty.
            fields = parse_line(line, LINK_FIELDS)
            if fields is None:
                stats["links: malformed, dropped"] += 1
                continue
            id_a, id_b = fields
            if id_a == b"" or id_b == b"":
                stats["links: a side empty, dropped"] += 1
                continue
            # 2. Join. Every remaining line joins its two IDs into one person.
            neighbours[id_a].add(id_b)
            neighbours[id_b].add(id_a)

    # Joining is transitive: a person is everything reachable through the lines.
    name_of = {}
    for first in neighbours:
        if first in name_of:
            continue
        person = {first}
        to_visit = [first]
        while to_visit:
            for other in neighbours[to_visit.pop()]:
                if other not in person:
                    person.add(other)
                    to_visit.append(other)
        # 3. Name each person by the byte-wise smallest of their IDs.
        name = min(person)
        for member in person:
            name_of[member] = name
        stats["links: people with linked IDs"] += 1
    stats["links: IDs in remaining lines"] = len(name_of)
    return name_of


Event = collections.namedtuple("Event", "event_id user_id ts ingest_ts event_type product_id price_cents")
Click = collections.namedtuple("Click", "click_id user_id campaign_id ts")


def read_events(data_dir, window_start, window_end):
    """Steps 1 to 4: the surviving events as (user_id, ts_ms, event_type, product_id, price)."""
    # 1. Parse. Drop malformed lines and lines whose event_id is empty.
    # 2. Dedup on event_id. Keep only the copy with the smallest ingest_ts.
    kept = {}                # event_id -> (ingest_ms, Event)
    tied_and_different = set()
    for path in csv_files(os.path.join(data_dir, "events")):
        for line in data_lines(path):
            stats["events: lines"] += 1
            fields = parse_line(line, EVENT_FIELDS)
            if fields is None:
                stats["events: malformed, dropped"] += 1
                continue
            event = Event(*fields)
            if event.event_id == b"":
                stats["events: empty event_id, dropped"] += 1
                continue
            ingest_ms = parse_ts(event.ingest_ts)
            if ingest_ms is None or not (len(event.ingest_ts) == 20 and event.ingest_ts.endswith(b"Z")):
                # The rules promise ingest_ts is always valid F1 and so say
                # nothing about this. Refuse to guess.
                sys.exit(f"pyref: ingest_ts is not valid F1, which the rules rule out: {line!r} in {path}")
            if event.event_id not in kept:
                kept[event.event_id] = (ingest_ms, event)
                continue
            stats["events: extra copies of an event_id"] += 1
            kept_ingest_ms, kept_event = kept[event.event_id]
            if ingest_ms < kept_ingest_ms:
                kept[event.event_id] = (ingest_ms, event)
                tied_and_different.discard(event.event_id)
            elif ingest_ms == kept_ingest_ms and event != kept_event:
                tied_and_different.add(event.event_id)
    if tied_and_different:
        warnings["events: copies with the smallest ingest_ts are NOT identical, which the rules rule out "
                 "(first read is kept; the result depends on the implementation)"] += len(tied_and_different)
    stats["events: distinct event_id"] = len(kept)

    surviving = []
    for ingest_ms, event in kept.values():
        # 3. Validate the kept copy.
        if event.user_id == b"":
            stats["events: empty user_id, dropped"] += 1
            continue
        ts = parse_ts(event.ts)
        if ts is None:
            stats["events: bad ts, dropped"] += 1
            continue
        if not (window_start <= ts < window_end):
            stats["events: ts outside the window, dropped"] += 1
            continue
        if event.event_type not in EVENT_TYPES:
            stats["events: bad event_type, dropped"] += 1
            continue
        if event.product_id == b"":
            stats["events: empty product_id, dropped"] += 1
            continue
        price = 0
        if event.event_type == b"purchase":
            if not valid_price(event.price_cents):
                stats["events: bad purchase price, dropped"] += 1
                continue
            price = int(event.price_cents)
        # 4. Drop late events: ingest_ts - ts over 24 hours. Exactly 24 hours is kept.
        if ingest_ms - ts > LATE_MS:
            stats["events: late, dropped"] += 1
            continue
        surviving.append((event.user_id, ts, event.event_type, event.product_id, price))
    stats["events: surviving"] = len(surviving)
    return surviving


def read_clicks(data_dir, window_start, window_end, campaigns):
    """Steps 1 to 3: the valid clicks as (user_id, ts_ms, click_id, campaign_id)."""
    # 1. Parse. Drop malformed lines and lines whose click_id is empty.
    # 2. Dedup on click_id. Copies of a click have identical values, so keep any one.
    kept = {}                # click_id -> Click
    different_copies = set()
    for path in csv_files(os.path.join(data_dir, "clicks")):
        for line in data_lines(path):
            stats["clicks: lines"] += 1
            fields = parse_line(line, CLICK_FIELDS)
            if fields is None:
                stats["clicks: malformed, dropped"] += 1
                continue
            click = Click(*fields)
            if click.click_id == b"":
                stats["clicks: empty click_id, dropped"] += 1
                continue
            if click.click_id not in kept:
                kept[click.click_id] = click
                continue
            stats["clicks: extra copies of a click_id"] += 1
            if click != kept[click.click_id]:
                different_copies.add(click.click_id)
    if different_copies:
        warnings["clicks: copies of a click_id are NOT identical, which the rules rule out "
                 "(first read is kept; the result depends on the implementation)"] += len(different_copies)
    stats["clicks: distinct click_id"] = len(kept)

    valid = []
    for click in kept.values():
        # 3. Validate.
        if click.user_id == b"":
            stats["clicks: empty user_id, dropped"] += 1
            continue
        ts = parse_ts(click.ts)
        if ts is None:
            stats["clicks: bad ts, dropped"] += 1
            continue
        if not (window_start - CLICK_REACH_MS <= ts < window_end):
            stats["clicks: ts outside the click window, dropped"] += 1
            continue
        if click.campaign_id not in campaigns:
            stats["clicks: campaign not in campaigns.csv, dropped"] += 1
            continue
        valid.append((click.user_id, ts, click.click_id, click.campaign_id))
    stats["clicks: valid"] = len(valid)
    return valid


# --------------------------------------------------------------------------
# The two tables
# --------------------------------------------------------------------------

def build_tables(data_dir):
    window_start, window_end = read_window(data_dir)
    products = read_lookup(os.path.join(data_dir, "products.csv"), PRODUCT_FIELDS, "products.csv")
    campaigns = read_lookup(os.path.join(data_dir, "campaigns.csv"), CAMPAIGN_FIELDS, "campaigns.csv")
    if ORGANIC in campaigns:
        warnings["campaigns.csv holds the ID `organic`, which the rules rule out"] += 1
    if UNKNOWN in products.values():
        warnings["products.csv uses the category `unknown`, which the rules rule out"] += 1

    # Links come before everything else: an event or click is the person's as
    # soon as it is parsed, whatever its own ID.
    name_of = read_people(data_dir)

    def person_of(user_id):
        # An ID that appears in no remaining link line is a person on its own.
        return name_of.get(user_id, user_id)

    events_of = collections.defaultdict(list)       # person -> [(ts, event_type, product_id, price)]
    for user_id, ts, event_type, product_id, price in read_events(data_dir, window_start, window_end):
        events_of[person_of(user_id)].append((ts, event_type, product_id, price))

    clicks_of = collections.defaultdict(list)       # person -> [(ts, click_id, campaign_id)]
    for user_id, ts, click_id, campaign_id in read_clicks(data_dir, window_start, window_end, campaigns):
        clicks_of[person_of(user_id)].append((ts, click_id, campaign_id))

    # (person, day) -> totals
    user_daily = collections.defaultdict(lambda: {"sessions": 0, "events": 0, "revenue_cents": 0,
                                                  "by_category": collections.Counter()})
    # (campaign_id, day) -> totals
    campaign_daily = collections.defaultdict(lambda: {"sessions": 0, "purchasing_sessions": 0,
                                                      "revenue_cents": 0, "assisted_sessions": 0})

    for person, events in events_of.items():
        # 5. Sessionize the person's surviving events, all of their IDs together,
        #    sorted by ts. A new session starts when the gap from the previous
        #    event is over 30 minutes. Events with equal ts have a gap of 0.
        events.sort(key=lambda event: event[0])
        sessions = []
        previous_ts = None
        for event in events:
            if previous_ts is None or event[0] - previous_ts > SESSION_GAP_MS:
                sessions.append([])
            sessions[-1].append(event)
            previous_ts = event[0]

        clicks = clicks_of.get(person, [])
        for session in sessions:
            stats["sessions"] += 1
            # A session starts at its first event's ts.
            session_start = session[0][0]
            # 6. One day per session: the UTC date of its start.
            day = utc_day(session_start)

            purchases = [(product_id, price) for _, event_type, product_id, price in session
                         if event_type == b"purchase"]
            revenue = sum(price for _, price in purchases)

            row = user_daily[(person, day)]
            row["sessions"] += 1
            row["events"] += len(session)
            row["revenue_cents"] += revenue
            for product_id, price in purchases:
                # 7. A product not in products.csv has the category `unknown`.
                row["by_category"][products.get(product_id, UNKNOWN)] += price

            # Clicks 4. The person's valid clicks with click.ts <= session_start
            # and session_start - click.ts <= 7 days.
            qualifying = [click for click in clicks
                          if click[0] <= session_start and session_start - click[0] <= CLICK_REACH_MS]
            if qualifying:
                # The latest ts; on a tie in ts, the byte-wise smallest click_id.
                latest_ts = max(click[0] for click in qualifying)
                chosen = min((click for click in qualifying if click[0] == latest_ts),
                             key=lambda click: click[1])
                credited = chosen[2]
            else:
                # Clicks 5. No qualifying click credits the session to `organic`.
                credited = ORGANIC
                stats["sessions credited to organic"] += 1

            row = campaign_daily[(credited, day)]
            row["sessions"] += 1
            if purchases:
                row["purchasing_sessions"] += 1
            row["revenue_cents"] += revenue

            # Clicks 7. Every campaign with at least one qualifying click assists
            # the session once. `organic` is never among them: it has no clicks.
            for campaign_id in {click[2] for click in qualifying}:
                campaign_daily[(campaign_id, day)]["assisted_sessions"] += 1

    user_rows = []
    for (person, day), row in user_daily.items():
        if row["by_category"]:
            # The highest summed purchase price; a tie goes to the byte-wise smallest.
            highest = max(row["by_category"].values())
            top_category = min(category for category, total in row["by_category"].items() if total == highest)
        else:
            top_category = None      # null with no purchases
        user_rows.append((person, day, row["sessions"], row["events"], row["revenue_cents"], top_category))
    user_rows.sort(key=lambda r: (r[0], r[1]))

    campaign_rows = []
    for (campaign_id, day), row in campaign_daily.items():
        channel = ORGANIC if campaign_id == ORGANIC else campaigns[campaign_id]
        campaign_rows.append((campaign_id, channel, day, row["sessions"], row["purchasing_sessions"],
                              row["revenue_cents"], row["assisted_sessions"]))
    campaign_rows.sort(key=lambda r: (r[0], r[2]))

    stats["user_daily rows"] = len(user_rows)
    stats["campaign_daily rows"] = len(campaign_rows)
    return user_rows, campaign_rows


# --------------------------------------------------------------------------
# Output
# --------------------------------------------------------------------------

def csv_field(value):
    """One output field. None (null) is the empty field.

    Values go out as their raw bytes. A value cannot hold a comma, a quote or
    a newline (such a line was malformed), but it can hold a `\\r`; that one is
    wrapped in quotes, because CSV readers strip a bare `\\r` at a field's end.
    """
    if value is None:
        return b""
    if isinstance(value, int):
        return str(value).encode("ascii")
    if b"\r" in value or b'"' in value or b"," in value or b"\n" in value:
        return b'"' + value.replace(b'"', b'""') + b'"'
    return value


def write_csv(path, header, rows):
    with open(path, "wb") as handle:
        handle.write(header + b"\n")
        for row in rows:
            handle.write(b",".join(csv_field(value) for value in row) + b"\n")


def main():
    if len(sys.argv) != 3:
        sys.exit("usage: python pyref.py DATA_DIR OUT_DIR")
    data_dir, out_dir = sys.argv[1], sys.argv[2]
    user_rows, campaign_rows = build_tables(data_dir)
    os.makedirs(out_dir, exist_ok=True)
    write_csv(os.path.join(out_dir, "user_daily.csv"),
              b"user_id,day,sessions,events,revenue_cents,top_category", user_rows)
    write_csv(os.path.join(out_dir, "campaign_daily.csv"),
              b"campaign_id,channel,day,sessions,purchasing_sessions,revenue_cents,assisted_sessions",
              campaign_rows)
    for name, count in stats.items():
        print(f"pyref: {name}: {count}", file=sys.stderr)
    for name, count in warnings.items():
        print(f"pyref: WARNING: {name}: {count}", file=sys.stderr)


if __name__ == "__main__":
    main()
