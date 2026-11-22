#!/usr/bin/env python3
"""One sticky pull-request comment per commit SHA, holding the reports of that commit's CI jobs.

    sticky_comment.py --sha SHA --reports DIR --repo OWNER/NAME [--pr N | --branch NAME]
                      [--dry-run] [--summary FILE]

DIR holds the downloaded job artifacts (DIR/<artifact>/<job>.json, written by bench.py). The
comment starts with the SHA and the compact candidate line (bench/LEDGER.md format), now complete
because every job's result is at hand, followed by each job's report. It is found again by a
hidden marker naming the SHA, so a re-run updates it instead of adding another. Needs `gh` and a
token with pull-requests: write in GH_TOKEN. --summary also appends the SHA line and the compact
line to a file (the job's step summary).
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import report  # noqa: E402

ORDER = ["correctness", "bench-20m", "bench-60m"]
LIMIT = 64000  # GitHub rejects comment bodies over 65536 characters


def gh(*args, check=True):
    # GH names the GitHub CLI when it is not plain `gh` (a wrapper script, say: GH="bash path/to/ghx").
    command = shlex.split(os.environ.get("GH", "gh")) + list(args)
    done = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, errors="replace")
    if check and done.returncode != 0:
        raise RuntimeError(f"gh {' '.join(args[:4])} ...: {done.stderr.strip()[:500]}")
    return done


def marker(sha):
    return f"<!-- ss-ci-evidence:{sha} -->"


def load_results(reports_dir, sha=None):
    """One result per job. Several runs can have tested the same commit (a push, then a 60M run by
    hand): for bench-20m the result with the most paired evidence wins, otherwise the latest, so a
    later, smaller run adds its jobs to the comment without replacing a fuller report."""
    found = {}
    for path in sorted(Path(reports_dir).rglob("*.json")):
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if isinstance(data, dict) and data.get("job"):
            found.setdefault(data["job"], []).append(data)

    def weight(item):
        seeds = item.get("seeds") or [] if item.get("job") == "bench-20m" else []
        return (sum(1 for seed in seeds if seed.get("ab")), sum(1 for seed in seeds if seed.get("aa")), len(seeds),
                item.get("utc") or "")

    results = {}
    for job, items in found.items():
        same_commit = [item for item in items if item.get("sha") == sha] if sha else []
        results[job] = max(same_commit or items, key=weight)
    return results


def build_body(sha, reports_dir):
    """SHA first, then the compact line, then each job's report without its own first line."""
    results = load_results(reports_dir, sha)
    names = [name for name in ORDER if name in results] + sorted(name for name in results if name not in ORDER)
    if not names:
        return None, None
    correctness, b20, b60 = results.get("correctness"), results.get("bench-20m"), results.get("bench-60m")
    verdicts = " · ".join(f"{name} {report.verdict(results[name].get('ok'))}" for name in names)
    urls = []
    for name in names:
        url = results[name].get("run_url")
        if url and url not in urls:
            urls.append(url)
    runs = ", ".join(f"[Actions run {url.rsplit('/', 1)[-1]}]({url})" for url in urls)
    parts = [f"{sha} — CI evidence for this commit — {verdicts}" + (f" — {runs}" if runs else "")]
    other = [results[name].get("sha") for name in names if results[name].get("sha") and results[name].get("sha") != sha]
    if other:
        parts.append(f"**Warning: these results say they belong to `{other[0]}`, not to this commit.**")
    if b20:
        table = report.compact_table(b20, correctness, b60)
        if table:
            parts.append("\n".join(table))
    lead = "\n\n".join(parts)
    for name in names:
        text = report.render(results[name], correctness, b60, with_line=False).strip()
        parts.append(text.partition("\n")[2].strip())  # its first line repeats the SHA
    body = "\n\n".join(parts + [marker(sha)])
    if len(body) > LIMIT:  # drop the collapsible extras first, then cut
        body = re.sub(r"<details>.*?</details>", "(details omitted here for length; see the Actions run summary)", body, flags=re.S)
    if len(body) > LIMIT:
        keep = LIMIT - len(marker(sha)) - 200
        body = body[:keep] + "\n\n(truncated for length; the full report is in the Actions run summary)\n\n" + marker(sha)
    return body, lead


def open_pull_requests(repo, branch):
    done = gh("pr", "list", "--repo", repo, "--head", branch, "--state", "open", "--json", "number", "--jq", ".[].number")
    return [int(line) for line in done.stdout.split()]


def existing_comment(repo, number, sha):
    done = gh("api", "--paginate", f"repos/{repo}/issues/{number}/comments?per_page=100",
              "--jq", ".[] | {id, body} | @json")
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
    parser.add_argument("--summary", help="also write the comment body to this file")
    args = parser.parse_args()

    body, lead = build_body(args.sha, args.reports)
    if body is None:
        print(f"no results under {args.reports}: nothing to post")
        return 0
    print(f"comment body: {len(body)} characters, first line: {body.splitlines()[0][:200]}")
    if args.summary:  # the SHA and the completed compact line; the jobs' own summaries hold the rest
        with open(args.summary, "a", encoding="utf-8") as handle:
            handle.write(lead + "\n")
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
