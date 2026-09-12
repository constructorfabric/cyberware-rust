#!/usr/bin/env python3
"""Repository and blob access for toolkit-pr-review.

Everything here is addressed by immutable SHA. The working tree is never read: it is
whatever branch the operator happens to be sitting on, it does not contain files the PR
adds, and it contains files the PR deletes. The prose this replaces sanctioned `wc -c`
on the checkout for shard weights and read it again for ToolKit classification, so a
fork PR was weighed and classified against unrelated code.

Blob reads try the local object database first and fall back to the GitHub contents API,
recording which was used in `blob_source` so a surprising review can be traced.
"""

from __future__ import annotations

import base64
import json
import subprocess
from urllib.parse import quote


class SourceError(RuntimeError):
    pass


def run(cmd: list[str], check: bool = True) -> str:
    p = subprocess.run(cmd, capture_output=True, text=True)
    if check and p.returncode != 0:
        raise SourceError(f"{' '.join(cmd[:3])}...: {p.stderr.strip()[:300]}")
    return p.stdout


def resolve_repo(explicit: str | None = None) -> str:
    if explicit:
        return explicit
    for remote in ("upstream", "origin"):
        out = run(["git", "remote", "get-url", remote], check=False).strip()
        if out:
            slug = out.split("github.com", 1)[-1].lstrip(":/")
            return slug[:-4] if slug.endswith(".git") else slug
    out = run(["gh", "repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"]).strip()
    if not out:
        raise SourceError("could not resolve the repository; pass --repo owner/name")
    return out


def fetch_pr_meta(pr: int, repo: str) -> dict:
    """PR metadata including **both** SHAs.

    `baseRefOid` is what makes a deleted file readable: it does not exist at the head, so
    its pre-deletion content can only come from the base commit. Omitting it is why three
    places in the prose referred to a base SHA that was never fetched.
    """
    out = run([
        "gh", "pr", "view", str(pr), "--repo", repo, "--json",
        "number,title,body,headRefOid,baseRefOid,baseRefName,headRefName,isCrossRepository",
    ])
    return json.loads(out)


def pr_diff(pr: int, repo: str) -> str:
    return run(["gh", "pr", "diff", str(pr), "--repo", repo])


def ensure_objects(repo: str, pr: int | None, shas: list[str]) -> bool:
    """Try to make `shas` readable from the local object database.

    Returns True when every SHA resolves locally afterwards. A PR from a fork is the
    common failure: its head commit is not on any local remote until `pull/N/head` is
    fetched, and `git show <sha>:<path>` fails with "bad object" until then.
    """
    def have(sha: str) -> bool:
        return subprocess.run(
            ["git", "cat-file", "-e", f"{sha}^{{commit}}"],
            capture_output=True,
        ).returncode == 0

    if all(have(s) for s in shas):
        return True
    remote = "upstream"
    if not run(["git", "remote", "get-url", remote], check=False).strip():
        remote = "origin"
    if pr is not None:
        run(["git", "fetch", "--no-tags", "--quiet", remote,
             f"pull/{pr}/head:refs/pr-review/{pr}/head"], check=False)
    for sha in shas:
        if not have(sha):
            run(["git", "fetch", "--no-tags", "--quiet", remote, sha], check=False)
    return all(have(s) for s in shas)


def read_blob_git(sha: str, path: str) -> bytes | None:
    p = subprocess.run(["git", "cat-file", "-p", f"{sha}:{path}"], capture_output=True)
    return p.stdout if p.returncode == 0 else None


def read_blob_api(repo: str, sha: str, path: str) -> bytes | None:
    p = subprocess.run(
        ["gh", "api", f"repos/{repo}/contents/{quote(path, safe='/')}?ref={sha}",
         "-q", ".content"],
        capture_output=True, text=True,
    )
    if p.returncode != 0 or not p.stdout.strip():
        return None
    try:
        return base64.b64decode(p.stdout)
    except ValueError:
        return None


def blob_size_git(sha: str, path: str) -> int | None:
    p = subprocess.run(["git", "cat-file", "-s", f"{sha}:{path}"], capture_output=True, text=True)
    if p.returncode != 0:
        return None
    try:
        return int(p.stdout.strip())
    except ValueError:
        return None


def local_base(base_ref: str | None, branch: str) -> str:
    """Merge base for local mode, with a base that is discovered rather than assumed.

    The prose hardcoded `main` then `origin/main` then stopped, so a repository whose
    trunk is `master` could not use local mode at all.
    """
    candidates = [base_ref] if base_ref else []
    head = run(["git", "symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"],
               check=False).strip()
    if head:
        candidates.append(head)
    candidates += ["main", "origin/main", "master", "origin/master"]
    for cand in candidates:
        if not cand:
            continue
        if subprocess.run(["git", "rev-parse", "--verify", "--quiet", cand],
                          capture_output=True).returncode == 0:
            mb = run(["git", "merge-base", cand, branch], check=False).strip()
            if mb:
                return mb
    raise SourceError(
        "could not resolve a base ref; pass --base with a branch or commit that exists locally"
    )
