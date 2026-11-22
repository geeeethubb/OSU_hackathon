#!/usr/bin/env python3
"""
Builds extra_fixtures/ : small, hand-checkable cases in the layout of fixtures/.

Every input file and every expected table below is a literal written by hand
from rules.md (the comment above each case is the derivation). Nothing here is
computed by an implementation: checks/pyref.py and the pipeline are then run
AGAINST these files (checks/pyref_fixtures.py, scripts/run_fixtures.py).
Only three inputs are written by a loop, because they are long and regular
(XV3, XB4 and the zero padding in XT4 / XV1); their pattern is stated in place.

Usage: python checks/make_extra_fixtures.py        (rewrites extra_fixtures/)

Bytes matter: files are written in binary mode, lines end in \\n only, and a
\\r or a trailing space in a literal below is test data.
"""
import os
import shutil
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(os.path.dirname(HERE), "extra_fixtures")

EVENTS_HEADER = "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n"
CLICKS_HEADER = "click_id,user_id,campaign_id,ts\n"
LINKS_HEADER = "user_id_a,user_id_b\n"
HEADERS = {"events": EVENTS_HEADER, "clicks": CLICKS_HEADER, "links": LINKS_HEADER}
USER_HEADER = "user_id,day,sessions,events,revenue_cents,top_category\n"
CAMPAIGN_HEADER = "campaign_id,channel,day,sessions,purchasing_sessions,revenue_cents,assisted_sessions\n"

PRODUCTS = "product_id,category\np1,shoes\np2,books\n"
CAMPAIGNS = "campaign_id,channel\nc_1,search\nc_2,social\n"
CAMPAIGNS_3 = "campaign_id,channel\nc_1,search\nc_2,social\nc_3,email\n"


def manifest(start, end):
    """The generator's own layout of manifest.json."""
    return '{\n  "window_start": "%s",\n  "window_end": "%s"\n}\n' % (start, end)


MANIFEST = manifest("2026-10-01T00:00:00Z", "2026-10-15T00:00:00Z")

CASES = []
GROUPS = []     # (title, intro) in README order


def group(title, intro):
    GROUPS.append((title, intro))


def text(literal):
    """A literal that starts on the line after the opening quotes."""
    assert literal.startswith("\n"), literal[:20]
    return literal[1:]


class Raw(bytes):
    """A whole file given byte for byte (no header is added)."""


def case(name, summary, catches, events=None, clicks=None, links=None, user_daily="", campaign_daily="",
         products=PRODUCTS, campaigns=CAMPAIGNS, manifest_json=MANIFEST, other_files=None):
    """events / clicks / links: {file name: body}; the header row is added unless the body is Raw."""
    files = {"manifest.json": manifest_json.encode("utf-8"),
             "products.csv": products.encode("utf-8"),
             "campaigns.csv": campaigns.encode("utf-8")}
    for folder, given in (("events", events), ("clicks", clicks), ("links", links)):
        for file_name, body in (given or {"part-00000.csv": ""}).items():
            if isinstance(body, Raw):
                content = bytes(body)
            else:
                content = (HEADERS[folder] + (text(body) if body else "")).encode("utf-8")
            files[f"{folder}/{file_name}"] = content
    for path, content in (other_files or {}).items():
        files[path] = content
    expected = {"user_daily.csv": (USER_HEADER + (text(user_daily) if user_daily else "")).encode("utf-8"),
                "campaign_daily.csv": (CAMPAIGN_HEADER + (text(campaign_daily) if campaign_daily else "")
                                       ).encode("utf-8")}
    CASES.append({"name": name, "group": GROUPS[-1][0], "summary": " ".join(summary.split()),
                  "catches": " ".join(catches.split()), "files": files, "expected": expected})


# ==========================================================================
group("Linked IDs",
      "One session per person at Oct 3 10:00 unless noted.")
# ==========================================================================

# XL1. u1..u8 and U0 are one person: the lines form the chain u1-u2-...-u8, closed into a
# cycle by `u8,u1`, with `u4,u5` and `u2,u1` repeating earlier lines the other way round
# and `u3,u3` a self-link. `U0` is in no event or click and is the byte-wise smallest
# (0x55 < 0x75), so it names the person. u9 is in no link.
#   U0: events 10:00, 10:25, 10:50, 11:15, 11:40, 12:05, 12:30, 12:55 (one per ID): every
#       gap is 25 min, so 1 session of 8 events starting 10:00; revenue 800 (p2 = books).
#       Any ID left out of the person splits it (a 50 min gap).
#       Clicks: k1 (u6, c_1, 09:00), k2 (u3, c_2, 09:30): latest is k2 -> c_2; both assist.
#   u9: 1 session, 1 event, no click -> organic.
case("XL1",
     """Links `u7,u8` `u5,u6` `u3,u4` `u1,u2`, then `u7,u6` `u2,u3` `u5,u4` `u4,u5` `u3,u3`, then
     `u8,u1` `u8,U0` `u2,u1`, in three files. `u1` to `u8` each have one event, 25 min apart (10:00 to
     12:55, the last a purchase of p2 for 800); clicks by `u6` (c_1, 09:00) and `u3` (c_2, 09:30).
     `u9` is unlinked""",
     """A chain 9 IDs long given out of order, in both directions and across files, with a cycle,
     repeats and a self-link, is one person; its session exists only when all 8 IDs are merged;
     the never-used upper-case `U0` names it; clicks from two of its IDs compete""",
     links={"part-00000.csv": '''
u7,u8
u5,u6
u3,u4
u1,u2
''', "part-00001.csv": '''
u7,u6
u2,u3
u5,u4
u4,u5
u3,u3
''', "part-00002.csv": '''
u8,u1
u8,U0
u2,u1
'''},
     events={"part-00000.csv": '''
e1,u1,2026-10-03T10:00:00Z,2026-10-03T14:00:00Z,view,p1,
e2,u2,2026-10-03T10:25:00Z,2026-10-03T14:00:00Z,view,p1,
e3,u3,2026-10-03T10:50:00Z,2026-10-03T14:00:00Z,view,p1,
e4,u4,2026-10-03T11:15:00Z,2026-10-03T14:00:00Z,view,p1,
e5,u5,2026-10-03T11:40:00Z,2026-10-03T14:00:00Z,view,p1,
e6,u6,2026-10-03T12:05:00Z,2026-10-03T14:00:00Z,view,p1,
e7,u7,2026-10-03T12:30:00Z,2026-10-03T14:00:00Z,view,p1,
e8,u8,2026-10-03T12:55:00Z,2026-10-03T14:00:00Z,purchase,p2,800
e9,u9,2026-10-03T10:00:00Z,2026-10-03T14:00:00Z,view,p1,
'''},
     clicks={"part-00000.csv": '''
k1,u6,c_1,2026-10-03T09:00:00Z
k2,u3,c_2,2026-10-03T09:30:00Z
'''},
     user_daily='''
U0,2026-10-03,1,8,800,books
u9,2026-10-03,1,1,0,
''',
     campaign_daily='''
c_1,search,2026-10-03,0,0,0,1
c_2,social,2026-10-03,1,1,800,1
organic,organic,2026-10-03,1,0,0,0
''')

# XL2. File 0 makes {u5,u6}, {u1,u9} and {u7,u8}; file 1's `u9,u6` merges the first two
# into {u1,u5,u6,u9}, named u1 (never used in a row). {u7,u8} is named u7.
#   u1: e1 (u6, 10:00), e2 (u5, 10:10), e3 (u9, 10:20, purchase p1 300): 1 session, 3 events,
#       300, shoes. Clicks k1 (u9, c_1, 09:00), k2 (u5, c_2, 09:30): credit c_2, both assist.
#   u7: e4 (u8, 10:00): 1 session, 1 event. Click k3 (u7, c_1, 09:45): credit c_1.
#   c_1: 1 credited (u7), assists 2 (both sessions). c_2: 1 credited with the purchase, assists 1.
case("XL2",
     """Links `u5,u6`, `u1,u9`, `u7,u8` in one file and `u9,u6` in a later one. Events by `u6`
     (10:00), `u5` (10:10), `u9` (10:20, a purchase of p1 for 300) and `u8` (10:00); clicks by `u9`
     (c_1, 09:00), `u5` (c_2, 09:30) and `u7` (c_1, 09:45)""",
     """Two groups merged late by one link become one person, renamed to the smallest ID of the
     merged group (`u1`, used in no row); a name fixed before the last link is read is stale""",
     links={"part-00000.csv": '''
u5,u6
u1,u9
u7,u8
''', "part-00001.csv": '''
u9,u6
'''},
     events={"part-00000.csv": '''
e1,u6,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e2,u5,2026-10-03T10:10:00Z,2026-10-03T12:00:00Z,view,p1,
e3,u9,2026-10-03T10:20:00Z,2026-10-03T12:00:00Z,purchase,p1,300
e4,u8,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p2,
'''},
     clicks={"part-00000.csv": '''
k1,u9,c_1,2026-10-03T09:00:00Z
k2,u5,c_2,2026-10-03T09:30:00Z
k3,u7,c_1,2026-10-03T09:45:00Z
'''},
     user_daily='''
u1,2026-10-03,1,3,300,shoes
u7,2026-10-03,1,1,0,
''',
     campaign_daily='''
c_1,search,2026-10-03,1,0,0,2
c_2,social,2026-10-03,1,1,300,1
''')

# XL3. IDs compare exactly. `U1,u2` links U1 (not u1) to u2. ` u3,u3` links " u3" (leading
# space) to "u3"; the smallest is " u3" (0x20 sorts first). `u4 ,u5` links "u4 " (trailing
# space) to u5; "u4 " < "u5" because '4' < '5'. "u4" itself is in no link.
# `"",u1` and `u4,""` have an empty side once the quotes are removed: dropped. (Kept, they
# would join u1 and u4 through an empty ID.)
#   u1   : e1                       -> 1 session, 1 event; click k1 (u1, c_1, 09:00) -> c_1
#   U1   : e2 (u2 10:00), e3 (U1 10:10) -> 1 session, 2 events -> organic
#   " u3": e4 (u3)                  -> 1 session, 1 event; click k2 (u3, c_2, 09:00) -> c_2
#   u4   : e5                       -> 1 session, 1 event -> organic
#   "u4 ": e6 (u5 10:00), e7 ("u4 " 10:10) -> 1 session, 2 events -> organic
case("XL3",
     '''Links `U1,u2`, ` u3,u3` (a leading space), `u4 ,u5` (a trailing space), and `"",u1` and
     `u4,""` (a quoted empty side). Events by `u1`, `u2`, `U1` (10:10), `u3`, `u4`, `u5` and `u4 `
     (10:10); clicks by `u1` (c_1) and `u3` (c_2)''',
     '''IDs in links differ by case and by spaces: `U1` is not `u1`, ` u3` is not `u3` but is linked
     to it and names the pair, `u4 ` is not `u4`; five people, three of them named by an ID with a
     capital or a space. A side that is empty once unquoted drops the line; it is not an ID that
     joins `u1` and `u4`''',
     links={"part-00000.csv": '''
U1,u2
 u3,u3
u4 ,u5
"",u1
u4,""
'''},
     events={"part-00000.csv": '''
e1,u1,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e2,u2,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e3,U1,2026-10-03T10:10:00Z,2026-10-03T12:00:00Z,view,p1,
e4,u3,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e5,u4,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e6,u5,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e7,u4 ,2026-10-03T10:10:00Z,2026-10-03T12:00:00Z,view,p1,
'''},
     clicks={"part-00000.csv": '''
k1,u1,c_1,2026-10-03T09:00:00Z
k2,u3,c_2,2026-10-03T09:00:00Z
'''},
     user_daily='''
 u3,2026-10-03,1,1,0,
U1,2026-10-03,1,2,0,
u1,2026-10-03,1,1,0,
u4,2026-10-03,1,1,0,
u4 ,2026-10-03,1,2,0,
''',
     campaign_daily='''
c_1,search,2026-10-03,1,0,0,1
c_2,social,2026-10-03,1,0,0,1
organic,organic,2026-10-03,3,0,0,0
''')

# XL4. A links file whose data lines end \r\n (the header does not). The \r belongs to the
# last field:
#   `u1,u2\r`      links u1 to "u2\r", not to u2      -> u1 and u2 stay two people
#   `u4,U3\r`      links u4 to "U3\r"; "U3\r" < "u4"  -> the person is named "U3\r"; U3 is another
#   `u5,"u6"\r`    the last field is `"u6"\r`: a quote that is not around the whole field
#                  -> malformed, dropped              -> u5 and u6 stay two people
#   `"u7","u8"`    (plain \n) links u7 and u8         -> one person, u7
# One event per ID u1, u2, u4, U3, u5, u6, u7, u8: seven people, u7 with 2 events (10:00, 10:10).
# The expected file wraps "U3\r" in quotes so that CSV readers keep its \r.
case("XL4",
     """A links file with `\\r\\n` line ends: `u1,u2`, `u4,U3`, `u5,"u6"`, and then `"u7","u8"`
     ending in a plain `\\n`. One event each by `u1`, `u2`, `u4`, `U3`, `u5`, `u6`, `u7`, `u8`""",
     """`\\r` is part of the last field of a link too: `u2\\r` is not `u2`, `U3\\r` is a real ID and
     names its person, and a quoted last field followed by `\\r` makes the line malformed""",
     links={"part-00000.csv": Raw(b'user_id_a,user_id_b\n'
                                  b'u1,u2\r\n'
                                  b'u4,U3\r\n'
                                  b'u5,"u6"\r\n'
                                  b'"u7","u8"\n')},
     events={"part-00000.csv": '''
e1,u1,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e2,u2,2026-10-03T10:10:00Z,2026-10-03T12:00:00Z,view,p1,
e3,u4,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e4,U3,2026-10-03T10:10:00Z,2026-10-03T12:00:00Z,view,p1,
e5,u5,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e6,u6,2026-10-03T10:10:00Z,2026-10-03T12:00:00Z,view,p1,
e7,u7,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e8,u8,2026-10-03T10:10:00Z,2026-10-03T12:00:00Z,view,p1,
'''},
     user_daily='''
U3,2026-10-03,1,1,0,
"U3\r",2026-10-03,1,1,0,
u1,2026-10-03,1,1,0,
u2,2026-10-03,1,1,0,
u5,2026-10-03,1,1,0,
u6,2026-10-03,1,1,0,
u7,2026-10-03,1,2,0,
''',
     campaign_daily='''
organic,organic,2026-10-03,7,0,0,0
''')

# XL5. The name is the byte-wise smallest ID, nothing else:
#   {u9,u10}  -> u10  ('1' < '9': not the shorter one, not the smaller number)
#   {ub,uB}   -> uB   ('B' 0x42 < 'b' 0x62)
#   {ua1,u_1} -> u_1  ('_' 0x5F < 'a' 0x61; but 'A' < '_')
#   {u1a,u1}  -> u1   (a prefix sorts before the longer ID)
#   {uA1,u_2} -> uA1  ('A' 0x41 < '_' 0x5F)
# One event each, by the ID that is NOT the name.
case("XL5",
     """Links `u9,u10`, `ub,uB`, `ua1,u_1`, `u1a,u1`, `u_2,uA1`. One event each by `u9`, `ub`, `ua1`,
     `u1a` and `u_2`""",
     """Byte order names the person: `u10` before `u9`, `uB` before `ub`, `u_1` before `ua1`,
     `u1` before `u1a`, `uA1` before `u_2`. Not length, not numeric value, not case-insensitive""",
     links={"part-00000.csv": '''
u9,u10
ub,uB
ua1,u_1
u1a,u1
u_2,uA1
'''},
     events={"part-00000.csv": '''
e1,u9,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e2,ub,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e3,ua1,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e4,u1a,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e5,u_2,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
'''},
     user_daily='''
u1,2026-10-03,1,1,0,
u10,2026-10-03,1,1,0,
uA1,2026-10-03,1,1,0,
uB,2026-10-03,1,1,0,
u_1,2026-10-03,1,1,0,
''',
     campaign_daily='''
organic,organic,2026-10-03,5,0,0,0
''')

# XL6. IDs outside ASCII, compared as UTF-8 bytes. é = C3 A9, ü = C3 BC, ñ = C3 B1.
#   {é1,z9} -> z9   ('z' 0x7A < 0xC3: every ASCII byte sorts before every non-ASCII one;
#                    a signed-byte comparison gets this backwards)
#   {ü2,é3} -> é3   (C3 A9 < C3 BC)
#   ñ4 is unlinked and names itself.
case("XL6",
     """Links `é1,z9` and `ü2,é3`. One event each by `é1`, `ü2` and the unlinked `ñ4`""",
     """Non-ASCII IDs are bytes like any other: `z9` sorts before `é1` (unsigned bytes), `é3` before
     `ü2`, and `ñ4` passes through to the output unchanged""",
     links={"part-00000.csv": '''
é1,z9
ü2,é3
'''},
     events={"part-00000.csv": '''
e1,é1,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e2,ü2,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e3,ñ4,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
'''},
     user_daily='''
z9,2026-10-03,1,1,0,
é3,2026-10-03,1,1,0,
ñ4,2026-10-03,1,1,0,
''',
     campaign_daily='''
organic,organic,2026-10-03,3,0,0,0
''')

# ==========================================================================
group("Duplicates",
      "One user, `u1`, on Oct 3 unless noted. The copy ingested first is usually not the one listed first.")
# ==========================================================================

# XD1. Dedup keeps the copy with the smallest ingest_ts and only then validates it.
#   e1: kept copy (ingest 10:01) has type `Purchase`                 -> dropped
#   e2: kept copy (10:02) has an empty user_id                       -> dropped
#   e3: kept copy (10:03) is a purchase priced 0                     -> dropped
#   e4: copies (ts Oct 4 10:00, ingest Oct 4 10:05:00) and (ts Oct 3 10:00:00, ingest
#       Oct 4 10:00:01). The second has the smaller ingest_ts, is valid, and is late by
#       24 h + 1 s                                                   -> dropped (no Oct 4 row)
#   e5: kept copy (10:04) has an empty product_id                    -> dropped
#   e6: kept copy (10:04:30) has ts 2026-10-15T00:00:00Z = window_end -> dropped
#   e7: kept copy (10:06) is a view at 10:05; the later copy is a purchase of 900 -> a view
#   e8: a view at 10:06
# Survivors: e7 (10:05), e8 (10:06): 1 session, 2 events, no revenue.
case("XD1",
     """Six events whose earliest copy is invalid and whose later copy is valid: event type
     `Purchase`; empty user; purchase priced `0`; late by 24 h + 1 s (its later copy has another `ts`,
     on Oct 4, and is on time); empty product; `ts` at the window's end. `e7`: a view, re-sent later
     as a purchase of 900. `e8`: one view""",
     """Dedup comes before every kind of validation and before the late rule: the earliest copy
     kills the event however it is invalid, and a later copy never replaces any of its values""",
     events={"part-00000.csv": '''
e1,u1,2026-10-03T10:00:00Z,2026-10-03T10:05:00Z,purchase,p1,100
e1,u1,2026-10-03T10:00:00Z,2026-10-03T10:01:00Z,Purchase,p1,100
e2,u1,2026-10-03T10:01:00Z,2026-10-03T10:05:00Z,view,p1,
e2,,2026-10-03T10:01:00Z,2026-10-03T10:02:00Z,view,p1,
e3,u1,2026-10-03T10:02:00Z,2026-10-03T10:05:00Z,purchase,p1,500
e3,u1,2026-10-03T10:02:00Z,2026-10-03T10:03:00Z,purchase,p1,0
e4,u1,2026-10-04T10:00:00Z,2026-10-04T10:05:00Z,view,p1,
e4,u1,2026-10-03T10:00:00Z,2026-10-04T10:00:01Z,view,p1,
e5,u1,2026-10-03T10:03:00Z,2026-10-03T10:05:00Z,view,p1,
e5,u1,2026-10-03T10:03:00Z,2026-10-03T10:04:00Z,view,,
e6,u1,2026-10-03T10:04:00Z,2026-10-03T10:05:00Z,view,p1,
e6,u1,2026-10-15T00:00:00Z,2026-10-03T10:04:30Z,view,p1,
e7,u1,2026-10-03T10:05:00Z,2026-10-03T10:07:00Z,purchase,p1,900
e7,u1,2026-10-03T10:05:00Z,2026-10-03T10:06:00Z,view,p1,
e8,u1,2026-10-03T10:06:00Z,2026-10-03T10:07:00Z,view,p2,
'''},
     user_daily='''
u1,2026-10-03,1,2,0,
''',
     campaign_daily='''
organic,organic,2026-10-03,1,0,0,0
''')

# XD2. Copies spread over two files.
#   e1: copies by u2 (ingest 10:09), u3 (10:05) in file 0 and by u1 (10:01) in file 1 -> u1's.
#   e2: the earliest line (ingest 10:01, user u9) has 6 fields: malformed, never a copy.
#       Of the rest, u1's (10:12, file 1) beats u2's (10:13, file 0).
#   e3: the earliest line (10:01) has the user `u"9`: malformed. Left: u1's purchase of p2
#       for 700 at 10:20.
#   e4: two copies with the same ingest_ts and the same values, one in each file, one of
#       them with every field quoted -> one event (a cart at 10:30).
#   e5: two identical copies (ingest 10:41, one per file) and a later one by u2 (10:45) -> u1's.
# u1: 10:00, 10:10, 10:20 (purchase 700 books), 10:30, 10:40 -> 1 session, 5 events.
# u2, u3 and u9 keep nothing.
case("XD2",
     """Two event files. `e1`: copies by `u2` (ingested 10:09), `u3` (10:05) and, in the other file,
     `u1` (10:01). `e2` and `e3`: the earliest copy is malformed (6 fields; a quote inside the user),
     the next one is `u1`'s. `e4`: two copies with equal `ingest_ts`, one fully quoted, one per file.
     `e5`: two equal copies and a later one by `u2`""",
     """The earliest copy wins across files and users; a malformed line is never the kept copy;
     copies with equal `ingest_ts` are one event however they are quoted""",
     events={"part-00000.csv": '''
e1,u2,2026-10-03T10:00:00Z,2026-10-03T10:09:00Z,view,p1,
e1,u3,2026-10-03T10:00:00Z,2026-10-03T10:05:00Z,view,p1,
e2,u9,2026-10-03T10:10:00Z,2026-10-03T10:01:00Z,view,p1
e2,u2,2026-10-03T10:10:00Z,2026-10-03T10:13:00Z,view,p1,
e3,u"9,2026-10-03T10:20:00Z,2026-10-03T10:01:00Z,view,p1,
e4,u1,2026-10-03T10:30:00Z,2026-10-03T10:31:00Z,cart,p1,
e5,u1,2026-10-03T10:40:00Z,2026-10-03T10:41:00Z,view,p2,
e5,u2,2026-10-03T10:40:00Z,2026-10-03T10:45:00Z,view,p2,
''', "part-00001.csv": '''
e1,u1,2026-10-03T10:00:00Z,2026-10-03T10:01:00Z,view,p1,
e2,u1,2026-10-03T10:10:00Z,2026-10-03T10:12:00Z,view,p1,
e3,u1,2026-10-03T10:20:00Z,2026-10-03T10:24:00Z,purchase,p2,700
"e4","u1","2026-10-03T10:30:00Z","2026-10-03T10:31:00Z","cart","p1",""
e5,u1,2026-10-03T10:40:00Z,2026-10-03T10:41:00Z,view,p2,
'''},
     user_daily='''
u1,2026-10-03,1,5,700,books
''',
     campaign_daily='''
organic,organic,2026-10-03,1,1,700,0
''')

# XD3. Every value of an event comes from its earliest copy.
#   e1: kept (ingest 10:01): u1, Oct 3 10:00, purchase p1 400. The later copy says u2,
#       Oct 4 09:00, p2, 900.
#   e2: kept (ingest 11:03): u2, a view at Oct 3 11:00. The later copy says u1, purchase 50.
#   u1: session Oct 3 10:00, 400, shoes; click k1 (c_1, 09:30) -> c_1.
#   u2: session Oct 3 11:00, a view;     click k2 (c_2, 10:30) -> c_2.
case("XD3",
     """`e1`: ingested 10:01 as `u1`'s purchase of p1 for 400 at Oct 3 10:00, and 10:02 as `u2`'s
     purchase of p2 for 900 at Oct 4 09:00. `e2`: ingested 11:03 as `u2`'s view at 11:00, and 11:04
     as `u1`'s purchase for 50. Clicks: `u1` c_1 09:30, `u2` c_2 10:30""",
     """User, time, type, product and price all come from the earliest copy, so the day, the
     person, the revenue, the category and the credited campaign do too""",
     events={"part-00000.csv": '''
e1,u2,2026-10-04T09:00:00Z,2026-10-03T10:02:00Z,purchase,p2,900
e1,u1,2026-10-03T10:00:00Z,2026-10-03T10:01:00Z,purchase,p1,400
e2,u1,2026-10-03T11:00:00Z,2026-10-03T11:04:00Z,purchase,p1,50
e2,u2,2026-10-03T11:00:00Z,2026-10-03T11:03:00Z,view,p1,
'''},
     clicks={"part-00000.csv": '''
k1,u1,c_1,2026-10-03T09:30:00Z
k2,u2,c_2,2026-10-03T10:30:00Z
'''},
     user_daily='''
u1,2026-10-03,1,1,400,shoes
u2,2026-10-03,1,1,0,
''',
     campaign_daily='''
c_1,search,2026-10-03,1,1,400,1
c_2,social,2026-10-03,1,0,0,1
''')

# XD4. k1 (c_1, 09:00) appears three times and k2 (c_2, 08:00) twice, quoted differently,
# in two files. They are two clicks. u1's session (10:00 view, 10:05 purchase p1 250):
# credit k1 -> c_1; c_1 and c_2 assist it once each.
case("XD4",
     """`k1` (c_1, 09:00) three times and `k2` (c_2, 08:00) twice, with different fields quoted, over
     two click files. One session: a view at 10:00 and a purchase of p1 for 250 at 10:05""",
     """Copies of a click are one click: one credited session, and one assist per campaign, not
     one per copy""",
     events={"part-00000.csv": '''
e1,u1,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e2,u1,2026-10-03T10:05:00Z,2026-10-03T12:00:00Z,purchase,p1,250
'''},
     clicks={"part-00000.csv": '''
k1,u1,c_1,2026-10-03T09:00:00Z
"k1","u1","c_1","2026-10-03T09:00:00Z"
k2,u1,c_2,2026-10-03T08:00:00Z
''', "part-00001.csv": '''
"k2",u1,"c_2",2026-10-03T08:00:00Z
k1,"u1",c_1,"2026-10-03T09:00:00Z"
'''},
     user_daily='''
u1,2026-10-03,1,2,250,shoes
''',
     campaign_daily='''
c_1,search,2026-10-03,1,1,250,1
c_2,social,2026-10-03,0,0,0,1
''')

# XD5. Event IDs: `e1` and `"e1"` are the same ID, `E1` is another.
#   e1: copies ingested 10:01:00 (a view at 10:00) and 10:00:30 (quoted ID; a purchase of p2
#       for 600 at 10:10, ingested before its ts) -> the purchase at 10:10 is kept.
#   E1: a view at 10:05.
# u1: 10:05 and 10:10 -> 1 session starting 10:05, 2 events, 600, books.
# Click k1 (c_1) at 10:02: at or before the start (10:05) -> c_1. Had the 10:00 view been
# kept, the session would start at 10:00 and the click would come too late.
case("XD5",
     """`e1`: a view at 10:00 ingested 10:01:00. `"e1"` (quoted): a purchase of p2 for 600 at 10:10
     ingested 10:00:30. `E1`: a view at 10:05. A click for c_1 at 10:02""",
     """Quotes are stripped before dedup (`"e1"` is `e1`) and case is not folded (`E1` is another
     event); the kept copy decides where the session starts, and so whether the click counts""",
     events={"part-00000.csv": '''
e1,u1,2026-10-03T10:00:00Z,2026-10-03T10:01:00Z,view,p1,
E1,u1,2026-10-03T10:05:00Z,2026-10-03T10:06:00Z,view,p1,
"e1",u1,2026-10-03T10:10:00Z,2026-10-03T10:00:30Z,purchase,p2,600
'''},
     clicks={"part-00000.csv": '''
k1,u1,c_1,2026-10-03T10:02:00Z
'''},
     user_daily='''
u1,2026-10-03,1,2,600,books
''',
     campaign_daily='''
c_1,search,2026-10-03,1,1,600,1
''')

# ==========================================================================
group("Session boundaries",
      "Oct 3 unless noted. `H` = Oct 3 10:00:00.000 = `1791021600000`; timestamps with milliseconds are F3.")
# ==========================================================================

# XS1. ua, ub, uc are one person (ua). Times (UTC): e1 ua 10:00:00.000; e2 ub 12:30+02:00 =
# 10:30:00.000; e3 uc 11:00:00.000; e4 ua and e5 ub 11:30:00.001; e6 uc 12:00:00.001;
# e7 ua 12:30:00.002.
# Gaps: 30:00.000, 30:00.000 | 30:00.001 -> new | 0, 30:00.000 | 30:00.001 -> new.
#   S1 = e1,e2,e3 (start 10:00:00.000)   S2 = e4,e5,e6 (start 11:30:00.001, purchase 500)
#   S3 = e7 (start 12:30:00.002)
# The gap is to the previous event, not to the session start: S1 lasts a full hour.
# Alone, each ID would have 3 + 2 + 2 = 7 sessions.
# Clicks: k1 (uc, c_1) at 11:30:00.001 = S2's start -> credits S2 and S3, not S1.
#         k2 (ub, c_2) at 12:30:00.003, 1 ms after S3's start -> credits nothing; c_2 has no row.
case("XS1",
     """`ua`, `ub`, `uc` linked. Events by `ua` 10:00:00.000, `ub` 10:30:00.000 (written
     `12:30:00+02:00`), `uc` 11:00:00.000, `ua` and `ub` (a purchase of p1 for 500) 11:30:00.001,
     `uc` 12:00:00.001, `ua` 12:30:00.002. Clicks by `uc` for c_1 at 11:30:00.001 and by `ub` for
     c_2 at 12:30:00.003""",
     """Sessions exist only across the linked IDs; gaps of exactly 30:00.000 chain three events
     into one hour-long session (the gap is to the previous event, not to the start); 30:00.001
     splits; a click by one ID at the exact start set by another ID's event counts, 1 ms later not""",
     links={"part-00000.csv": '''
ua,ub
uc,ub
'''},
     events={"part-00000.csv": '''
e1,ua,2026-10-03T10:00:00Z,2026-10-03T14:00:00Z,view,p1,
e2,ub,2026-10-03T12:30:00+02:00,2026-10-03T14:00:00Z,view,p1,
e3,uc,1791025200000,2026-10-03T14:00:00Z,cart,p1,
e4,ua,1791027000001,2026-10-03T14:00:00Z,view,p1,
e5,ub,1791027000001,2026-10-03T14:00:00Z,purchase,p1,500
e6,uc,1791028800001,2026-10-03T14:00:00Z,view,p1,
e7,ua,1791030600002,2026-10-03T14:00:00Z,view,p1,
'''},
     clicks={"part-00000.csv": '''
k1,uc,c_1,1791027000001
k2,ub,c_2,1791030600003
'''},
     user_daily='''
ua,2026-10-03,3,7,500,shoes
''',
     campaign_daily='''
c_1,search,2026-10-03,2,1,500,2
organic,organic,2026-10-03,1,0,0,0
''')

# XS2. Dropped events neither start nor bridge a session.
#   e1 10:00 late by 24 h + 1 s -> dropped     e2 10:20 kept
#   e3 10:45 type `View`        -> dropped     e4 11:10 kept
#   e5 11:35 purchase `12.50`   -> dropped     e6 12:00 kept (ingested at its ts)
#   e7 12:25 two copies, the earliest (ingest 12:25:30) without a product -> dropped
#   e8 12:50 kept
# Survivors 10:20, 11:10, 12:00, 12:50: every gap is 50 min -> 4 sessions of 1 event.
# (With the dropped events every gap would be 20 or 25 min: 1 session from 10:00.)
# Click k1 (c_1) at 10:10 is before the first real start (10:20): all 4 sessions -> c_1.
case("XS2",
     """Events every 20 to 25 min from 10:00 to 12:50; every other one is dropped: late by
     24 h + 1 s (10:00), event type `View` (10:45), a purchase priced `12.50` (11:35), an earliest
     copy without a product (12:25). A click for c_1 at 10:10""",
     """Sessions are built from surviving events only: a dropped event does not bridge a 50 min
     gap and does not start a session (the click at 10:10 would otherwise come after the start)""",
     events={"part-00000.csv": '''
e1,u1,2026-10-03T10:00:00Z,2026-10-04T10:00:01Z,view,p1,
e2,u1,2026-10-03T10:20:00Z,2026-10-03T12:00:00Z,view,p1,
e3,u1,2026-10-03T10:45:00Z,2026-10-03T12:00:00Z,View,p1,
e4,u1,2026-10-03T11:10:00Z,2026-10-03T12:00:00Z,view,p1,
e5,u1,2026-10-03T11:35:00Z,2026-10-03T12:00:00Z,purchase,p1,12.50
e6,u1,2026-10-03T12:00:00Z,2026-10-03T12:00:00Z,view,p1,
e7,u1,2026-10-03T12:25:00Z,2026-10-03T12:26:00Z,view,p1,
e7,u1,2026-10-03T12:25:00Z,2026-10-03T12:25:30Z,view,,
e8,u1,2026-10-03T12:50:00Z,2026-10-03T12:51:00Z,view,p1,
'''},
     clicks={"part-00000.csv": '''
k1,u1,c_1,2026-10-03T10:10:00Z
'''},
     user_daily='''
u1,2026-10-03,4,4,0,
''',
     campaign_daily='''
c_1,search,2026-10-03,4,0,0,4
''')

# XS3. UTC times: e1 Oct 3 23:40:00; e2 05:29:59+05:30 = Oct 3 23:59:59; e3 Oct 3
# 19:00-05:00 = Oct 4 00:00:00; e4 Oct 4 00:29:59; e5 Oct 4 10:00+09:00 = Oct 4 01:00:00;
# e6 Oct 4 01:10:00. Gaps: 19:59, 0:01, 29:59 | 30:01 -> new | 10:00.
#   S1 = e1..e4, starts Oct 3 23:40 -> all of it counts on Oct 3: 4 events, books 800 +
#        shoes 300 = 1100, top books.
#   S2 = e5,e6, starts Oct 4 01:00 -> Oct 4: 2 events, shoes 100.
# Clicks: k2 (c_2) at 23:40:00 = S1's start -> S1; k1 (c_1) at 23:50 is inside S1.
#   S1: only k2 qualifies -> c_2, assisted by c_2.
#   S2: k1 (23:50) and k2 (23:40) qualify -> latest k1 -> c_1, assisted by c_1 and c_2.
case("XS3",
     """One session from Oct 3 23:40 to Oct 4 00:29:59: a view, a cart at 23:59:59 (written
     `2026-10-04T05:29:59+05:30`), purchases of p2 for 800 at 00:00:00 (written
     `2026-10-03T19:00:00-05:00`) and of p1 for 300 at 00:29:59. A second from Oct 4 01:00 (written
     `2026-10-04T10:00:00+09:00`, 30 min + 1 s later) with a purchase of p1 for 100. Clicks: c_2 at
     23:40:00, c_1 at 23:50""",
     """Everything in a session that crosses midnight counts on its start day, in both tables:
     events, revenue, the day's top category and the campaign's revenue; the next session, 30:01
     later, is the next day's. Offsets move the written date both ways""",
     events={"part-00000.csv": '''
e1,u1,2026-10-03T23:40:00Z,2026-10-04T00:30:00Z,view,p1,
e2,u1,2026-10-04T05:29:59+05:30,2026-10-04T00:30:00Z,cart,p1,
e3,u1,2026-10-03T19:00:00-05:00,2026-10-04T00:30:00Z,purchase,p2,800
e4,u1,2026-10-04T00:29:59Z,2026-10-04T00:30:00Z,purchase,p1,300
e5,u1,2026-10-04T10:00:00+09:00,2026-10-04T02:00:00Z,view,p1,
e6,u1,2026-10-04T01:10:00Z,2026-10-04T02:00:00Z,purchase,p1,100
'''},
     clicks={"part-00000.csv": '''
k1,u1,c_1,2026-10-03T23:50:00Z
k2,u1,c_2,2026-10-03T23:40:00Z
'''},
     user_daily='''
u1,2026-10-03,1,4,1100,books
u1,2026-10-04,1,2,100,shoes
''',
     campaign_daily='''
c_1,search,2026-10-04,1,1,100,1
c_2,social,2026-10-03,1,1,1100,1
c_2,social,2026-10-04,0,0,0,1
''')

# XS4. The window is [Oct 1 00:00:00.000 = 1790812800000, Oct 15 00:00:00.000 = 1792022400000).
#   ua 1790812800000              = window_start             valid   Oct 1
#   ub 1790812799999              1 ms before                bad
#   uc 1792022399999              Oct 14 23:59:59.999        valid   Oct 14
#   ud 1792022400000              = window_end               bad
#   ue 2026-10-01T02:00:00+02:00  = Oct 1 00:00:00Z          valid   Oct 1
#   uf 2026-10-01T01:59:59+02:00  = Sep 30 23:59:59Z         bad
#   ug 2026-10-14T20:00:00-04:00  = Oct 15 00:00:00Z         bad
#   uh 2026-10-14T19:59:59-04:00  = Oct 14 23:59:59Z         valid   Oct 14
#   ui 2026-09-30T19:00:00-05:00  = Oct 1 00:00:00Z          valid   Oct 1  (written date outside)
#   uj 2026-10-15T09:00:00+09:30  = Oct 14 23:30:00Z         valid   Oct 14 (written date outside)
# Clicks: k1 (ua, c_1) 2026-09-24T02:00:00+02:00 = Sep 24 00:00:00Z = window_start - 7 days:
#            in the click window and exactly 7 days before ua's start -> c_1.
#         k2 (ue, c_2) 1790207999999 = 1 ms earlier -> bad -> ue is organic.
#         k3 (uc, c_2) 1792022399999 = uc's start, inside the window -> c_2.
#         k4 (uh, c_1) 2026-10-14T20:00:00-04:00 = window_end -> bad -> uh is organic.
case("XS4",
     """One event per user at the window's edges, in F3 and F2: `1790812800000` (the first instant),
     1 ms before it, `1792022399999` (the last millisecond), `1792022400000` (the end);
     `2026-10-01T02:00:00+02:00` and `2026-09-30T19:00:00-05:00` (both the first instant),
     `2026-10-01T01:59:59+02:00`, `2026-10-14T20:00:00-04:00` (the end), `2026-10-14T19:59:59-04:00`,
     `2026-10-15T09:00:00+09:30`. Clicks at `window_start - 7 days` written with an offset, 1 ms
     before that, at the last millisecond, and at the end""",
     """The window is tested on the UTC instant, to the millisecond, whatever date the text shows;
     the click window's edges likewise""",
     events={"part-00000.csv": '''
e1,ua,1790812800000,2026-10-01T00:01:00Z,view,p1,
e2,ub,1790812799999,2026-10-01T00:01:00Z,view,p1,
e3,uc,1792022399999,2026-10-15T00:01:00Z,view,p1,
e4,ud,1792022400000,2026-10-15T00:01:00Z,view,p1,
e5,ue,2026-10-01T02:00:00+02:00,2026-10-01T00:01:00Z,view,p1,
e6,uf,2026-10-01T01:59:59+02:00,2026-10-01T00:01:00Z,view,p1,
e7,ug,2026-10-14T20:00:00-04:00,2026-10-15T00:01:00Z,view,p1,
e8,uh,2026-10-14T19:59:59-04:00,2026-10-15T00:01:00Z,view,p1,
e9,ui,2026-09-30T19:00:00-05:00,2026-10-01T00:01:00Z,view,p1,
e10,uj,2026-10-15T09:00:00+09:30,2026-10-15T00:01:00Z,view,p1,
'''},
     clicks={"part-00000.csv": '''
k1,ua,c_1,2026-09-24T02:00:00+02:00
k2,ue,c_2,1790207999999
k3,uc,c_2,1792022399999
k4,uh,c_1,2026-10-14T20:00:00-04:00
'''},
     user_daily='''
ua,2026-10-01,1,1,0,
uc,2026-10-14,1,1,0,
ue,2026-10-01,1,1,0,
uh,2026-10-14,1,1,0,
ui,2026-10-01,1,1,0,
uj,2026-10-14,1,1,0,
''',
     campaign_daily='''
c_1,search,2026-10-01,1,0,0,1
c_2,social,2026-10-14,1,0,0,1
organic,organic,2026-10-01,2,0,0,0
organic,organic,2026-10-14,2,0,0,0
''')

# XS5. Late means ingest_ts - ts > 24 h, on the UTC instants, to the millisecond.
#   u1  ts 10:00:00Z                ingest Oct 4 10:00:00  = 24 h          kept
#   u2  ts 10:00:00.001 (F3)        ingest Oct 4 10:00:00  = 24 h - 1 ms   kept
#   u3  ts 09:59:59.999 (F3)        ingest Oct 4 10:00:00  = 24 h + 1 ms   dropped
#   u4  ts 12:00+02:00 = 10:00:00Z  ingest Oct 4 10:00:01  = 24 h + 1 s    dropped
#   u5  ts 08:00-02:00 = 10:00:00Z  ingest Oct 4 10:00:00  = 24 h          kept
#   u6  ts Oct 10 10:00             ingest Oct 1 00:00 (9 days BEFORE)     kept, Oct 10
#   u7  ts Oct 3 10:00              ingest Sep 1 (before the window)       kept
#   u8  ts Oct 3 10:00              ingest Oct 8 10:00 (5 days)            dropped
#   u9  ts Oct 14 23:59:59          ingest Oct 15 23:59:59 = 24 h          kept, Oct 14
#   u10 ts Oct 14 23:59:59          ingest Oct 16 00:00:00 = 24 h + 1 s    dropped
case("XS5",
     """One event per user. `ts` Oct 3 10:00:00 ingested exactly 24 h later; `ts` 10:00:00.001 and
     09:59:59.999 (F3) ingested Oct 4 10:00:00; `ts` `12:00:00+02:00` ingested 24 h + 1 s later and
     `08:00:00-02:00` exactly 24 h later; ingested 9 days before its `ts`; ingested before the
     window; ingested 5 days late; `ts` Oct 14 23:59:59 ingested 24 h and 24 h + 1 s later""",
     """The late rule compares UTC instants to the millisecond: 24 h + 1 ms is late, exactly 24 h
     is not, an offset is applied first, an `ingest_ts` before `ts` or outside the window is fine""",
     events={"part-00000.csv": '''
e1,u1,2026-10-03T10:00:00Z,2026-10-04T10:00:00Z,view,p1,
e2,u2,1791021600001,2026-10-04T10:00:00Z,view,p1,
e3,u3,1791021599999,2026-10-04T10:00:00Z,view,p1,
e4,u4,2026-10-03T12:00:00+02:00,2026-10-04T10:00:01Z,view,p1,
e5,u5,2026-10-03T08:00:00-02:00,2026-10-04T10:00:00Z,view,p1,
e6,u6,2026-10-10T10:00:00Z,2026-10-01T00:00:00Z,view,p1,
e7,u7,2026-10-03T10:00:00Z,2026-09-01T00:00:00Z,view,p1,
e8,u8,2026-10-03T10:00:00Z,2026-10-08T10:00:00Z,view,p1,
e9,u9,2026-10-14T23:59:59Z,2026-10-15T23:59:59Z,view,p1,
e10,u10,2026-10-14T23:59:59Z,2026-10-16T00:00:00Z,view,p1,
'''},
     user_daily='''
u1,2026-10-03,1,1,0,
u2,2026-10-03,1,1,0,
u5,2026-10-03,1,1,0,
u6,2026-10-10,1,1,0,
u7,2026-10-03,1,1,0,
u9,2026-10-14,1,1,0,
''',
     campaign_daily='''
organic,organic,2026-10-03,4,0,0,0
organic,organic,2026-10-10,1,0,0,0
organic,organic,2026-10-14,1,0,0,0
''')

# ==========================================================================
group("Half-hour buckets and assist intervals",
      "Aimed at an engine that summarizes events per 30-minute bucket and counts assists by merged "
      "`[t, t + 7 days]` intervals. `G` = 30 min = 1,800,000 ms.")
# ==========================================================================

# XB1. With the window starting at midnight, 30-minute buckets counted from window_start
# have their edges at every UTC :00 and :30. H = 1791021600000 = Oct 3 10:00:00.000.
#   ua  H, H+G (10:00:00.000, 10:30:00.000): bucket starts, gap exactly G      -> 1 session
#   ub  H, H+2G-1 (10:00:00.000, 10:59:59.999): ADJACENT buckets, gap 59:59.999 -> 2 sessions
#   uc  H+G-1, H+2G (10:29:59.999, 11:00:00.000): one bucket skipped; the gap is G + 1,
#       the smallest a skipped bucket allows                                   -> 2 sessions
#   ud  H+G-1, H+2G-1, H+3G-1 (bucket ends, gaps exactly G), then H+4G = 12:00:00.000
#       (a purchase of p2 for 500; gap G + 1 over a skipped bucket) -> sessions of 3 and 1
#   ue  two events at 10:29:59.999, three at 10:30:00.000 (one a purchase of p1 for 700),
#       three at 11:00:00.000 (gap G), two at 11:30:00.001 (gap G + 1; one a purchase of p2
#       for 300) -> sessions of 8 and 2; the day: shoes 700, books 300
#   uf  H, H+G-1: the two ends of one bucket                                   -> 1 session
#   ug  10:05, 10:35, 11:05: three buckets, gaps exactly G                     -> 1 session
#   uh  written in the order 10:29:00, 10:01:00, 10:58:00, 10:31:00, 11:29:00, 11:28:00.
#       Sorted: 10:01, 10:29, 10:31, 10:58, 11:28, 11:29; gaps 28:00, 2:00, 27:00, 30:00,
#       1:00 -> 1 session of 6 starting 10:01:00. A bucket's min and max are not its first
#       and last line: taking 10:01 as the first bucket's max, or 11:29 as the third's min,
#       splits the session; taking 10:29 as the start lets the click below count.
# Clicks: k1 (uh, c_1) at 10:15:00, after uh's start (10:01) -> no credit, no c_1 row.
#         k2 (ud, c_2) at 12:00:00.000 = the start of ud's second session -> c_2 (500).
# organic: 1 + 2 + 2 + 1 (ud's first) + 2 + 1 + 1 + 1 = 11 sessions, 2 with purchases (ue's), 1000.
case("XB1",
     """Events on 30-minute boundaries (`H`+k`G`, i.e. every :00 and :30). `ua`: `H`, `H`+`G`. `ub`:
     `H`, `H`+2`G`-1. `uc`: `H`+`G`-1, `H`+2`G`. `ud`: `H`+`G`-1, `H`+2`G`-1, `H`+3`G`-1, then a purchase
     at `H`+4`G`. `ue`: two events at 10:29:59.999, three at 10:30:00.000, three at 11:00:00.000, two
     at 11:30:00.001, two of the ten purchases. `uf`: `H`, `H`+`G`-1. `ug`: 10:05, 10:35, 11:05. `uh`:
     six events in three buckets, written out of time order. Clicks at 10:15 (`uh`) and at `H`+4`G`
     (`ud`)""",
     """Bucket edges are not session edges: a gap of exactly `G` joins adjacent buckets whether
     the events sit at bucket starts or ends; adjacent buckets still split when the real gap is
     over `G`; the smallest gap over a skipped bucket is `G`+1 and splits; equal timestamps on both
     sides of an edge stay together; a bucket's min and max are not its first and last line""",
     events={"part-00000.csv": '''
e1,ua,1791021600000,2026-10-03T14:00:00Z,view,p1,
e2,ua,1791023400000,2026-10-03T14:00:00Z,view,p1,
e3,ub,1791021600000,2026-10-03T14:00:00Z,view,p1,
e4,ub,1791025199999,2026-10-03T14:00:00Z,view,p1,
e5,uc,1791023399999,2026-10-03T14:00:00Z,view,p1,
e6,uc,1791025200000,2026-10-03T14:00:00Z,view,p1,
e7,ud,1791023399999,2026-10-03T14:00:00Z,view,p1,
e8,ud,1791025199999,2026-10-03T14:00:00Z,view,p1,
e9,ud,1791026999999,2026-10-03T14:00:00Z,view,p1,
e10,ud,1791028800000,2026-10-03T14:00:00Z,purchase,p2,500
e11,ue,1791023399999,2026-10-03T14:00:00Z,view,p1,
e12,ue,1791023400000,2026-10-03T14:00:00Z,view,p1,
e13,ue,1791025200000,2026-10-03T14:00:00Z,view,p1,
e14,ue,1791027000001,2026-10-03T14:00:00Z,view,p1,
e15,ue,1791023400000,2026-10-03T14:00:00Z,purchase,p1,700
e16,ue,1791023399999,2026-10-03T14:00:00Z,cart,p1,
e17,ue,1791025200000,2026-10-03T14:00:00Z,view,p2,
e18,ue,1791027000001,2026-10-03T14:00:00Z,purchase,p2,300
e19,ue,1791023400000,2026-10-03T14:00:00Z,view,p2,
e20,ue,1791025200000,2026-10-03T14:00:00Z,cart,p2,
e21,uf,1791021600000,2026-10-03T14:00:00Z,view,p1,
e22,uf,1791023399999,2026-10-03T14:00:00Z,view,p1,
e23,ug,2026-10-03T10:05:00Z,2026-10-03T14:00:00Z,view,p1,
e24,ug,2026-10-03T10:35:00Z,2026-10-03T14:00:00Z,view,p1,
e25,ug,2026-10-03T11:05:00Z,2026-10-03T14:00:00Z,view,p1,
e26,uh,2026-10-03T10:29:00Z,2026-10-03T14:00:00Z,view,p1,
e27,uh,2026-10-03T10:01:00Z,2026-10-03T14:00:00Z,view,p1,
e28,uh,2026-10-03T10:58:00Z,2026-10-03T14:00:00Z,view,p1,
e29,uh,2026-10-03T10:31:00Z,2026-10-03T14:00:00Z,view,p1,
e30,uh,2026-10-03T11:29:00Z,2026-10-03T14:00:00Z,view,p1,
e31,uh,2026-10-03T11:28:00Z,2026-10-03T14:00:00Z,view,p1,
'''},
     clicks={"part-00000.csv": '''
k1,uh,c_1,2026-10-03T10:15:00Z
k2,ud,c_2,1791028800000
'''},
     user_daily='''
ua,2026-10-03,1,2,0,
ub,2026-10-03,2,2,0,
uc,2026-10-03,2,2,0,
ud,2026-10-03,2,4,500,books
ue,2026-10-03,2,10,1000,shoes
uf,2026-10-03,1,2,0,
ug,2026-10-03,1,3,0,
uh,2026-10-03,1,6,0,
''',
     campaign_daily='''
c_2,social,2026-10-03,1,1,500,1
organic,organic,2026-10-03,11,2,1000,0
''')

# XB2. A window that does not start at midnight: [Oct 1 07:13:20.000 = 1790838800000,
# Oct 15 07:13:20.000 = 1792048400000). Buckets counted from window_start have their edges
# at :13:20 and :43:20; the day is still the UTC date.
#   ua  1790838800000 = window_start                          valid, Oct 1
#   ub  1790838799999                                         bad
#   uc  1792048399999 = Oct 15 07:13:19.999, the last ms      valid, Oct 15 (a 15th UTC date)
#   ud  1792048400000 = window_end                            bad
#   ue  07:13:19Z bad, 07:13:20Z valid                        1 session, 1 event, Oct 1
#   uf  Oct 2 00:10:00Z: less than 24 h after window_start, but its UTC date is Oct 2
#   ug  Oct 2 23:50, Oct 3 00:10 (one session, counted Oct 2) and Oct 3 07:00 (a second,
#       6 h 50 min later, counted Oct 3 although it is still "window day 2")
#   uh  1790849599999 (10:13:19.999, a bucket's last ms), 1790851399999 (10:43:19.999,
#       gap exactly G), 1790853200000 (11:13:20.000, gap G + 1 over a skipped bucket)
#       -> sessions of 2 and 1
#   ui  10:00:00, 10:30:00 (gap exactly G, mid-bucket here), 11:00:01 (gap 30:01)
#       -> sessions of 2 and 1
# Clicks: k1 (ua, c_1) 1790234000000 = Sep 24 07:13:20.000 = window_start - 7 days -> valid
#            and exactly 7 days before ua's start -> c_1.
#         k2 (ue, c_2) 1790233999999, 1 ms earlier -> bad -> ue is organic.
#         k3 (uc, c_2) Oct 15 07:13:19Z, inside the window, before uc's start -> c_2.
#         k5 (ug, c_1) Oct 2 23:50:00Z = ug's first start -> c_1 for both of ug's sessions.
# Oct 1: c_1 1 (ua); organic ue 1 + uh 2 + ui 2 = 5.  Oct 2: c_1 1 (ug); organic 1 (uf).
# Oct 3: c_1 1 (ug).  Oct 15: c_2 1 (uc).
case("XB2",
     """The window is `2026-10-01T07:13:20Z` to `2026-10-15T07:13:20Z`. Events at its first instant,
     1 ms before, its last millisecond (on Oct 15) and its end; at 07:13:19 and 07:13:20; at
     Oct 2 00:10; a session over the Oct 2/3 midnight and another at Oct 3 07:00; events 1 ms
     before 10:13:20, 30 min later, and at 11:13:20.000; events at 10:00:00, 10:30:00, 11:00:01.
     Clicks at `window_start - 7 days`, 1 ms before that, at Oct 15 07:13:19 and at Oct 2 23:50.
     Not something the generator writes: its windows start at midnight""",
     """Nothing may assume a midnight window: the window test and the click window use the real
     instants; the day is the UTC date (15 dates here), not a 24 h slice counted from
     `window_start`; 30-minute buckets counted from `window_start` still split only on real gaps""",
     manifest_json=manifest("2026-10-01T07:13:20Z", "2026-10-15T07:13:20Z"),
     events={"part-00000.csv": '''
e1,ua,1790838800000,2026-10-01T08:00:00Z,view,p1,
e2,ub,1790838799999,2026-10-01T08:00:00Z,view,p1,
e3,uc,1792048399999,2026-10-15T08:00:00Z,view,p1,
e4,ud,1792048400000,2026-10-15T08:00:00Z,view,p1,
e5,ue,2026-10-01T07:13:19Z,2026-10-01T08:00:00Z,view,p1,
e6,ue,2026-10-01T07:13:20Z,2026-10-01T08:00:00Z,view,p1,
e7,uf,2026-10-02T00:10:00Z,2026-10-02T01:00:00Z,view,p1,
e8,ug,2026-10-02T23:50:00Z,2026-10-03T08:00:00Z,view,p1,
e9,ug,2026-10-03T00:10:00Z,2026-10-03T08:00:00Z,view,p1,
e10,ug,2026-10-03T07:00:00Z,2026-10-03T08:00:00Z,view,p1,
e11,uh,1790849599999,2026-10-01T12:00:00Z,view,p1,
e12,uh,1790851399999,2026-10-01T12:00:00Z,view,p1,
e13,uh,1790853200000,2026-10-01T12:00:00Z,view,p1,
e14,ui,2026-10-01T10:00:00Z,2026-10-01T12:00:00Z,view,p1,
e15,ui,2026-10-01T10:30:00Z,2026-10-01T12:00:00Z,view,p1,
e16,ui,2026-10-01T11:00:01Z,2026-10-01T12:00:00Z,view,p1,
'''},
     clicks={"part-00000.csv": '''
k1,ua,c_1,1790234000000
k2,ue,c_2,1790233999999
k3,uc,c_2,2026-10-15T07:13:19Z
k5,ug,c_1,2026-10-02T23:50:00Z
'''},
     user_daily='''
ua,2026-10-01,1,1,0,
uc,2026-10-15,1,1,0,
ue,2026-10-01,1,1,0,
uf,2026-10-02,1,1,0,
ug,2026-10-02,1,2,0,
ug,2026-10-03,1,1,0,
uh,2026-10-01,2,3,0,
ui,2026-10-01,2,3,0,
''',
     campaign_daily='''
c_1,search,2026-10-01,1,0,0,1
c_1,search,2026-10-02,1,0,0,1
c_1,search,2026-10-03,1,0,0,1
c_2,social,2026-10-15,1,0,0,1
organic,organic,2026-10-01,5,0,0,0
organic,organic,2026-10-02,1,0,0,0
''')

# XB3. T = Sep 26 23:59:59.999 = 1790467199999. T + 7 d = Oct 3 23:59:59.999 = 1791071999999;
# 1 ms later is Oct 4 00:00:00.000 = 1791072000000. A click at t assists starts in the
# closed interval [t, t + 7 d].
#   ua  clicks c_1 at T and at T+7d; start T+7d: both intervals hold it -> ONE assist; c_1
#   ub  clicks c_1 at T and at T+7d+1; start T+7d (between them): only the first -> c_1
#   uc  clicks c_1 at T and at T+7d+2; start T+7d+1 (Oct 4): in neither interval -> organic
#   ud  clicks c_1 at T and at T+7d+1; start T+7d+1 (Oct 4): only the second -> c_1
#   ue  clicks c_1 at T and c_2 at T+7d; starts T+7d (both qualify, latest c_2; both assist)
#       and Oct 4 01:00 (only the c_2 click, 1 h 1 ms old) -> c_2 twice
#   uf  click c_2 at Oct 3 00:00:00; starts Oct 10 00:00:00.000 (exactly 7 days: counts,
#       and it is Oct 10's) and Oct 10 00:30:00.001 (too old) -> c_2, then organic
# Oct 3: c_1 credited ua, ub; assists ua, ub, ue = 3. c_2 credited ue; assists 1.
# Oct 4: c_1 credited ud (1 assist). c_2 credited ue's second (1 assist). organic uc.
# Oct 10: c_2 credited uf's first (1 assist). organic uf's second.
case("XB3",
     """`T` = Sep 26 23:59:59.999, so `T`+7d is Oct 3's last millisecond. `ua`: c_1 clicks at `T` and
     `T`+7d, session at `T`+7d. `ub`: c_1 at `T` and `T`+7d+1 ms, session at `T`+7d. `uc`: c_1 at `T`
     and `T`+7d+2 ms, session at `T`+7d+1 ms. `ud`: c_1 at `T` and `T`+7d+1 ms, session at `T`+7d+1 ms.
     `ue`: c_1 at `T`, c_2 at `T`+7d, sessions at `T`+7d and Oct 4 01:00. `uf`: c_2 at Oct 3
     00:00:00, sessions at Oct 10 00:00:00.000 and 00:30:00.001""",
     """Assist intervals `[t, t + 7 days]` are closed at both ends and counted once per campaign:
     two clicks of one campaign exactly 7 days apart share an endpoint (one assist, not two);
     1 ms further apart they do not touch; a 1 ms hole between two intervals stays a hole; an
     interval that ends on a day's last or first millisecond counts on the right day""",
     events={"part-00000.csv": '''
e1,ua,1791071999999,2026-10-04T01:00:00Z,view,p1,
e2,ub,1791071999999,2026-10-04T01:00:00Z,view,p1,
e3,uc,1791072000000,2026-10-04T01:00:00Z,view,p1,
e4,ud,2026-10-04T00:00:00Z,2026-10-04T01:00:00Z,view,p1,
e5,ue,1791071999999,2026-10-04T01:00:00Z,view,p1,
e6,ue,2026-10-04T01:00:00Z,2026-10-04T02:00:00Z,view,p1,
e7,uf,2026-10-10T00:00:00Z,2026-10-10T01:00:00Z,view,p1,
e8,uf,1791592200001,2026-10-10T01:00:00Z,view,p1,
'''},
     clicks={"part-00000.csv": '''
ka1,ua,c_1,1790467199999
ka2,ua,c_1,1791071999999
kb1,ub,c_1,1790467199999
kb2,ub,c_1,1791072000000
kc1,uc,c_1,1790467199999
kc2,uc,c_1,1791072000001
kd1,ud,c_1,1790467199999
kd2,ud,c_1,2026-10-04T00:00:00Z
ke1,ue,c_1,1790467199999
ke2,ue,c_2,1791071999999
kf1,uf,c_2,2026-10-03T00:00:00Z
'''},
     user_daily='''
ua,2026-10-03,1,1,0,
ub,2026-10-03,1,1,0,
uc,2026-10-04,1,1,0,
ud,2026-10-04,1,1,0,
ue,2026-10-03,1,1,0,
ue,2026-10-04,1,1,0,
uf,2026-10-10,2,2,0,
''',
     campaign_daily='''
c_1,search,2026-10-03,2,0,0,3
c_1,search,2026-10-04,1,0,0,1
c_2,social,2026-10-03,1,0,0,1
c_2,social,2026-10-04,1,0,0,1
c_2,social,2026-10-10,1,0,0,1
organic,organic,2026-10-04,1,0,0,0
organic,organic,2026-10-10,1,0,0,0
''')


# XB5. A window whose length is not a multiple of 30 minutes (and not 14 days):
# [Oct 1 07:13:20.000 = 1790838800000, Oct 4 07:30:25.000 = 1791099025000), 3 days 17 min 5 s.
# Counted from window_start it has 144 full 30-minute buckets and a last one of 17 min 5 s
# that starts at Oct 4 07:13:20.000 = 1791098000000. It touches four UTC dates.
#   ua  1791099024999 = Oct 4 07:30:24.999, the last ms (in the short last bucket)  valid, Oct 4
#   ub  1791099025000 = window_end                                                  bad
#   uc  1791097999999 and 1791098000000: the last ms of bucket 143 and the first of the
#       last bucket, 1 ms apart                                -> 1 session, 2 events, Oct 4
#   ud  Oct 1 07:13:20 (window_start) and 07:43:20 (exactly G later) -> 1 session, 2 events, Oct 1
#   ue  Oct 3 23:59:59 and Oct 4 00:00:00                      -> 1 session, 2 events, Oct 3
# Clicks: k1 (ua, c_1) Sep 24 07:13:20 = window_start - 7 days: valid, but about 10 days
#            before ua's start -> no credit.
#         k2 (ua, c_2) Oct 4 07:30:24Z: inside the window, before ua's start -> c_2.
#         k3 (ud, c_1) Sep 24 07:13:20, exactly 7 days before ud's start -> c_1.
case("XB5",
     """The window is `2026-10-01T07:13:20Z` to `2026-10-04T07:30:25Z`: 3 days 17 min 5 s. Events at
     its last millisecond and at its end; 1 ms apart around the start of its last, short
     30-minute slice; at its first instant and 30 min later; around the Oct 3/4 midnight. Clicks
     at `window_start - 7 days` (two users) and 1 s before the window's end. Not something the
     generator writes: its windows are 14 days""",
     """Nothing may assume a 14-day window or a whole number of 30-minute buckets: the last,
     partial bucket holds valid events, and the window's real end is the limit""",
     manifest_json=manifest("2026-10-01T07:13:20Z", "2026-10-04T07:30:25Z"),
     events={"part-00000.csv": '''
e1,ua,1791099024999,2026-10-04T08:00:00Z,view,p1,
e2,ub,1791099025000,2026-10-04T08:00:00Z,view,p1,
e3,uc,1791097999999,2026-10-04T08:00:00Z,view,p1,
e4,uc,1791098000000,2026-10-04T08:00:00Z,view,p1,
e5,ud,2026-10-01T07:13:20Z,2026-10-01T08:00:00Z,view,p1,
e6,ud,2026-10-01T07:43:20Z,2026-10-01T08:00:00Z,view,p1,
e7,ue,2026-10-03T23:59:59Z,2026-10-04T08:00:00Z,view,p1,
e8,ue,2026-10-04T00:00:00Z,2026-10-04T08:00:00Z,view,p1,
'''},
     clicks={"part-00000.csv": '''
k1,ua,c_1,2026-09-24T07:13:20Z
k2,ua,c_2,2026-10-04T07:30:24Z
k3,ud,c_1,2026-09-24T07:13:20Z
'''},
     user_daily='''
ua,2026-10-04,1,1,0,
uc,2026-10-04,1,2,0,
ud,2026-10-01,1,2,0,
ue,2026-10-03,1,2,0,
''',
     campaign_daily='''
c_1,search,2026-10-01,1,0,0,1
c_2,social,2026-10-04,1,0,0,1
organic,organic,2026-10-03,1,0,0,0
organic,organic,2026-10-04,1,0,0,0
''')


# XB4. A heavy person, written by a loop. u1 has one event at every even hour (00:00,
# 02:00, ... 22:00) of each day Oct 1 to Oct 14: 168 events, 2 h apart, so 168 sessions of
# 1 event, 12 per day. The 12:00 event of Oct D is a purchase of p1 for D cents; the rest
# are views.
# Clicks, all by u1 and all on the hour:
#   c_1: every hour from Sep 24 00:00 to Oct 14 23:00: 504 clicks, IDs h0000 .. h0503.
#   c_2: every hour of Oct 5 (IDs g00 .. g23) and of Oct 6 (IDs i00 .. i23): 48 clicks.
# Every click is written twice: once in part-00000.csv, and again in part-00001.csv in
# reverse order with every field quoted (1104 lines for 552 clicks).
# Credit. Every session starts on an even hour, where a c_1 click sits at the same instant,
# so the latest qualifying click is at the start itself. On Oct 5 a c_2 click `gNN` ties
# with it and 'g' < 'h': c_2. On Oct 6 a c_2 click `iNN` ties and 'h' < 'i': c_1.
#   -> Oct 5: all 12 sessions c_2. Every other day: all 12 sessions c_1.
# Assists. c_1 has a click at every session's start: all 168, 12 per day. c_2's clicks run
# from Oct 5 00:00 to Oct 6 23:00, so their intervals cover Oct 5 00:00 .. Oct 13 23:00:
# all 12 sessions of each day Oct 5 .. Oct 13 (the last at Oct 13 22:00), none on Oct 14.
# Revenue D follows the day's 12:00 session to its campaign.
def heavy_person():
    events, clicks = [], []
    number = 0
    for day in range(1, 15):
        for hour in range(0, 24, 2):
            number += 1
            kind, price = ("purchase", str(day)) if hour == 12 else ("view", "")
            events.append(f"e{number},u1,2026-10-{day:02d}T{hour:02d}:00:00Z,2026-10-{day:02d}T23:59:59Z,"
                          f"{kind},p1,{price}\n")
    number = 0
    for month, day in [(9, d) for d in range(24, 31)] + [(10, d) for d in range(1, 15)]:
        for hour in range(24):
            clicks.append((f"h{number:04d}", "u1", "c_1", f"2026-{month:02d}-{day:02d}T{hour:02d}:00:00Z"))
            number += 1
    for letter, day in (("g", 5), ("i", 6)):
        for hour in range(24):
            clicks.append((f"{letter}{hour:02d}", "u1", "c_2", f"2026-10-{day:02d}T{hour:02d}:00:00Z"))
    assert len(events) == 168 and len(clicks) == 552
    plain = "".join(",".join(click) + "\n" for click in clicks)
    quoted = "".join(",".join(f'"{field}"' for field in click) + "\n" for click in reversed(clicks))
    user_rows = "".join(f"u1,2026-10-{day:02d},12,12,{day},shoes\n" for day in range(1, 15))
    campaign_rows = "".join(
        (f"c_1,search,2026-10-{day:02d},0,0,0,12\n" if day == 5 else
         f"c_1,search,2026-10-{day:02d},12,1,{day},12\n") for day in range(1, 15))
    campaign_rows += "c_2,social,2026-10-05,12,1,5,12\n"
    campaign_rows += "".join(f"c_2,social,2026-10-{day:02d},0,0,0,12\n" for day in range(6, 14))
    return "\n" + "".join(events), "\n" + plain, "\n" + quoted, "\n" + user_rows, "\n" + campaign_rows


_events, _plain, _quoted, _user_rows, _campaign_rows = heavy_person()
case("XB4",
     """One person with 168 sessions (an event every 2 hours for 14 days, each day's 12:00 one a
     purchase of p1 for the day's number in cents) and 552 clicks, each written twice (the second
     file reversed and fully quoted): c_1 every hour from Sep 24 00:00 to Oct 14 23:00 (`h0000` to
     `h0503`); c_2 every hour of Oct 5 (`g00` to `g23`) and Oct 6 (`i00` to `i23`). Written by a
     loop in `checks/make_extra_fixtures.py`""",
     """A heavy person: hundreds of overlapping intervals per campaign merge into one, each
     session is assisted once per campaign (12 per day, not 12 times the clicks, not doubled by
     the copies); a same-millisecond tie at every session start goes to `g..` over `h....` on Oct 5
     and to `h....` over `i..` on Oct 6; c_2's coverage ends Oct 13 23:00, so Oct 14 gets none""",
     events={"part-00000.csv": _events},
     clicks={"part-00000.csv": _plain, "part-00001.csv": _quoted},
     user_daily=_user_rows,
     campaign_daily=_campaign_rows)

# ==========================================================================
group("Attribution",
      "One session per user at Oct 3 10:00:00 unless noted.")
# ==========================================================================

# XA1. Every user has clicks for c_1 and c_2 at the same millisecond (09:00:00 unless noted);
# the byte-wise smallest click_id takes the session. Each session is a purchase of p1 with
# its own power-of-two price, so the campaign revenue says exactly which sessions it got.
#   ua (1)   Ka2 c_1 vs ka1 c_2    'K' 0x4B < 'k' 0x6B              -> Ka2  -> c_1
#   ub (2)   kb10 c_1 vs kb9 c_2   '1' < '9'                        -> kb10 -> c_1
#   uc (4)   kc0 c_1 vs kc c_2     a prefix sorts first             -> kc   -> c_2
#   ud (8)   kda c_1, kd_ c_1, kdA c_2   'A' 0x41 < '_' 0x5F < 'a'  -> kdA  -> c_2
#   ue (16)  ke1 c_1 vs " ke2" c_2 (leading space, 0x20)            -> " ke2" -> c_2
#   uf (32)  "kf2" (quoted in the file) c_1 vs kf1 c_2: quotes are not part of the ID -> kf1 -> c_2
#   ug (64)  kg3 c_2 and kg2 c_1 at 09:00:00.500, kg1 c_2 at 09:00:00.499 (earlier: loses
#            although its ID is smaller), kg0 c_2 at 10:00:00.001 (after the start) -> kg2 -> c_1
#   uh (128) uh and ui are linked; kh2 by uh for c_1 at 09:00:00Z, kh1 by ui for c_2 at
#            11:00:00+02:00, the same instant                       -> kh1  -> c_2
#   uj (256) kjA c_2 vs kj9 c_1    '9' 0x39 < 'A' 0x41: digits before letters -> kj9 -> c_1
# c_1: ua, ub, ug, uj = 4 sessions, 1 + 2 + 64 + 256 = 323.  c_2: uc, ud, ue, uf, uh =
# 5 sessions, 4 + 8 + 16 + 32 + 128 = 188. Both campaigns have a qualifying click for all 9.
case("XA1",
     """Nine users, each buying p1 for a different power of two (1 to 256 cents), each with clicks
     for both campaigns at the same millisecond: `Ka2`/`ka1`; `kb10`/`kb9`; `kc0`/`kc`;
     `kda`,`kd_`/`kdA`; `ke1`/` ke2` (leading space); `"kf2"` (quoted)/`kf1`; `kg2`/`kg3` at
     09:00:00.500 with `kg1` 1 ms earlier and `kg0` after the start; `kh2` by `uh`/`kh1` by the
     linked `ui`, written with an offset; `kj9`/`kjA`""",
     """A same-millisecond tie goes to the byte-wise smallest click_id: upper case before lower,
     `10` before `9`, a prefix before the longer ID, digits before letters, `A` < `_` < `a`, a
     leading space first, quotes not part of the ID; an earlier click never wins on its ID; the
     tie is across all the person's IDs. The revenue sums (323 and 188) identify which sessions
     each campaign got""",
     links={"part-00000.csv": '''
uh,ui
'''},
     events={"part-00000.csv": '''
e1,ua,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,1
e2,ub,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,2
e3,uc,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,4
e4,ud,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,8
e5,ue,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,16
e6,uf,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,32
e7,ug,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,64
e8,uh,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,128
e9,uj,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,256
'''},
     clicks={"part-00000.csv": '''
ka1,ua,c_2,2026-10-03T09:00:00Z
Ka2,ua,c_1,2026-10-03T09:00:00Z
kb9,ub,c_2,2026-10-03T09:00:00Z
kb10,ub,c_1,2026-10-03T09:00:00Z
kc0,uc,c_1,2026-10-03T09:00:00Z
kc,uc,c_2,2026-10-03T09:00:00Z
kda,ud,c_1,2026-10-03T09:00:00Z
kd_,ud,c_1,2026-10-03T09:00:00Z
kdA,ud,c_2,2026-10-03T09:00:00Z
ke1,ue,c_1,2026-10-03T09:00:00Z
 ke2,ue,c_2,2026-10-03T09:00:00Z
"kf2",uf,c_1,2026-10-03T09:00:00Z
kf1,uf,c_2,2026-10-03T09:00:00Z
kg3,ug,c_2,1791018000500
kg2,ug,c_1,1791018000500
kg1,ug,c_2,1791018000499
kg0,ug,c_2,1791021600001
kh2,uh,c_1,2026-10-03T09:00:00Z
kh1,ui,c_2,2026-10-03T11:00:00+02:00
kjA,uj,c_2,2026-10-03T09:00:00Z
kj9,uj,c_1,2026-10-03T09:00:00Z
'''},
     user_daily='''
ua,2026-10-03,1,1,1,shoes
ub,2026-10-03,1,1,2,shoes
uc,2026-10-03,1,1,4,shoes
ud,2026-10-03,1,1,8,shoes
ue,2026-10-03,1,1,16,shoes
uf,2026-10-03,1,1,32,shoes
ug,2026-10-03,1,1,64,shoes
uh,2026-10-03,1,1,128,shoes
uj,2026-10-03,1,1,256,shoes
''',
     campaign_daily='''
c_1,search,2026-10-03,4,4,323,9
c_2,social,2026-10-03,5,5,188,9
''')

# XA2. u1's only session starts Oct 10 10:00:00.000 (a purchase of p2 for 700).
#   k1 c_1  Oct 3 10:00:00, exactly 7 days before                 qualifies
#   k2 c_2  1791021599999 = Oct 3 09:59:59.999, 7 days + 1 ms     too old
#   k3 c_3  1791626400001 = Oct 10 10:00:00.001, after the start  no
#   k4 c_999 unknown campaign; k5 ts without a zone; k6 empty user; k7 `C_2` (wrong case):
#      dropped.   k8 is U1's click, another person.
# Only k1 qualifies: c_1 is credited and is the only campaign with a row.
case("XA2",
     """One session at Oct 10 10:00 (a purchase of p2 for 700). Clicks: c_1 exactly 7 days before;
     c_2 7 days + 1 ms before; c_3 1 ms after the start; later ones for `c_999`, with a `ts` without
     a zone, with an empty user, for `C_2`, and one by `U1`""",
     """The only qualifying click is the oldest allowed one: every later click is invalid, too
     late or someone else's, and none of them credits or assists""",
     campaigns=CAMPAIGNS_3,
     events={"part-00000.csv": '''
e1,u1,2026-10-10T10:00:00Z,2026-10-10T12:00:00Z,purchase,p2,700
'''},
     clicks={"part-00000.csv": '''
k1,u1,c_1,2026-10-03T10:00:00Z
k2,u1,c_2,1791021599999
k3,u1,c_3,1791626400001
k4,u1,c_999,2026-10-10T09:00:00Z
k5,u1,c_2,2026-10-10T09:30:00
k6,,c_2,2026-10-10T09:45:00Z
k7,u1,C_2,2026-10-10T09:50:00Z
k8,U1,c_3,2026-10-10T09:55:00Z
'''},
     user_daily='''
u1,2026-10-10,1,1,700,books
''',
     campaign_daily='''
c_1,search,2026-10-10,1,1,700,1
''')

# XA3. u1: every click is dropped ->  organic.
#   k1 campaign `organic` (never in campaigns.csv)   k2 empty campaign      k3 empty user
#   k4 `C_1`     k5 ts without a zone     k6 `c_1 ` with a trailing space    (empty click_id)
#   k7 three fields   k8 five fields   k9 fractional seconds   k10 lowercase `t`
#   k11 an opening quote with no closing one (malformed; the lines after it are still lines)
#   k12 `c_01`   k13 lowercase `z`   k14 a space instead of `T`   k15 `...000.0`   k16 `-5`
#   k17 empty ts
# u2: k20 (c_2, 08:00) is valid; the later k21 (`c_9`) and k22 (lowercase `z`) are dropped
#     -> c_2, the latest VALID click.
# u3 has a valid click for c_1 and no event: no row anywhere.
case("XA3",
     """`u1` has 18 clicks, all dropped: campaign `organic`, an empty campaign, an empty user, `C_1`,
     `c_1 `, `c_01`, an empty click_id, 3 and 5 fields, an unclosed quote, and `ts` values without a
     zone, with fractional seconds, lowercase `t` or `z`, a space for `T`, `.0`, `-5` and empty.
     `u2` has a valid click for c_2 at 08:00 and later ones for `c_9` and with a lowercase `z`.
     `u3` has a click and no event""",
     """Invalid clicks neither credit nor assist, and `organic` is not a campaign a click can
     name; the latest valid click wins behind later invalid ones; a line with an unclosed quote
     does not swallow the lines after it; a person with clicks and no session has no row""",
     events={"part-00000.csv": '''
e1,u1,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
e2,u2,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
'''},
     clicks={"part-00000.csv": '''
k1,u1,organic,2026-10-03T09:00:00Z
k2,u1,,2026-10-03T09:01:00Z
k3,,c_1,2026-10-03T09:02:00Z
k4,u1,C_1,2026-10-03T09:03:00Z
k5,u1,c_1,2026-10-03T09:04:00
k6,u1,c_1 ,2026-10-03T09:05:00Z
,u1,c_1,2026-10-03T09:06:00Z
k7,u1,c_1
k8,u1,c_1,2026-10-03T09:07:00Z,x
k9,u1,c_1,2026-10-03T09:08:00.000Z
k10,u1,c_1,2026-10-03t09:09:00Z
k11,u1,"c_1,2026-10-03T09:10:00Z
k12,u1,c_01,2026-10-03T09:11:00Z
k13,u1,c_1,2026-10-03T09:12:00z
k14,u1,c_1,2026-10-03 09:13:00Z
k15,u1,c_1,1791018840000.0
k16,u1,c_1,-5
k17,u1,c_1,
k20,u2,c_2,2026-10-03T08:00:00Z
k21,u2,c_9,2026-10-03T09:00:00Z
k22,u2,c_1,2026-10-03T09:30:00z
k23,u3,c_1,2026-10-03T09:00:00Z
'''},
     user_daily='''
u1,2026-10-03,1,1,0,
u2,2026-10-03,1,1,0,
''',
     campaign_daily='''
c_2,social,2026-10-03,1,0,0,1
organic,organic,2026-10-03,1,0,0,0
''')

# XA4. u1 and u2 are one person (u1). Sessions: S1 Oct 5 10:00 (u1), S2 Oct 5 15:00 (u2,
# purchase 1000), S3 Oct 12 10:00 (u1), S4 Oct 12 15:00:00.001 = 1791817200001 (u2).
# Clicks: k1 c_1 Oct 5 09:00 (u1), k2 c_1 Oct 5 09:30 (u2), k3 c_2 Oct 5 12:00 (u1),
#         k4 c_3 Oct 5 15:00:00 (u2).
#   S1: k1, k2 qualify -> k2 -> c_1. Assists: c_1 (once, for two clicks).
#   S2: k1..k4 qualify (k4 at the start) -> k4 -> c_3. Assists: c_1, c_2, c_3.
#   S3: k1 and k2 are over 7 days old; k3, k4 qualify -> k4 -> c_3. Assists: c_2, c_3.
#   S4: k4 is 7 days + 1 ms old -> nothing -> organic.
case("XA4",
     """`u1` and `u2` linked. Sessions Oct 5 10:00, Oct 5 15:00 (a purchase of p1 for 1000), Oct 12
     10:00 and Oct 12 15:00:00.001. Clicks: c_1 Oct 5 09:00 and 09:30, c_2 Oct 5 12:00, c_3 Oct 5
     15:00:00, from both IDs""",
     """A campaign can be credited for one session and only assist another on the same day
     (c_1: 1 credited, 2 assisted); two clicks of a campaign are one assist; assists and credit
     follow each session's own 7-day reach, so on Oct 12 the first session still has c_2 and c_3
     and the second, 1 ms past 7 days, is organic""",
     campaigns=CAMPAIGNS_3,
     links={"part-00000.csv": '''
u1,u2
'''},
     events={"part-00000.csv": '''
e1,u1,2026-10-05T10:00:00Z,2026-10-05T12:00:00Z,view,p1,
e2,u2,2026-10-05T15:00:00Z,2026-10-05T16:00:00Z,purchase,p1,1000
e3,u1,2026-10-12T10:00:00Z,2026-10-12T12:00:00Z,view,p1,
e4,u2,1791817200001,2026-10-12T16:00:00Z,view,p1,
'''},
     clicks={"part-00000.csv": '''
k1,u1,c_1,2026-10-05T09:00:00Z
k2,u2,c_1,2026-10-05T09:30:00Z
k3,u1,c_2,2026-10-05T12:00:00Z
k4,u2,c_3,2026-10-05T15:00:00Z
'''},
     user_daily='''
u1,2026-10-05,2,2,1000,shoes
u1,2026-10-12,2,2,0,
''',
     campaign_daily='''
c_1,search,2026-10-05,1,0,0,2
c_2,social,2026-10-05,0,0,0,1
c_2,social,2026-10-12,0,0,0,1
c_3,email,2026-10-05,1,1,1000,1
c_3,email,2026-10-12,1,0,0,1
organic,organic,2026-10-12,1,0,0,0
''')

# XA5. u1: sessions Oct 3 09:00, 12:00, 15:00; clicks k1 c_1 08:00, k2 c_2 11:00, k3 c_1 11:30.
#   09:00: k1 -> c_1; assists c_1.
#   12:00: k1, k2, k3 -> k3 -> c_1; assists c_1, c_2.
#   15:00: the same three -> c_1; assists c_1, c_2.
# u2: session 12:00 (purchase p2 600); k4 c_2 11:59:59 -> c_2; k5 c_1 12:00:01 is after the start.
# c_1: 3 credited, 3 assisted (one per session, although two of them have two c_1 clicks).
# c_2: 1 credited (600), 3 assisted (u1's 12:00 and 15:00, u2's 12:00).
case("XA5",
     """`u1`: sessions at 09:00, 12:00 and 15:00; clicks c_1 08:00, c_2 11:00, c_1 11:30. `u2`: a
     purchase of p2 for 600 at 12:00; clicks c_2 11:59:59 and c_1 12:00:01""",
     """Assists are counted per session: three sessions of one person on one day are three
     assists for c_1 (not one per person-day, not five for five click-session pairs), and c_2's
     three come from two people""",
     events={"part-00000.csv": '''
e1,u1,2026-10-03T09:00:00Z,2026-10-03T16:00:00Z,view,p1,
e2,u1,2026-10-03T12:00:00Z,2026-10-03T16:00:00Z,view,p1,
e3,u1,2026-10-03T15:00:00Z,2026-10-03T16:00:00Z,view,p1,
e4,u2,2026-10-03T12:00:00Z,2026-10-03T16:00:00Z,purchase,p2,600
'''},
     clicks={"part-00000.csv": '''
k1,u1,c_1,2026-10-03T08:00:00Z
k2,u1,c_2,2026-10-03T11:00:00Z
k3,u1,c_1,2026-10-03T11:30:00Z
k4,u2,c_2,2026-10-03T11:59:59Z
k5,u2,c_1,2026-10-03T12:00:01Z
'''},
     user_daily='''
u1,2026-10-03,3,3,0,
u2,2026-10-03,1,1,600,books
''',
     campaign_daily='''
c_1,search,2026-10-03,3,0,0,3
c_2,social,2026-10-03,1,1,600,3
''')

# ==========================================================================
group("Constructed regression cases",
      "The three cases named in the team's regression table.")
# ==========================================================================

# XR1a / XR1b. u1: S1 Oct 7 10:00 (purchase p1 1000), S2 Oct 12 09:00:00, S3 Oct 13
# 09:00:00.001 = 1791882000001.
# Clicks: c_1: k1 Oct 5 09:00, k5 Oct 5 21:00, k2 Oct 6 09:00.  c_2: k3 Oct 6 09:00.
#   S1: all four qualify; the latest instant, Oct 6 09:00, is shared by k2 (c_1) and k3
#       (c_2); `k2` < `k3` -> c_1. Assists: c_1 once (three clicks), and c_2: the click that
#       loses the tie is c_2's only click and still assists.
#   S2: k1 is exactly 7 days old and still qualifies; the same tie -> c_1. Assists c_1, c_2.
#   S3: k2 and k3 are 7 days + 1 ms old, k1 and k5 older -> organic.
# XR1b holds the same clicks 2 or 3 times each, quoted differently, over three files in
# other orders, and the same events over two files. Its expected tables are XR1a's.
_xr1_events = [
    "e1,u1,2026-10-07T10:00:00Z,2026-10-07T12:00:00Z,purchase,p1,1000\n",
    "e2,u1,2026-10-12T09:00:00Z,2026-10-12T12:00:00Z,view,p1,\n",
    "e3,u1,1791882000001,2026-10-13T12:00:00Z,view,p2,\n",
]
_xr1_user = '''
u1,2026-10-07,1,1,1000,shoes
u1,2026-10-12,1,1,0,
u1,2026-10-13,1,1,0,
'''
_xr1_campaign = '''
c_1,search,2026-10-07,1,1,1000,1
c_1,search,2026-10-12,1,0,0,1
c_2,social,2026-10-07,0,0,0,1
c_2,social,2026-10-12,0,0,0,1
organic,organic,2026-10-13,1,0,0,0
'''
case("XR1a",
     """Sessions Oct 7 10:00 (a purchase of p1 for 1000), Oct 12 09:00:00 and Oct 13 09:00:00.001.
     Clicks for c_1 on Oct 5 09:00 (`k1`), Oct 5 21:00 (`k5`) and Oct 6 09:00 (`k2`), and for c_2 on
     Oct 6 09:00 (`k3`), tied with `k2`""",
     """Three overlapping clicks of one campaign are one assist per session; c_2, whose only
     click loses the same-millisecond tie, still assists both sessions (dropping the loser
     before counting assists loses its rows); the winner takes the session and its revenue;
     the oldest click still counts at exactly 7 days and nothing counts 1 ms later""",
     events={"part-00000.csv": "\n" + "".join(_xr1_events)},
     clicks={"part-00000.csv": '''
k1,u1,c_1,2026-10-05T09:00:00Z
k5,u1,c_1,2026-10-05T21:00:00Z
k3,u1,c_2,2026-10-06T09:00:00Z
k2,u1,c_1,2026-10-06T09:00:00Z
'''},
     user_daily=_xr1_user,
     campaign_daily=_xr1_campaign)
case("XR1b",
     """XR1a with every click written 2 or 3 times, quoted differently, over three click files in
     other orders, and the events over two files in another order""",
     """Duplicated clicks and reshuffled files change nothing: both tables are byte for byte
     XR1a's""",
     events={"part-00000.csv": "\n" + _xr1_events[2] + _xr1_events[0],
             "part-00001.csv": "\n" + _xr1_events[1]},
     clicks={"part-00000.csv": '''
k3,u1,c_2,2026-10-06T09:00:00Z
"k5","u1","c_1","2026-10-05T21:00:00Z"
k1,u1,c_1,2026-10-05T09:00:00Z
''', "part-00001.csv": '''
k2,u1,c_1,2026-10-06T09:00:00Z
k1,"u1",c_1,2026-10-05T09:00:00Z
"k3",u1,c_2,"2026-10-06T09:00:00Z"
''', "part-00002.csv": '''
k5,u1,c_1,2026-10-05T21:00:00Z
"k2","u1","c_1","2026-10-06T09:00:00Z"
k3,u1,"c_2",2026-10-06T09:00:00Z
"k1",u1,c_1,2026-10-05T09:00:00Z
'''},
     user_daily=_xr1_user,
     campaign_daily=_xr1_campaign)

# XR2. Products pa (A), pb (B), pc (C). u1 on Oct 3: session 1 (10:00, 10:05) buys pa for 6
# and pc for 5; session 2 (14:00, 14:05) buys pb for 6 and pc for 5.
# The day: A 6, B 6, C 10 -> C, although C is second in each session. Revenue 22.
case("XR2",
     """Products `pa` (A), `pb` (B), `pc` (C). Two sessions on Oct 3: the first buys `pa` for 6 and
     `pc` for 5, the second `pb` for 6 and `pc` for 5""",
     """The day's top category comes from sums over all the day's sessions (C with 10), not from
     each session's winner (A and B with 6)""",
     products="product_id,category\npa,A\npb,B\npc,C\n",
     events={"part-00000.csv": '''
e1,u1,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,pa,6
e2,u1,2026-10-03T10:05:00Z,2026-10-03T12:00:00Z,purchase,pc,5
e3,u1,2026-10-03T14:00:00Z,2026-10-03T16:00:00Z,purchase,pb,6
e4,u1,2026-10-03T14:05:00Z,2026-10-03T16:00:00Z,purchase,pc,5
'''},
     user_daily='''
u1,2026-10-03,2,4,22,C
''',
     campaign_daily='''
organic,organic,2026-10-03,2,2,22,0
''')

# XR3. No event survives: e1 is 2 days late; e2 is at window_end; e3 has no user; e4 is a
# purchase priced 0; e5 has the type `click`; e6 has 6 fields; e7's earliest copy has a bad
# ts. The second events file holds only the header. A valid click and a link exist.
# Both tables are empty; each must still be a readable Parquet table with its columns.
case("XR3",
     """Eight event lines, none surviving (late, at the window's end, no user, a purchase priced
     `0`, event type `click`, 6 fields, an earliest copy with a bad `ts`), and a second events file
     with only the header. One valid click and one link""",
     """With no surviving event both tables are empty, and each output folder must still hold a
     readable Parquet table with every column and its type; a click without a session makes no
     campaign row""",
     links={"part-00000.csv": '''
u1,u2
'''},
     events={"part-00000.csv": '''
e1,u1,2026-10-03T10:00:00Z,2026-10-05T10:00:00Z,view,p1,
e2,u1,2026-10-15T00:00:00Z,2026-10-15T00:01:00Z,view,p1,
e3,,2026-10-03T10:00:00Z,2026-10-03T10:01:00Z,view,p1,
e4,u1,2026-10-03T10:00:00Z,2026-10-03T10:01:00Z,purchase,p1,0
e5,u1,2026-10-03T10:00:00Z,2026-10-03T10:01:00Z,click,p1,
e6,u1,2026-10-03T10:00:00Z,2026-10-03T10:01:00Z,view,p1
e7,u1,2026-10-03T10:00:00Z,2026-10-03T10:05:00Z,view,p1,
e7,u1,not-a-time,2026-10-03T10:01:00Z,view,p1,
''', "part-00001.csv": ""},
     clicks={"part-00000.csv": '''
k1,u1,c_1,2026-10-03T09:00:00Z
'''})

# ==========================================================================
group("CSV dialect and files",
      "One user, `u1`, on Oct 3 unless noted.")
# ==========================================================================

# XC1. Lines kept: e1 (every field quoted, the price `""`), e2 (quoted F3 ts 1791021900000 =
# 10:05:00.000, quoted price `0500` = 500), e7 and e9 (each right after a line with an
# unclosed quote). Dropped as invalid: e3 (user `""` is empty), the line whose event_id is
# `""`, e4 (product `""`), e5 (a purchase priced `""`). Dropped as malformed, each with
# exactly 7 comma-separated fields unless noted: e6 (`"e6`), e8 (a lone `"` as the price),
# e10 (`""x`), e11 (`"p1"x`), e12 (`x"p1"`), e13 (`vi"ew`), e14 (` "p1"`: the quote is not at
# the field's start), e15 (`"p1" `), e16 (`""p1""`), e17 (`"p""1"`), e18 (`"""`), e19 (`"u1`
# and `x"`), e20 (8 fields: every comma separates, quoted or not).
# u1: e1 10:00, e2 10:05 (500, books), e7 10:15, e9 10:25 -> 1 session, 4 events.
# Clicks: k1's ts has an unclosed quote: malformed, and the next line is still a line.
#         k2 (c_2, 08:00, quoted ts) is valid. k3 `"c_1,...Z"`: two fields with stray quotes.
#         -> c_2.
case("XC1",
     '''Kept: a line with every field quoted and the price `""`; one with a quoted F3 `ts` and the
     price `"0500"`; two plain lines that each follow a line with an unclosed quote. Dropped: `""` as
     user, as event_id, as product, as a purchase's price; a quote opening the line; a lone `"`
     as the last field; `""x`, `"p1"x`, `x"p1"`, `vi"ew`, ` "p1"`, `"p1" `, `""p1""`, `"p""1"`, `"""`;
     `"u1,x"` with 7 and with 8 fields in all. Clicks: an unclosed quote, then a valid click with a
     quoted `ts`, then `"c_1,...Z"`''',
     """Quotes are syntax only around a whole field; `""` is an empty field, so a null; any other
     quote makes the line malformed, and only that line: an unclosed quote never swallows the
     next line, a doubled quote is not an escape, and a comma inside quotes still separates""",
     events={"part-00000.csv": '''
"e1","u1","2026-10-03T10:00:00Z","2026-10-03T12:00:00Z","view","p1",""
"e2","u1","1791021900000","2026-10-03T12:00:00Z","purchase","p2","0500"
e3,"",2026-10-03T10:10:00Z,2026-10-03T12:00:00Z,view,p1,
"",u1,2026-10-03T10:10:00Z,2026-10-03T12:00:00Z,view,p1,
e4,u1,2026-10-03T10:10:00Z,2026-10-03T12:00:00Z,view,"",
e5,u1,2026-10-03T10:10:00Z,2026-10-03T12:00:00Z,"purchase",p1,""
"e6,u1,2026-10-03T10:10:00Z,2026-10-03T12:00:00Z,view,p1,
e7,u1,2026-10-03T10:15:00Z,2026-10-03T12:00:00Z,view,p1,
e8,u1,2026-10-03T10:20:00Z,2026-10-03T12:00:00Z,view,p1,"
e9,u1,2026-10-03T10:25:00Z,2026-10-03T12:00:00Z,view,p1,
e10,u1,2026-10-03T10:30:00Z,2026-10-03T12:00:00Z,view,p1,""x
e11,u1,2026-10-03T10:30:00Z,2026-10-03T12:00:00Z,view,"p1"x,
e12,u1,2026-10-03T10:30:00Z,2026-10-03T12:00:00Z,view,x"p1",
e13,u1,2026-10-03T10:30:00Z,2026-10-03T12:00:00Z,vi"ew,p1,
e14,u1,2026-10-03T10:30:00Z,2026-10-03T12:00:00Z,view, "p1",
e15,u1,2026-10-03T10:30:00Z,2026-10-03T12:00:00Z,view,"p1" ,
e16,u1,2026-10-03T10:30:00Z,2026-10-03T12:00:00Z,view,""p1"",
e17,u1,2026-10-03T10:30:00Z,2026-10-03T12:00:00Z,view,"p""1",
e18,u1,2026-10-03T10:30:00Z,2026-10-03T12:00:00Z,view,""",
e19,"u1,x",2026-10-03T10:30:00Z,2026-10-03T12:00:00Z,view,p1
e20,"u1,x",2026-10-03T10:30:00Z,2026-10-03T12:00:00Z,view,p1,
'''},
     clicks={"part-00000.csv": '''
k1,u1,c_1,"2026-10-03T09:00:00Z
k2,u1,c_2,"2026-10-03T08:00:00Z"
k3,u1,"c_1,2026-10-03T09:30:00Z"
'''},
     user_daily='''
u1,2026-10-03,1,4,500,books
''',
     campaign_daily='''
c_2,social,2026-10-03,1,1,500,1
''')

# XC2. Folder contents: every *.csv is read whatever its name; nothing else is.
#   events/a.csv: blank lines (after the header, in the middle, at the end), a line of three
#     spaces (one field), `,,,,,,` (seven empty fields: no event_id), e2 with 6 fields, e3
#     with 8: all dropped. e1 (view 10:00) and e4 (cart 10:10; its price 999 is ignored) kept.
#   events/z-last.csv: e5 (u1, purchase p1 300, 10:15), e6 (u2, view 10:20), e7 (u3, view 10:00).
#   events/empty.csv: only the header.
#   events/notes.txt and events/part-00000.csv.bak hold a header and a valid line for u9 and
#     u8. They are not *.csv: reading them adds a person.
#   clicks/clicks.csv: k1 (c_1, 09:00) valid; k2 3 fields; k3 5 fields (a trailing comma);
#     `,,,` no click_id. clicks/part-00000.csv: only the header.
#     clicks/old.csv.txt: a later click for c_2 that must not be read.
#   links/x.csv: a blank line, `u2,u1` (valid), `u3,u1,` (3 fields: dropped).
#     links/part-00000.csv: only the header.
# People: u1 = {u1,u2}: 10:00, 10:10, 10:15 (300), 10:20 -> 1 session, 4 events -> c_1.
#         u3: 1 session, 1 event -> organic.
case("XC2",
     """Event files `a.csv` (blank lines, a line of three spaces, `,,,,,,`, lines with 6 and 8 fields
     around two valid ones), `z-last.csv` (three valid lines), `empty.csv` (only the header), plus
     `notes.txt` and `part-00000.csv.bak`, each with a header and a valid line. Clicks in
     `clicks.csv` (one valid; 3 fields; 5 fields; `,,,`), a header-only `part-00000.csv` and
     `old.csv.txt`. Links in `x.csv` (a blank line, `u2,u1`, `u3,u1,`) and a header-only file""",
     """Every `*.csv` in a folder is read whatever its name, and only those (`.txt` and `.csv.bak`
     are not input); header-only files are fine; blank and whitespace-only lines and wrong field
     counts are dropped in all three kinds of file; a line of empty fields has no ID""",
     events={"a.csv": Raw(EVENTS_HEADER.encode() +
                          b'\n'
                          b'e1,u1,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,\n'
                          b'\n'
                          b'\n'
                          b'   \n'
                          b',,,,,,\n'
                          b'e2,u1,2026-10-03T10:05:00Z,2026-10-03T12:00:00Z,view,p1\n'
                          b'e3,u1,2026-10-03T10:05:00Z,2026-10-03T12:00:00Z,view,p1,,\n'
                          b'e4,u1,2026-10-03T10:10:00Z,2026-10-03T12:00:00Z,cart,p2,999\n'
                          b'\n'),
             "z-last.csv": '''
e5,u1,2026-10-03T10:15:00Z,2026-10-03T12:00:00Z,purchase,p1,300
e6,u2,2026-10-03T10:20:00Z,2026-10-03T12:00:00Z,view,p1,
e7,u3,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
''',
             "empty.csv": ""},
     clicks={"clicks.csv": Raw(CLICKS_HEADER.encode() +
                               b'\n'
                               b'k1,u1,c_1,2026-10-03T09:00:00Z\n'
                               b'k2,u1,c_2\n'
                               b'k3,u1,c_2,2026-10-03T09:30:00Z,\n'
                               b',,,\n'),
             "part-00000.csv": ""},
     links={"x.csv": Raw(LINKS_HEADER.encode() +
                         b'\n'
                         b'u2,u1\n'
                         b'u3,u1,\n'),
            "part-00000.csv": ""},
     other_files={
         "events/notes.txt": EVENTS_HEADER.encode() +
         b'e9,u9,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,\n',
         "events/part-00000.csv.bak": EVENTS_HEADER.encode() +
         b'e8,u8,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,\n',
         "clicks/old.csv.txt": CLICKS_HEADER.encode() +
         b'k9,u1,c_2,2026-10-03T09:45:00Z\n',
     },
     user_daily='''
u1,2026-10-03,1,4,300,shoes
u3,2026-10-03,1,1,0,
''',
     campaign_daily='''
c_1,search,2026-10-03,1,1,300,1
organic,organic,2026-10-03,1,0,0,0
''')

# XC3. The last line of a file has no trailing \n. It is still a line.
#   events/part-00000.csv ends `...purchase,p2,450` (u2's purchase at 10:10).
#   events/part-00001.csv is the header alone, with no \n at all.
#   clicks/part-00000.csv ends `k1,u1,c_1,2026-10-03T09:00:00Z`.
#   links/part-00000.csv ends `u2,u1`.
# u1 = {u1,u2}: 10:00 view, 10:10 purchase 450 (books) -> 1 session, 2 events -> c_1.
# Losing the last line loses the purchase, the click or the link.
case("XC3",
     """Files whose last line has no trailing `\\n`: the events file ends with `u2`'s purchase of p2
     for 450, the clicks file with the only click (c_1, 09:00), the links file with `u2,u1`; a
     second events file is a header with no `\\n` at all. The rules do not spell this out and the
     generator always writes the final `\\n`""",
     """Text after the last `\\n` is a line: the final event, click and link all count""",
     events={"part-00000.csv": Raw(EVENTS_HEADER.encode() +
                                   b'e1,u1,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,\n'
                                   b'e2,u2,2026-10-03T10:10:00Z,2026-10-03T12:00:00Z,purchase,p2,450'),
             "part-00001.csv": Raw(EVENTS_HEADER.encode()[:-1])},
     clicks={"part-00000.csv": Raw(CLICKS_HEADER.encode() + b'k1,u1,c_1,2026-10-03T09:00:00Z')},
     links={"part-00000.csv": Raw(LINKS_HEADER.encode() + b'u2,u1')},
     user_daily='''
u1,2026-10-03,1,2,450,books
''',
     campaign_daily='''
c_1,search,2026-10-03,1,1,450,1
''')

# XC4. \r is an ordinary character wherever it is (the header lines end in plain \n).
# Events:
#   e1 `...,view,p1,\r`           the price `\r` of a view is ignored          kept (10:00)
#   e2 `...,view,p1,""\r`         `""\r`: a quote not around the whole field   malformed
#   e3 `...,view\r,p1,`           type `view\r`                                dropped
#   e4 ts `...10:15:00Z\r`        bad ts                                       dropped
#   e5 `...,purchase,p1\r,700`    product `p1\r` is not in products.csv        kept: 700, unknown
#   `\r`                          a line holding one \r: one field             malformed
#   e6 `...,cart,p1,"5"\r`        `"5"\r`                                      malformed
#   e7 `...,purchase,p1,300`      a plain line                                 kept: 300, shoes
#   e8 ts `1791023700000\r`       a non-digit in F3                            dropped
# u1: 10:00, 10:20, 10:30 -> 1 session, 3 events, 1000, top unknown (700 > 300).
# Clicks: k1 `"...Z"\r` malformed; k2 ts `1791019800000\r` bad; k3 campaign `c_1\r` unknown;
#         k4 user `u1\r` is another person (valid click, no session); k5 (c_2, 08:00) valid.
#         Every wrongly accepted click would give c_1. -> c_2.
case("XC4",
     """Events: a view ending `,\\r\\n` (kept); a view ending `,""\\r\\n`; event type `view\\r`; a `ts`
     followed by `\\r` (F1 and F3); a purchase of product `p1\\r` for 700; a line that is only `\\r`;
     a cart ending `"5"\\r\\n`; a plain purchase of p1 for 300. Clicks: a quoted `ts` followed by
     `\\r`, an F3 `ts` followed by `\\r`, campaign `c_1\\r`, user `u1\\r`, and one valid click for
     c_2""",
     """`\\r` is data in any field, not only the last: it makes a type, a timestamp or a campaign
     invalid, a product unknown and a user someone else; a quoted field followed by `\\r` is a
     stray quote, so the line is malformed even where the value would be ignored""",
     events={"part-00000.csv": Raw(EVENTS_HEADER.encode() +
                                   b'e1,u1,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,\r\n'
                                   b'e2,u1,2026-10-03T10:05:00Z,2026-10-03T12:00:00Z,view,p1,""\r\n'
                                   b'e3,u1,2026-10-03T10:10:00Z,2026-10-03T12:00:00Z,view\r,p1,\n'
                                   b'e4,u1,2026-10-03T10:15:00Z\r,2026-10-03T12:00:00Z,view,p1,\n'
                                   b'e5,u1,2026-10-03T10:20:00Z,2026-10-03T12:00:00Z,purchase,p1\r,700\n'
                                   b'\r\n'
                                   b'e6,u1,2026-10-03T10:25:00Z,2026-10-03T12:00:00Z,cart,p1,"5"\r\n'
                                   b'e7,u1,2026-10-03T10:30:00Z,2026-10-03T12:00:00Z,purchase,p1,300\n'
                                   b'e8,u1,1791023700000\r,2026-10-03T12:00:00Z,view,p1,\n')},
     clicks={"part-00000.csv": Raw(CLICKS_HEADER.encode() +
                                   b'k1,u1,c_1,"2026-10-03T09:30:00Z"\r\n'
                                   b'k2,u1,c_1,1791019800000\r\n'
                                   b'k3,u1,c_1\r,2026-10-03T09:00:00Z\n'
                                   b'k4,u1\r,c_1,2026-10-03T09:10:00Z\n'
                                   b'k5,u1,c_2,2026-10-03T08:00:00Z\n')},
     user_daily='''
u1,2026-10-03,1,3,1000,unknown
''',
     campaign_daily='''
c_2,social,2026-10-03,1,1,1000,1
''')

# XM1 / XM2. The window is Nov 1 to Nov 15 2026; the click window starts Oct 25 00:00.
#   u1 Nov 1 00:00:00 = window_start  valid      u2 Oct 31 23:59:59  bad
#   u3 Nov 14 23:59:59                valid      u4 Nov 15 00:00:00  bad
#   u5 Oct 3 10:00:00, inside the window every other case uses, outside this one: bad
#   k1 (u1, c_1) Oct 25 00:00:00 = window_start - 7 days, exactly 7 days before u1's start -> c_1
# XM1 writes the manifest on one line, exactly as the rules print it; XM2 without spaces,
# with the keys the other way round and without a final newline.
_xm_events = '''
e1,u1,2026-11-01T00:00:00Z,2026-11-01T00:10:00Z,view,p1,
e2,u2,2026-10-31T23:59:59Z,2026-11-01T00:10:00Z,view,p1,
e3,u3,2026-11-14T23:59:59Z,2026-11-15T00:10:00Z,view,p1,
e4,u4,2026-11-15T00:00:00Z,2026-11-15T00:10:00Z,view,p1,
e5,u5,2026-10-03T10:00:00Z,2026-10-03T10:10:00Z,view,p1,
'''
_xm_clicks = '''
k1,u1,c_1,2026-10-25T00:00:00Z
'''
_xm_user = '''
u1,2026-11-01,1,1,0,
u3,2026-11-14,1,1,0,
'''
_xm_campaign = '''
c_1,search,2026-11-01,1,0,0,1
organic,organic,2026-11-14,1,0,0,0
'''
case("XM1",
     """`manifest.json` on one line, as the rules print it:
     `{"window_start": "2026-11-01T00:00:00Z", "window_end": "2026-11-15T00:00:00Z"}`. Events at the
     window's first instant, 1 s before it, its last second, its end, and at Oct 3 10:00; a click
     at `window_start - 7 days`""",
     """The window comes from the manifest whatever its layout, and is not Oct 1 to Oct 15""",
     manifest_json='{"window_start": "2026-11-01T00:00:00Z", "window_end": "2026-11-15T00:00:00Z"}\n',
     events={"part-00000.csv": _xm_events}, clicks={"part-00000.csv": _xm_clicks},
     user_daily=_xm_user, campaign_daily=_xm_campaign)
case("XM2",
     """XM1 with the manifest written
     `{"window_end":"2026-11-15T00:00:00Z","window_start":"2026-11-01T00:00:00Z"}`: no spaces, the
     keys the other way round, no final newline. Valid JSON, but not a layout the generator
     writes""",
     """The manifest is JSON: key order and whitespace mean nothing""",
     manifest_json='{"window_end":"2026-11-15T00:00:00Z","window_start":"2026-11-01T00:00:00Z"}',
     events={"part-00000.csv": _xm_events}, clicks={"part-00000.csv": _xm_clicks},
     user_daily=_xm_user, campaign_daily=_xm_campaign)

# ==========================================================================
group("Timestamps",
      "One view per user; a user with a bad `ts` has no row.")
# ==========================================================================

# XT1. Window Feb 20 to Mar 5 2028 (a leap year: Feb 29 exists). Click window from Feb 13.
#   u1  2028-02-29T10:00:00Z            valid, Feb 29
#   u2  2028-02-30T10:00:00Z            no such day
#   u3  2028-02-29T23:30:00-02:00       = Mar 1 01:30Z   valid, Mar 1
#   u4  2028-03-01T00:30:00+01:00       = Feb 29 23:30Z  valid, Feb 29
#   u5  1835438400000                   = Feb 29 12:00:00.000Z  valid, Feb 29
#   u6  2028-02-29T24:00:00Z            hour 24
#   u7  2028-02-29T23:59:60Z            second 60
#   u8  2028-03-04T23:59:59Z            the last second   valid, Mar 4
#   u9  2028-03-05T00:00:00Z            window_end
#   u10 2028-02-20T00:00:00Z            window_start      valid, Feb 20
#   u11 2028-02-19T23:59:59Z            before the window
# Clicks: k1 (u1, c_1) Feb 22 10:00:00: exactly 7 days before Feb 29 10:00 (Feb has 29 days) -> c_1
#         k2 (u10, c_2) Feb 13 00:00:00 = window_start - 7 days, 7 days before u10's start -> c_2
#         k3 (u8, c_1) Feb 12 23:59:59: before the click window -> bad -> u8 is organic
#         k4 (u5, c_2) Feb 29 12:00:00Z = u5's start -> c_2
# Feb 29: u1 c_1, u5 c_2, u4 organic.
case("XT1",
     """The window is Feb 20 to Mar 5, 2028. `ts` values: Feb 29 10:00 (valid), Feb 30, Feb 29
     `23:30:00-02:00` (Mar 1), Mar 1 `00:30:00+01:00` (Feb 29), F3 `1835438400000` (Feb 29 12:00),
     Feb 29 `24:00:00` and `23:59:60`, and the window's four edges. Clicks exactly 7 days before a
     Feb 29 session (Feb 22), at `window_start - 7 days` (Feb 13), 1 s before that, and at an F3
     session's start""",
     """A leap day in the window: Feb 29, 2028 is a date, Feb 30 is not, 7 days before Feb 29 is
     Feb 22, hour 24 and second 60 do not roll over into Mar 1, and the window is the manifest's,
     not October's""",
     manifest_json=manifest("2028-02-20T00:00:00Z", "2028-03-05T00:00:00Z"),
     events={"part-00000.csv": '''
e1,u1,2028-02-29T10:00:00Z,2028-02-29T12:00:00Z,view,p1,
e2,u2,2028-02-30T10:00:00Z,2028-02-29T12:00:00Z,view,p1,
e3,u3,2028-02-29T23:30:00-02:00,2028-03-01T02:00:00Z,view,p1,
e4,u4,2028-03-01T00:30:00+01:00,2028-03-01T00:00:00Z,view,p1,
e5,u5,1835438400000,2028-02-29T12:30:00Z,view,p1,
e6,u6,2028-02-29T24:00:00Z,2028-03-01T00:30:00Z,view,p1,
e7,u7,2028-02-29T23:59:60Z,2028-03-01T00:30:00Z,view,p1,
e8,u8,2028-03-04T23:59:59Z,2028-03-05T00:30:00Z,view,p1,
e9,u9,2028-03-05T00:00:00Z,2028-03-05T00:30:00Z,view,p1,
e10,u10,2028-02-20T00:00:00Z,2028-02-20T00:30:00Z,view,p1,
e11,u11,2028-02-19T23:59:59Z,2028-02-20T00:30:00Z,view,p1,
'''},
     clicks={"part-00000.csv": '''
k1,u1,c_1,2028-02-22T10:00:00Z
k2,u10,c_2,2028-02-13T00:00:00Z
k3,u8,c_1,2028-02-12T23:59:59Z
k4,u5,c_2,2028-02-29T12:00:00Z
'''},
     user_daily='''
u1,2028-02-29,1,1,0,
u10,2028-02-20,1,1,0,
u3,2028-03-01,1,1,0,
u4,2028-02-29,1,1,0,
u5,2028-02-29,1,1,0,
u8,2028-03-04,1,1,0,
''',
     campaign_daily='''
c_1,search,2028-02-29,1,0,0,1
c_2,social,2028-02-20,1,0,0,1
c_2,social,2028-02-29,1,0,0,1
organic,organic,2028-02-29,1,0,0,0
organic,organic,2028-03-01,1,0,0,0
organic,organic,2028-03-04,1,0,0,0
''')

# XT2. Window Feb 20 to Mar 6 2027 (not a leap year: Feb has 28 days).
#   u1  2027-02-29T10:00:00Z         no such day (a lenient parser says Mar 1)
#   u2  2027-02-28T23:59:59Z         valid, Feb 28
#   u3  2027-03-01T00:00:00Z         valid, Mar 1
#   u4  2027-02-28T24:00:00Z         hour 24            u5  2027-02-30    u6  month 13
#   u7  2027-02-28T23:60:00Z         minute 60          u8  2027-03-00    u9  month 00
#   u10 2027-02-28T19:00:00-05:00    = Mar 1 00:00:00Z  valid, Mar 1
#   u11 2027-02-29T19:00:00-05:00    the written date does not exist
#   u12 2027-03-01T04:59:59+05:00    = Feb 28 23:59:59Z valid, Feb 28
#   u13 1803859200000                = Mar 1 00:00:00.000Z  valid, Mar 1
#   u14 2027-02-28T23:59:60Z         second 60          u15 2027-02-31
case("XT2",
     """The window is Feb 20 to Mar 6, 2027. `ts` values: Feb 29, Feb 30, Feb 31, Mar 00, month 13,
     month 00, hour 24, minute 60, second 60 (all in or next to the window), Feb 29 with an offset;
     and the valid Feb 28 23:59:59, Mar 1 00:00:00, Feb 28 `19:00:00-05:00` (Mar 1), Mar 1
     `04:59:59+05:00` (Feb 28) and F3 `1803859200000` (Mar 1)""",
     """Impossible calendar values are bad timestamps, never rolled over into a real instant
     (each of these would land inside the window): Feb 29 outside a leap year, day or month 00,
     minute 60; the written date must exist even when the offset moves the instant elsewhere""",
     manifest_json=manifest("2027-02-20T00:00:00Z", "2027-03-06T00:00:00Z"),
     events={"part-00000.csv": '''
e1,u1,2027-02-29T10:00:00Z,2027-03-01T10:30:00Z,view,p1,
e2,u2,2027-02-28T23:59:59Z,2027-03-01T10:30:00Z,view,p1,
e3,u3,2027-03-01T00:00:00Z,2027-03-01T10:30:00Z,view,p1,
e4,u4,2027-02-28T24:00:00Z,2027-03-01T10:30:00Z,view,p1,
e5,u5,2027-02-30T10:00:00Z,2027-03-01T10:30:00Z,view,p1,
e6,u6,2027-13-01T10:00:00Z,2027-03-01T10:30:00Z,view,p1,
e7,u7,2027-02-28T23:60:00Z,2027-03-01T10:30:00Z,view,p1,
e8,u8,2027-03-00T10:00:00Z,2027-03-01T10:30:00Z,view,p1,
e9,u9,2027-00-28T10:00:00Z,2027-03-01T10:30:00Z,view,p1,
e10,u10,2027-02-28T19:00:00-05:00,2027-03-01T10:30:00Z,view,p1,
e11,u11,2027-02-29T19:00:00-05:00,2027-03-01T10:30:00Z,view,p1,
e12,u12,2027-03-01T04:59:59+05:00,2027-03-01T10:30:00Z,view,p1,
e13,u13,1803859200000,2027-03-01T10:30:00Z,view,p1,
e14,u14,2027-02-28T23:59:60Z,2027-03-01T10:30:00Z,view,p1,
e15,u15,2027-02-31T10:00:00Z,2027-03-01T10:30:00Z,view,p1,
'''},
     user_daily='''
u10,2027-03-01,1,1,0,
u12,2027-02-28,1,1,0,
u13,2027-03-01,1,1,0,
u2,2027-02-28,1,1,0,
u3,2027-03-01,1,1,0,
''',
     campaign_daily='''
organic,organic,2027-02-28,2,0,0,0
organic,organic,2027-03-01,3,0,0,0
''')

# XT3. Six valid forms (ok1..ok6) and 28 that are "anything else" (b01..b28). Ingest 12:00.
#   ok1 0001791021600000              = Oct 3 10:00:00.000
#   ok2 2026-10-03T10:00:00+00:00     = Oct 3 10:00
#   ok3 2026-10-03T10:00:00-23:59     = Oct 4 09:59:00   (Oct 4; ingested before its ts)
#   ok4 "2026-10-03T12:00:00+02:00"   = Oct 3 10:00      (quoted)
#   ok5 2026-10-02T23:30:00-10:00     = Oct 3 09:30
#   ok6 2026-10-04T08:00:00+12:00     = Oct 3 20:00
# Clicks: k1 (ok1, c_1) 0001791018000000 = 09:00:00 -> c_1.   k2 (ok2) `...000.0` bad -> organic.
#         k3 (ok4, c_2) 09:00:00+00:00 -> c_2.   k5 (ok6) offset with seconds: bad -> organic.
#         k6 (ok3, c_2) 2026-10-03T23:59:00-10:00 = Oct 4 09:59:00 = ok3's start -> c_2 (Oct 4).
#         ok5 has no click -> organic.
case("XT3",
     """Valid: `0001791021600000`, `...T10:00:00+00:00`, `...T10:00:00-23:59` (Oct 4 09:59),
     a quoted `"...T12:00:00+02:00"`, `2026-10-02T23:30:00-10:00`, `2026-10-04T08:00:00+12:00`. Bad,
     one user each: fractional seconds, `z`, `t`, a space for `T`, no zone, no seconds, a trailing
     and a leading space, offsets `+2:00`, `+02`, `+02:0`, `+02:00:00` and `-24:00`, a one-digit day,
     `20261003T100000Z`, `+00:00Z`, slashes, `Z0`, no date, no time, hour 25 with an offset, a
     Unicode minus sign, and F3 with `+`, a trailing space, underscores, an exponent, `0x`, or `Z`.
     Clicks in the valid forms and with `.0` and `+00:00:00`""",
     """Only the three exact forms are timestamps; everything else is bad, in events and in
     clicks, however close it looks; a valid form may be quoted, may carry leading zeros (F3) and
     any offset from `-23:59` to `+23:59`""",
     events={"part-00000.csv": '''
e1,ok1,0001791021600000,2026-10-03T12:00:00Z,view,p1,
e2,ok2,2026-10-03T10:00:00+00:00,2026-10-03T12:00:00Z,view,p1,
e3,ok3,2026-10-03T10:00:00-23:59,2026-10-03T12:00:00Z,view,p1,
e4,ok4,"2026-10-03T12:00:00+02:00",2026-10-03T12:00:00Z,view,p1,
e5,ok5,2026-10-02T23:30:00-10:00,2026-10-03T12:00:00Z,view,p1,
e6,ok6,2026-10-04T08:00:00+12:00,2026-10-03T12:00:00Z,view,p1,
f1,b01,2026-10-03T10:00:00.000Z,2026-10-03T12:00:00Z,view,p1,
f2,b02,2026-10-03T10:00:00z,2026-10-03T12:00:00Z,view,p1,
f3,b03,2026-10-03t10:00:00Z,2026-10-03T12:00:00Z,view,p1,
f4,b04,2026-10-03 10:00:00Z,2026-10-03T12:00:00Z,view,p1,
f5,b05,2026-10-03T10:00:00,2026-10-03T12:00:00Z,view,p1,
f6,b06,2026-10-03T10:00Z,2026-10-03T12:00:00Z,view,p1,
f7,b07,2026-10-03T10:00:00Z ,2026-10-03T12:00:00Z,view,p1,
f8,b08, 2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
f9,b09,2026-10-03T10:00:00+2:00,2026-10-03T12:00:00Z,view,p1,
f10,b10,2026-10-03T10:00:00+02,2026-10-03T12:00:00Z,view,p1,
f11,b11,2026-10-03T10:00:00+02:0,2026-10-03T12:00:00Z,view,p1,
f12,b12,2026-10-3T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
f13,b13,20261003T100000Z,2026-10-03T12:00:00Z,view,p1,
f14,b14,2026-10-03T10:00:00+00:00Z,2026-10-03T12:00:00Z,view,p1,
f15,b15,2026/10/03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
f16,b16,+1791021600000,2026-10-03T12:00:00Z,view,p1,
f17,b17,1791021600000 ,2026-10-03T12:00:00Z,view,p1,
f18,b18,1_791_021_600_000,2026-10-03T12:00:00Z,view,p1,
f19,b19,1.791e12,2026-10-03T12:00:00Z,view,p1,
f20,b20,0x1A1013D7E00,2026-10-03T12:00:00Z,view,p1,
f21,b21,2026-10-03T10:00:00−02:00,2026-10-03T12:00:00Z,view,p1,
f22,b22,2026-10-03T10:00:00+02:00:00,2026-10-03T12:00:00Z,view,p1,
f23,b23,2026-10-03T10:00:00-24:00,2026-10-03T12:00:00Z,view,p1,
f24,b24,2026-10-03T10:00:00Z0,2026-10-03T12:00:00Z,view,p1,
f25,b25,T10:00:00Z,2026-10-03T12:00:00Z,view,p1,
f26,b26,2026-10-03,2026-10-03T12:00:00Z,view,p1,
f27,b27,2026-10-03T25:00:00+02:00,2026-10-03T12:00:00Z,view,p1,
f28,b28,1791021600000Z,2026-10-03T12:00:00Z,view,p1,
'''},
     clicks={"part-00000.csv": '''
k1,ok1,c_1,0001791018000000
k2,ok2,c_1,1791018000000.0
k3,ok4,c_2,2026-10-03T09:00:00+00:00
k5,ok6,c_2,2026-10-03T19:59:59+00:00:00
k6,ok3,c_2,2026-10-03T23:59:00-10:00
'''},
     user_daily='''
ok1,2026-10-03,1,1,0,
ok2,2026-10-03,1,1,0,
ok3,2026-10-04,1,1,0,
ok4,2026-10-03,1,1,0,
ok5,2026-10-03,1,1,0,
ok6,2026-10-03,1,1,0,
''',
     campaign_daily='''
c_1,search,2026-10-03,1,0,0,1
c_2,social,2026-10-03,1,0,0,1
c_2,social,2026-10-04,1,0,0,1
organic,organic,2026-10-03,3,0,0,0
''')

# XT4. An F3 value is just a number: leading zeros never matter, and a number outside the
# window is bad however a fixed-width integer would wrap it.
#   ok1 22 zeros + 1791021600000 (35 digits)          = Oct 3 10:00:00.000    valid
#   ok2 18 zeros + 1791021600001 (31 digits)          = Oct 3 10:00:00.001    valid
#   b1  18446745864731151616 = 2^64 + 1791021600000   (wraps to ok1's instant in 64 bits)  bad
#   b2  36893489938440703232 = 2^65 + 1791021600000                                        bad
#   b3  9223372036854775807  = 2^63 - 1                                                    bad
#   b4  forty 9s          b5  0          b6  thirty 0s (the number 0)                      bad
#   b7  1791021600000000 (microseconds)  b8  1791021600 (seconds)                          bad
# Clicks: k1 (ok1, c_1) 25 zeros + 1791018000000 = 09:00:00 -> c_1.
#         k2 (ok2, c_2) 18446745864727551616 = 2^64 + 1791018000000 -> bad -> ok2 is organic.
ZEROS = "0"
case("XT4",
     """F3 values. Valid: `1791021600000` behind 22 leading zeros (35 digits) and `1791021600001`
     behind 18. Bad: 2^64 + 1791021600000 and 2^65 + 1791021600000 (both wrap to a valid instant
     in 64-bit arithmetic), 2^63 - 1, forty 9s, `0`, thirty 0s, the instant in microseconds and in
     seconds. Clicks: a valid `ts` behind 25 leading zeros; 2^64 + a valid click time""",
     """F3 is the number the digits spell: any count of leading zeros is fine, even past the
     length of a 64-bit integer, and a huge value is outside the window, never wrapped into it""",
     events={"part-00000.csv": "\n" + "".join([
         f"e1,ok1,{ZEROS * 22}1791021600000,2026-10-03T12:00:00Z,view,p1,\n",
         f"e2,ok2,{ZEROS * 18}1791021600001,2026-10-03T12:00:00Z,view,p1,\n",
         "e3,b1,18446745864731151616,2026-10-03T12:00:00Z,view,p1,\n",
         "e4,b2,36893489938440703232,2026-10-03T12:00:00Z,view,p1,\n",
         "e5,b3,9223372036854775807,2026-10-03T12:00:00Z,view,p1,\n",
         f"e6,b4,{'9' * 40},2026-10-03T12:00:00Z,view,p1,\n",
         "e7,b5,0,2026-10-03T12:00:00Z,view,p1,\n",
         f"e8,b6,{ZEROS * 30},2026-10-03T12:00:00Z,view,p1,\n",
         "e9,b7,1791021600000000,2026-10-03T12:00:00Z,view,p1,\n",
         "e10,b8,1791021600,2026-10-03T12:00:00Z,view,p1,\n"])},
     clicks={"part-00000.csv": "\n" + "".join([
         f"k1,ok1,c_1,{ZEROS * 25}1791018000000\n",
         "k2,ok2,c_2,18446745864727551616\n"])},
     user_daily='''
ok1,2026-10-03,1,1,0,
ok2,2026-10-03,1,1,0,
''',
     campaign_daily='''
c_1,search,2026-10-03,1,0,0,1
organic,organic,2026-10-03,1,0,0,0
''')

# ==========================================================================
group("Prices and categories",
      "Oct 3, one session per user unless noted.")
# ==========================================================================

# XV1. Purchases of p1 at 10:00, one user each. Valid: v1 `1`; v2 `10000000`; v3 `250` behind
# 28 leading zeros (31 digits); v4 `"00042"` (quoted) = 42. v5 has a view priced `abc`, a
# cart priced `-5` and a view priced with twenty-six 9s: prices of non-purchases are ignored.
# Bad, the whole event dropped: `0`, `00`, `10000001`, empty, `""`, `+500`, `-5`, `19.99`,
# `1e3`, ` 250`, `250 `, `1_000`, `0x10`, 18446744073709551866 (= 2^64 + 250), 4294967546
# (= 2^32 + 250), `250.0`, `100000000`.
# organic: 5 sessions, 4 with a purchase, 1 + 10000000 + 250 + 42 = 10000293.
case("XV1",
     """Purchases priced `1`, `10000000`, `250` behind 28 leading zeros, and a quoted `"00042"`
     (valid); `0`, `00`, `10000001`, empty, `""`, `+500`, `-5`, `19.99`, `1e3`, ` 250`, `250 `, `1_000`,
     `0x10`, 2^64 + 250, 2^32 + 250, `250.0`, `100000000` (dropped). One user with a view priced
     `abc`, a cart priced `-5` and a view priced with twenty-six 9s""",
     """A price is ASCII digits with a value from 1 to 10,000,000: both ends count, leading zeros
     of any length are fine, a value that only fits after wrapping in 32 or 64 bits is out of
     range, and signs, spaces, underscores and exponents are not digits; non-purchases ignore
     the field""",
     events={"part-00000.csv": "\n" + "".join([
         "e1,v1,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,1\n",
         "e2,v2,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,10000000\n",
         f"e3,v3,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,{ZEROS * 28}250\n",
         'e4,v4,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,"00042"\n',
         "e5,v5,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,view,p1,abc\n",
         "e6,v5,2026-10-03T10:01:00Z,2026-10-03T12:00:00Z,cart,p1,-5\n",
         f"e7,v5,2026-10-03T10:02:00Z,2026-10-03T12:00:00Z,view,p1,{'9' * 26}\n",
         "f1,b01,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,0\n",
         "f2,b02,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,00\n",
         "f3,b03,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,10000001\n",
         "f4,b04,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,\n",
         'f5,b05,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,""\n',
         "f6,b06,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,+500\n",
         "f7,b07,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,-5\n",
         "f8,b08,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,19.99\n",
         "f9,b09,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,1e3\n",
         "f10,b10,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1, 250\n",
         "f11,b11,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,250\x20\n",
         "f12,b12,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,1_000\n",
         "f13,b13,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,0x10\n",
         "f14,b14,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,18446744073709551866\n",
         "f15,b15,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,4294967546\n",
         "f16,b16,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,250.0\n",
         "f17,b17,2026-10-03T10:00:00Z,2026-10-03T12:00:00Z,purchase,p1,100000000\n"])},
     user_daily='''
v1,2026-10-03,1,1,1,shoes
v2,2026-10-03,1,1,10000000,shoes
v3,2026-10-03,1,1,250,shoes
v4,2026-10-03,1,1,42,shoes
v5,2026-10-03,1,3,0,
''',
     campaign_daily='''
organic,organic,2026-10-03,5,4,10000293,0
''')

# XV2. Products: p1 shoes, p2 books, p3 shoes, p4 Toys, p5 zebra. p9, p77, P1 and p01 are
# not in the file: unknown.
#   ua  p2 500, p1 300, p3 300    shoes 600 > books 500 (two products, one category)   shoes
#   ub  p1 200, p3 200, p2 500    books 500 > shoes 400 (sums, not purchase counts)    books
#   uc  p4 400, p2 400            tie; `Toys` < `books` ('T' < 'b')                    Toys
#   ud  p9 300, p5 300            tie; `unknown` < `zebra`                             unknown
#   ue  p9 300, p1 300            tie; `shoes` < `unknown`                             shoes
#   uf  P1 250, p01 250, p2 400   unknown 500 > books 400 (unknown products pool)      unknown
#   ug  Oct 3: 10:00 p2 500, 14:00 p1 300, 18:00 p3 300 (three sessions): shoes 600 >
#       books 500 over the day -> shoes.  Oct 4 10:00: p2 100 -> books
#   uh  a view of p2 priced 99999, a cart of p2 priced 9999, a purchase of p1 for 100  shoes
#   ui  p1 100, p2 100, p4 100    three-way tie                                        Toys
#   uj  one view                                                                       null
# organic Oct 3: 9 single sessions + ug's 3 = 12; all but uj's have a purchase = 11;
# 1100 + 900 + 800 + 600 + 600 + 900 + 1100 + 100 + 300 = 6400.  Oct 4: 1, 1, 100.
case("XV2",
     """Products p1 and p3 (shoes), p2 (books), p4 (`Toys`), p5 (zebra); `p9`, `P1`, `p01` unknown.
     Ten users: shoes 300 + 300 against books 500; shoes 200 + 200 against books 500; ties between
     `Toys` and books, unknown and zebra, unknown and shoes, and a three-way one; unknown 250 + 250
     against books 400;
     one user with three sessions on Oct 3 (books 500, shoes 300, shoes 300) and one on Oct 4; a
     view and a cart with large prices beside a purchase of 100; a user with no purchase""",
     """The top category is the highest sum of purchase cents over the person's day: across
     products, purchases and sessions, never a count or a single largest purchase; ties go to
     the byte-wise smallest (`Toys` before `books`); `unknown` pools every unknown product and
     competes like any name; views and carts add nothing; no purchase gives null""",
     products="product_id,category\np1,shoes\np2,books\np3,shoes\np4,Toys\np5,zebra\n",
     events={"part-00000.csv": '''
e01,ua,2026-10-03T10:00:00Z,2026-10-03T22:00:00Z,purchase,p2,500
e02,ua,2026-10-03T10:01:00Z,2026-10-03T22:00:00Z,purchase,p1,300
e03,ua,2026-10-03T10:02:00Z,2026-10-03T22:00:00Z,purchase,p3,300
e04,ub,2026-10-03T10:00:00Z,2026-10-03T22:00:00Z,purchase,p1,200
e05,ub,2026-10-03T10:01:00Z,2026-10-03T22:00:00Z,purchase,p3,200
e06,ub,2026-10-03T10:02:00Z,2026-10-03T22:00:00Z,purchase,p2,500
e07,uc,2026-10-03T10:00:00Z,2026-10-03T22:00:00Z,purchase,p4,400
e08,uc,2026-10-03T10:01:00Z,2026-10-03T22:00:00Z,purchase,p2,400
e09,ud,2026-10-03T10:00:00Z,2026-10-03T22:00:00Z,purchase,p9,300
e10,ud,2026-10-03T10:01:00Z,2026-10-03T22:00:00Z,purchase,p5,300
e11,ue,2026-10-03T10:00:00Z,2026-10-03T22:00:00Z,purchase,p9,300
e12,ue,2026-10-03T10:01:00Z,2026-10-03T22:00:00Z,purchase,p1,300
e13,uf,2026-10-03T10:00:00Z,2026-10-03T22:00:00Z,purchase,P1,250
e14,uf,2026-10-03T10:01:00Z,2026-10-03T22:00:00Z,purchase,p01,250
e15,uf,2026-10-03T10:02:00Z,2026-10-03T22:00:00Z,purchase,p2,400
e16,ug,2026-10-03T10:00:00Z,2026-10-03T22:00:00Z,purchase,p2,500
e17,ug,2026-10-03T14:00:00Z,2026-10-03T22:00:00Z,purchase,p1,300
e18,ug,2026-10-03T18:00:00Z,2026-10-03T22:00:00Z,purchase,p3,300
e19,ug,2026-10-04T10:00:00Z,2026-10-04T12:00:00Z,purchase,p2,100
e20,uh,2026-10-03T10:00:00Z,2026-10-03T22:00:00Z,view,p2,99999
e21,uh,2026-10-03T10:01:00Z,2026-10-03T22:00:00Z,cart,p2,9999
e22,uh,2026-10-03T10:02:00Z,2026-10-03T22:00:00Z,purchase,p1,100
e23,ui,2026-10-03T10:00:00Z,2026-10-03T22:00:00Z,purchase,p1,100
e24,ui,2026-10-03T10:01:00Z,2026-10-03T22:00:00Z,purchase,p2,100
e25,ui,2026-10-03T10:02:00Z,2026-10-03T22:00:00Z,purchase,p4,100
e26,uj,2026-10-03T10:00:00Z,2026-10-03T22:00:00Z,view,p1,
'''},
     user_daily='''
ua,2026-10-03,1,3,1100,shoes
ub,2026-10-03,1,3,900,books
uc,2026-10-03,1,2,800,Toys
ud,2026-10-03,1,2,600,unknown
ue,2026-10-03,1,2,600,shoes
uf,2026-10-03,1,3,900,unknown
ug,2026-10-03,3,3,1100,shoes
ug,2026-10-04,1,1,100,books
uh,2026-10-03,1,3,100,shoes
ui,2026-10-03,1,3,300,Toys
uj,2026-10-03,1,1,0,
''',
     campaign_daily='''
organic,organic,2026-10-03,12,11,6400,0
organic,organic,2026-10-04,1,1,100,0
''')

# XV3. Written by a loop: 430 purchases of p1 for 10000000 by u1, one per second from
# 1791021600000 (Oct 3 10:00:00) to 1791022029000 (10:07:09). One session of 430 events;
# 430 x 10,000,000 = 4,300,000,000 cents, above 2^32 = 4,294,967,296.
case("XV3",
     """430 purchases of p1 for `10000000` by `u1`, one per second from 10:00:00 (F3 `1791021600000`
     + 1000 per event). Written by a loop in `checks/make_extra_fixtures.py`""",
     """Revenue sums need more than 32 bits: 4,300,000,000 in the user row, the campaign row and
     the category total""",
     events={"part-00000.csv": "\n" + "".join(
         f"e{index},u1,{1791021600000 + index * 1000},2026-10-03T12:00:00Z,purchase,p1,10000000\n"
         for index in range(430))},
     user_daily='''
u1,2026-10-03,1,430,4300000000,shoes
''',
     campaign_daily='''
organic,organic,2026-10-03,1,1,4300000000,0
''')


# --------------------------------------------------------------------------

README_HEAD = """# Extra fixtures

Hand-written cases in the layout of `fixtures/`: each has an `input/` folder laid out like `/data` and an `expected/` folder with the two tables as CSV (`day` as `YYYY-MM-DD`, an empty `top_category` for null). They cover what the 36 official cases leave thin. Every expected row was derived by hand from `rules.md` first (the derivation is the comment above each case in `checks/make_extra_fixtures.py`, which writes these files) and then confirmed by the independent reference `checks/pyref.py`.

```sh
python scripts/run_fixtures.py extra_fixtures -- CMD...      # the pipeline, Parquet against expected
python checks/pyref_fixtures.py extra_fixtures               # the reference, CSV against expected
```

Unless noted, a case uses the official fixtures' setup: a window of 2026-10-01 to 2026-10-15, the products `p1` (shoes) and `p2` (books), the campaigns `c_1` (search) and `c_2` (social), no links, events ingested within 24 h. Timestamps written with milliseconds are F3 in the files. Inputs are byte-exact test data: some hold `\\r`, trailing spaces, or no final newline (the repo's `.gitattributes` keeps Git from converting them).

Cases that go beyond what the generator writes, but not beyond the rules: `XL4` and `XC4` (`\\r`), `XL6` (non-ASCII IDs), `XC2` (other file names, non-CSV files), `XC3` (no final newline), `XB2` and `XB5` (a window that does not start at midnight; one that is not 14 days long), `XM1` and `XM2` (manifest layout). `XB4`, `XV3` and the zero padding in `XT4`/`XV1` are written by a loop; everything else is literal.
"""


def write_readme():
    lines = [README_HEAD]
    for title, intro in GROUPS:
        lines.append(f"## {title}\n")
        if intro:
            lines.append(f"{intro}\n")
        lines.append("| Case | Input | Catches |")
        lines.append("| --- | --- | --- |")
        for entry in sorted(CASES, key=lambda entry: entry["name"]):
            if entry["group"] == title:
                assert "|" not in entry["summary"] and "|" not in entry["catches"], entry["name"]
                lines.append(f"| {entry['name']} | {entry['summary']} | {entry['catches']} |")
        lines.append("")
    with open(os.path.join(OUT, "README.md"), "wb") as handle:
        handle.write("\n".join(lines).encode("utf-8"))


def self_check(entry):
    """The rules' own quick self-check on the hand-written expected tables."""
    def rows(name):
        body = entry["expected"][name].decode("utf-8").split("\n")[1:-1]
        return [line.split(",") for line in body]
    users, campaigns = rows("user_daily.csv"), rows("campaign_daily.csv")
    assert all(len(row) == 6 for row in users) and all(len(row) == 7 for row in campaigns), entry["name"]
    assert sum(int(r[2]) for r in users) == sum(int(r[3]) for r in campaigns), f"{entry['name']}: sessions"
    assert sum(int(r[4]) for r in users) == sum(int(r[5]) for r in campaigns), f"{entry['name']}: revenue"
    credited = sum(int(r[3]) for r in campaigns if r[0] != "organic")
    assert sum(int(r[6]) for r in campaigns) >= credited, f"{entry['name']}: assists"
    assert all(int(r[6]) == 0 for r in campaigns if r[0] == "organic"), f"{entry['name']}: organic assists"
    assert all(int(r[4]) <= int(r[3]) for r in campaigns), f"{entry['name']}: purchasing sessions"
    assert len({(r[0], r[1]) for r in users}) == len(users), f"{entry['name']}: duplicate user rows"
    assert len({(r[0], r[2]) for r in campaigns}) == len(campaigns), f"{entry['name']}: duplicate campaign rows"


def main():
    names = [entry["name"] for entry in CASES]
    assert len(names) == len(set(names)), "duplicate case names"
    xr1a, xr1b = (next(e for e in CASES if e["name"] == name) for name in ("XR1a", "XR1b"))
    assert xr1a["expected"] == xr1b["expected"]
    for entry in CASES:
        self_check(entry)
    os.makedirs(OUT, exist_ok=True)
    for name in os.listdir(OUT):                 # drop cases that no longer exist
        path = os.path.join(OUT, name)
        if os.path.isdir(path):
            shutil.rmtree(path)
    for entry in CASES:
        for folder, files in (("input", entry["files"]), ("expected", entry["expected"])):
            for relative, content in files.items():
                path = os.path.join(OUT, entry["name"], folder, *relative.split("/"))
                os.makedirs(os.path.dirname(path), exist_ok=True)
                with open(path, "wb") as handle:
                    handle.write(content)
    write_readme()
    print(f"wrote {len(CASES)} cases to {OUT}")


if __name__ == "__main__":
    sys.exit(main())
