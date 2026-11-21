#!/usr/bin/env python3
"""One sticky pull-request comment per commit SHA, holding the reports of that commit's CI jobs.

    sticky_comment.py --sha SHA --reports DIR --repo OWNER/NAME [--pr N | --branch NAME] [--dry-run]

DIR holds the downloaded job artifacts (DIR/<artifact>/<job>.md, written by report.py). The
comment is found again by a hidden marker naming the SHA, so a re-run or a later job updates it
instead of adding another. Needs `gh` and a token with pull-requests: write in GH_TOKEN.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ORDER = ["correctness", "bench-20m", "bench-60m"]
LIMIT = 64000  # GitHub rejects comment bodies over 65536 characters


def gh(*args, check=True):
    done = subprocess.run(["gh", *args], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, errors="replace")
    if check and done.returncode != 0:
        raise RuntimeError(f"gh {' '.join(args[:4])} ...: {done.stderr.strip()[:500]}")
    return done


def marker(sha):
    return f"<!-- ss-ci-evidence:{sha} -->"


def build_body(sha, reports_dir):
    """The comment: the SHA first, then each job's report (its own first line carries the same SHA)."""
    found = {path.stem: path for path in sorted(Path(reports_dir).rglob("*.md"))}
    names = [name for name in ORDER if name in found] + sorted(name for name in found if name not in ORDER)
    if not names:
        return None
    sections, verdicts = [], []
    for name in names:
        text = found[name].read_text(encoding="utf-8").strip()
        first, _, rest = text.partition("\n")
        fields = [field.strip() for field in first.split(" — ")]
        verdicts.append(f"{name} {fields[2] if len(fields) > 2 else '?'}")
        run_link = next((field for field in fields if field.startswith("[Actions run]")), "")
        sections.append((name, rest.strip(), run_link))
    run_link = next((link for _, _, link in sections if link), "")
    head = f"{sha} — CI evidence for this commit — " + " · ".join(verdicts) + (f" — {run_link}" if run_link else "")
    body = "\n\n".join([head] + [text for _, text, _ in sections] + [marker(sha)])
    if len(body) > LIMIT:  # drop the collapsible extras first, then cut
        body = re.sub(r"<details>.*?</details>", "(details omitted here for length; see the Actions run summary)", body, flags=re.S)
    if len(body) > LIMIT:
        keep = LIMIT - len(marker(sha)) - 200
        body = body[:keep] + "\n\n(truncated for length; the full report is in the Actions run summary)\n\n" + marker(sha)
    return body


def open_pull_requests(repo, branch):
    done = gh("pr", "list", "--repo", repo, "--head", branch, "--state", "open", "--json", "number", "--jq", ".[].number")
    return [int(line) for line in done.stdout.split()]


def existing_comment(repo, number, sha):
    done = gh("api", "--paginate", f"repos/{repo}/issues/{number}/comments?per_page=100",
              "--jq", ".[] | {id, body, login: .user.login} | @json")
    for line in done.stdout.splitlines():
        try:
            comment = json.loads(line)
        except ValueError:
            continue
        if marker(sha) in (comment.get("body") or ""):
            return comment["id"]
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--sha", required=True)
    parser.add_argument("--reports", required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--pr", type=int)
    parser.add_argument("--branch")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    body = build_body(args.sha, args.reports)
    if body is None:
        print(f"no reports under {args.reports}: nothing to post")
        return 0
    print(f"comment body: {len(body)} characters, first line: {body.splitlines()[0][:200]}")
    if args.pr:
        numbers = [args.pr]
    elif args.branch:
        numbers = open_pull_requests(args.repo, args.branch)
    else:
        numbers = []
    if not numbers:
        print(f"no open pull request for branch {args.branch!r}: nothing to post (the reports are in the run summary and artifacts)")
        return 0
    with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False, encoding="utf-8") as handle:
        handle.write(body)
        body_file = handle.name
    for number in numbers:
        comment_id = existing_comment(args.repo, number, args.sha)
        if args.dry_run:
            print(f"dry run: would {'update comment ' + str(comment_id) if comment_id else 'create a comment'} on pull request #{number}")
            continue
        if comment_id:
            done = gh("api", "--method", "PATCH", f"repos/{args.repo}/issues/comments/{comment_id}", "-F", f"body=@{body_file}",
                      "--jq", ".html_url")
            print(f"updated the comment for {args.sha[:7]} on pull request #{number}: {done.stdout.strip()}")
        else:
            done = gh("api", "--method", "POST", f"repos/{args.repo}/issues/{number}/comments", "-F", f"body=@{body_file}",
                      "--jq", ".html_url")
            print(f"posted the comment for {args.sha[:7]} on pull request #{number}: {done.stdout.strip()}")
    return 0


if __name__ == "__main__":
    sys.stdout.reconfigure(encoding="utf-8")
    try:
        sys.exit(main())
    except RuntimeError as error:
        print(f"::error::sticky comment: {error}")
        sys.exit(1)
