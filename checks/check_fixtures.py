"""Compare logical output rows with the public fixture CSVs. No third-party packages."""
import csv
import json
import re
import sys
from collections import Counter, defaultdict
from datetime import date
from pathlib import Path
from check_rewrites import DAY, WEEK, bucket_sessions, compact_ads

EPOCH = date(1970, 1, 1)
DATE_TIME = re.compile(r'([0-9]{4})-([0-9]{2})-([0-9]{2})T([0-9]{2}):([0-9]{2}):([0-9]{2})(Z|[+-][0-9]{2}:[0-9]{2})')
DIGITS = re.compile(r'[0-9]+')

def millis(value):
    if DIGITS.fullmatch(value):
        trimmed = value.lstrip("0") or "0"
        return int(trimmed) if len(trimmed) <= 19 else None
    match = DATE_TIME.fullmatch(value)
    if match is None:
        return None
    y, m, d, h, mi, s = map(int, match.groups()[:6])
    zone = match[7]
    if h >= 24 or mi >= 60 or s >= 60:
        return None
    offset = 0
    if zone != "Z":
        oh, om = int(zone[1:3]), int(zone[4:6])
        if oh >= 24 or om >= 60:
            return None
        offset = (oh * 60 + om) * (1 if zone[0] == "+" else -1)
    try:
        return (date(y,m,d) - EPOCH).days * DAY + ((h*60+mi-offset)*60+s)*1000
    except ValueError:
        return None

def records(paths, width):
    for path in sorted(paths):
        for line in path.read_bytes().split(b'\n')[1:]:
            fields = line.split(b',')
            if len(fields) != width:
                continue
            decoded = []
            for field in fields:
                if len(field) >= 2 and field[:1] == b'"' and field[-1:] == b'"':
                    field = field[1:-1]
                if b'"' in field:
                    break
                decoded.append(field.decode('utf-8'))
            else:
                yield decoded

def logical_tables(data):
    manifest = json.loads((data/'manifest.json').read_text())
    lo, hi = millis(manifest['window_start']), millis(manifest['window_end'])
    products = dict(records([data/'products.csv'], 2))
    campaigns = dict(records([data/'campaigns.csv'], 2))
    # Simple graph traversal gives an independent reference for alias resolution.
    graph = defaultdict(set)
    for a,b in records((data/'links').glob('*.csv'), 2):
        if a and b:
            graph[a].add(b)
            graph[b].add(a)
    names = {}
    for a in graph:
        if a in names:
            continue
        todo, component = [a], {a}
        while todo:
            for b in graph[todo.pop()]:
                if b not in component:
                    component.add(b)
                    todo.append(b)
        name = min(component, key=str.encode)
        names.update(dict.fromkeys(component, name))

    winners = {}
    for r in records((data/'events').glob('*.csv'), 7):
        eid = r[0]
        if eid and (eid not in winners or r[3] < winners[eid][3]):
            winners[eid] = r
    events = defaultdict(list)
    for _, uid, ts, ingest, kind, product, price in winners.values():
        t = millis(ts)
        if not uid or t is None or not lo <= t < hi or not product:
            continue
        if kind not in ('view','cart','purchase') or millis(ingest)-t > DAY:
            continue
        cat, cents = None, 0
        if kind == 'purchase':
            if not DIGITS.fullmatch(price):
                continue
            p = price.lstrip('0') or '0'
            if len(p) > 8 or not 1 <= int(p) <= 10_000_000:
                continue
            cat, cents = products.get(product, 'unknown'), int(p)
        events[names.get(uid,uid)].append((t,cat,cents))
    clicks = defaultdict(list)
    for cid, uid, campaign, ts in records((data/'clicks').glob('*.csv'), 4):
        t = millis(ts)
        if cid and uid and campaign in campaigns and t is not None and lo-WEEK <= t < hi:
            clicks[names.get(uid,uid)].append((t,cid.encode(),campaign))

    users = {}
    ads = defaultdict(lambda: [0,0,0,0])
    for person, ev in events.items():
        sessions = bucket_sessions(ev)
        credits, assists = compact_ads([s[0] for s in sessions], clicks[person])
        daily = defaultdict(lambda: [0,0,0,Counter()])
        for (start, _, n, revenue, cats), campaign in zip(sessions,credits):
            day = start // DAY
            user = daily[day]
            user[0] += 1
            user[1] += n
            user[2] += revenue
            user[3].update(cats)
            cm = ads[campaign,day]
            cm[0] += 1
            cm[1] += int(revenue > 0)
            cm[2] += revenue
        for day,(n,evs,revenue,cats) in daily.items():
            top = min(cats, key=lambda c: (-cats[c],c.encode())) if cats else ''
            users[person,day] = (n,evs,revenue,top)
        for key,n in assists.items():
            ads[key][3] += n

    def daystr(day):
        return date.fromordinal(EPOCH.toordinal()+day).isoformat()
    user_rows = [(p,daystr(d),*v) for (p,d),v in users.items()]
    ad_rows = [(c,campaigns.get(c,'organic'),daystr(d),*v) for (c,d),v in ads.items()]
    return {'user_daily': user_rows, 'campaign_daily': ad_rows}

def run(root):
    cases = sorted(p for p in root.iterdir() if p.is_dir())
    if not cases:
        raise ValueError('No fixture directories found')
    columns = {'user_daily': 'user_id day sessions events revenue_cents top_category',
               'campaign_daily': 'campaign_id channel day sessions purchasing_sessions revenue_cents assisted_sessions'}
    for case in cases:
        for name, rows in logical_tables(case/'input').items():
            with (case/'expected'/f'{name}.csv').open(newline='') as f:
                expected = [tuple(r[k] for k in columns[name].split()) for r in csv.DictReader(f)]
            actual = [tuple(map(str,r)) for r in rows]
            if Counter(expected) != Counter(actual):
                raise AssertionError((case.name,name,expected,actual))
    print(f'PASS: {len(cases)} public fixtures, {2*len(cases)} logical output tables matched')

if __name__ == '__main__':
    run(Path(sys.argv[1]))
