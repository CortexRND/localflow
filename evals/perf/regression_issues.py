"""Open, update and close ``perf-regression`` issues from a regressions report.

Stdlib only (docs/plans/PLAN-perf-evals.md §8). One issue per ``metric_key``,
found through the hidden ``<!-- lf-perf-regression: <metric_key> -->`` marker
in its body, so reruns are idempotent.

``plan_issue_actions`` is pure; ``sync_github_issues`` applies a plan through
the REST API with ``urllib.request``.

CLI::

    python -m evals.perf.regression_issues --report regressions.json \\
        --repo owner/name [--dry-run] [--existing issues.json] [--output plan.json]

The token comes from ``$GITHUB_TOKEN`` (or ``$GH_TOKEN``). ``--dry-run``
prints the plan without writing; it still lists issues when a token is set,
unless ``--existing`` supplies them.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

LABEL = "perf-regression"
LABEL_COLOR = "d93f0b"
API_URL = "https://api.github.com"
DEFAULT_MAX_OPEN = 20
MARKER_RE = re.compile(r"<!--\s*lf-perf-regression:\s*(?P<key>[^\s>]+)\s*-->")

EXIT_OK = 0
EXIT_FAILED = 1
EXIT_INVALID = 2


def marker(key: str) -> str:
    return f"<!-- lf-perf-regression: {key} -->"


def issue_key(issue: dict) -> str | None:
    match = MARKER_RE.search(str(issue.get("body") or ""))
    return match.group("key") if match else None


def _with_marker(key: str, body: str) -> str:
    return body if marker(key) in body else f"{marker(key)}\n{body}"


def _index_issues(existing: list[dict]) -> dict[str, dict]:
    """``metric_key -> issue``; an open issue wins over closed ones, then the newest."""
    by_key: dict[str, dict] = {}
    for issue in existing:
        if "pull_request" in issue:  # the issues API also lists PRs
            continue
        key = issue_key(issue)
        if key is None:
            continue
        cur = by_key.get(key)
        rank = (issue.get("state") == "open", int(issue.get("number") or 0))
        if cur is None or rank > (cur.get("state") == "open", int(cur.get("number") or 0)):
            by_key[key] = issue
    return by_key


def plan_issue_actions(
    regressions_report: dict,
    existing_issues: list[dict],
    *,
    max_open: int | None = DEFAULT_MAX_OPEN,
) -> list[dict]:
    """Plan issue writes for ``regressions_report`` given the current issues.

    * active regression, no issue           -> ``open``
    * active regression, open issue         -> ``update`` (skipped if the body is unchanged)
    * active regression, only closed issue  -> ``update`` with ``reopen: True``
    * recovered, open issue                 -> ``close`` with a recovery comment
    * recovered, closed or no issue         -> nothing

    ``max_open`` caps new issues per run so a runner-wide slowdown does not
    open one issue per clip; the overflow is listed in one ``skipped`` action.
    """
    issues = _index_issues(existing_issues)
    actions: list[dict] = []
    overflow: list[str] = []
    opened = 0
    for item in regressions_report.get("active_regressions") or []:
        key = item["metric_key"]
        body = _with_marker(key, item.get("body") or "")
        title = item.get("title") or f"Perf regression: {key}"
        issue = issues.get(key)
        if issue is None:
            if max_open is not None and opened >= max_open:
                overflow.append(key)
                continue
            opened += 1
            actions.append({"action": "open", "metric_key": key, "title": title, "body": body, "labels": [LABEL]})
        elif issue.get("state") == "open":
            if issue.get("body") == body and issue.get("title") == title:
                continue
            actions.append({
                "action": "update", "metric_key": key, "number": issue["number"],
                "title": title, "body": body, "reopen": False,
            })
        else:
            actions.append({
                "action": "update", "metric_key": key, "number": issue["number"],
                "title": title, "body": body, "reopen": True,
            })
    for item in regressions_report.get("recovered") or []:
        key = item["metric_key"]
        issue = issues.get(key)
        if issue is None or issue.get("state") != "open":
            continue
        comment = item.get("body") or f"{marker(key)}\nRecovered in the latest nightly run."
        actions.append({"action": "close", "metric_key": key, "number": issue["number"], "comment": comment})
    if overflow:
        actions.append({"action": "skipped", "reason": f"max_open {max_open} reached", "metric_keys": overflow})
    return actions


# --------------------------------------------------------------------------
# GitHub REST
# --------------------------------------------------------------------------


class GitHubClient:
    def __init__(self, repo: str, token: str, api_url: str = API_URL, timeout: float = 30.0) -> None:
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo or ""):
            raise ValueError(f"invalid repo {repo!r} (expected owner/name)")
        self.repo = repo
        self.token = token
        self.api_url = api_url.rstrip("/")
        self.timeout = timeout

    def request(self, method: str, path: str, payload: dict | None = None) -> Any:
        url = path if path.startswith("http") else f"{self.api_url}/repos/{self.repo}{path}"
        data = json.dumps(payload).encode("utf-8") if payload is not None else None
        req = urllib.request.Request(url, data=data, method=method)
        req.add_header("Accept", "application/vnd.github+json")
        req.add_header("X-GitHub-Api-Version", "2022-11-28")
        req.add_header("User-Agent", "lf-perf-evals")
        if self.token:
            req.add_header("Authorization", f"Bearer {self.token}")
        if data is not None:
            req.add_header("Content-Type", "application/json")
        with urllib.request.urlopen(req, timeout=self.timeout) as resp:
            raw = resp.read()
        return json.loads(raw) if raw else None

    def list_issues(self, label: str = LABEL) -> list[dict]:
        out: list[dict] = []
        query = urllib.parse.urlencode({"labels": label, "state": "all", "per_page": 100})
        for page in range(1, 51):
            batch = self.request("GET", f"/issues?{query}&page={page}") or []
            out.extend(batch)
            if len(batch) < 100:
                break
        return out

    def ensure_label(self, label: str = LABEL) -> None:
        try:
            self.request("POST", "/labels", {
                "name": label, "color": LABEL_COLOR,
                "description": "Nightly perf metric regressed in consecutive runs",
            })
        except urllib.error.HTTPError as exc:
            if exc.code != 422:  # 422: already exists
                raise


def sync_github_issues(
    actions: list[dict],
    repo: str,
    token: str,
    *,
    dry_run: bool = False,
    client: GitHubClient | None = None,
) -> list[dict]:
    """Apply ``actions``; returns one ``{action, metric_key, ok, ...}`` per action."""
    results: list[dict] = []
    if dry_run:
        return [{**a, "ok": True, "dry_run": True} for a in actions]
    gh = client or GitHubClient(repo, token)
    if any(a["action"] == "open" for a in actions):
        try:
            gh.ensure_label()
        except (urllib.error.URLError, OSError) as exc:
            print(f"warning: could not ensure label {LABEL!r}: {exc}", file=sys.stderr)
    for action in actions:
        kind = action["action"]
        try:
            if kind == "open":
                created = gh.request("POST", "/issues", {
                    "title": action["title"], "body": action["body"], "labels": action["labels"],
                })
                results.append({**action, "ok": True, "number": (created or {}).get("number")})
            elif kind == "update":
                payload: dict[str, Any] = {"title": action["title"], "body": action["body"]}
                if action.get("reopen"):
                    payload["state"] = "open"
                gh.request("PATCH", f"/issues/{int(action['number'])}", payload)
                results.append({**action, "ok": True})
            elif kind == "close":
                number = int(action["number"])
                gh.request("POST", f"/issues/{number}/comments", {"body": action["comment"]})
                gh.request("PATCH", f"/issues/{number}", {"state": "closed", "state_reason": "completed"})
                results.append({**action, "ok": True})
            else:
                results.append({**action, "ok": True})
        except (urllib.error.URLError, OSError, ValueError) as exc:
            results.append({**action, "ok": False, "error": f"{type(exc).__name__}: {exc}"})
    return results


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def _load_json(path: Path, label: str) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        raise ValueError(f"{label}: cannot read {path}: {exc}") from exc


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="python -m evals.perf.regression_issues",
        description="Open/update/close perf-regression issues from `python -m evals.perf regressions` output.",
    )
    parser.add_argument("--report", type=Path, required=True, help="regressions.json")
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""), help="owner/name")
    parser.add_argument("--existing", type=Path, help="issues JSON list (instead of listing via the API)")
    parser.add_argument("--dry-run", action="store_true", help="print the plan, write nothing")
    parser.add_argument("--max-open", type=int, default=DEFAULT_MAX_OPEN, help="new issues per run")
    parser.add_argument("--output", type=Path, help="write the plan/results JSON here")
    try:
        args = parser.parse_args(argv)
    except SystemExit as exc:
        return int(exc.code or 0)

    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN") or ""
    try:
        report = _load_json(args.report, "report")
        if not isinstance(report, dict):
            raise TypeError(f"report: {args.report} is not a JSON object")
        if args.existing:
            existing = _load_json(args.existing, "existing")
            if not isinstance(existing, list):
                raise TypeError(f"existing: {args.existing} is not a JSON list")
        elif token and args.repo:
            existing = GitHubClient(args.repo, token).list_issues()
        elif args.dry_run:
            existing = []
        else:
            raise ValueError("need --repo and $GITHUB_TOKEN (or --existing / --dry-run)")
    except (TypeError, ValueError, urllib.error.URLError, OSError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return EXIT_INVALID

    actions = plan_issue_actions(report, existing, max_open=args.max_open)
    if not args.dry_run and not (token and args.repo):
        print("error: need --repo and $GITHUB_TOKEN to apply the plan (or --dry-run)", file=sys.stderr)
        return EXIT_INVALID
    results = sync_github_issues(actions, args.repo, token, dry_run=args.dry_run)
    text = json.dumps(results, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(text, encoding="utf-8")
    print(text, end="")
    for r in results:
        num = f" #{r['number']}" if r.get("number") else ""
        state = "ok" if r.get("ok") else f"FAILED ({r.get('error')})"
        print(f"{r['action']}{num} {r.get('metric_key', '')}: {state}", file=sys.stderr)
    return EXIT_OK if all(r.get("ok") for r in results) else EXIT_FAILED


if __name__ == "__main__":
    sys.exit(main())
