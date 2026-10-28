"""Repository snapshot held in memory for read-only exploration, editing and diffing.

The agent never executes repository code on the host. It reads source text here,
and every command that runs the code goes through a sandbox.
"""

from __future__ import annotations

import difflib
import fnmatch
import io
import re
import tarfile
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath

import httpx

MAX_FILE_BYTES = 256_000
SKIP_DIRS = {".git", ".hg", "node_modules", "__pycache__", ".venv", "venv", ".tox", ".mypy_cache", "dist", "build"}

_ISSUE_RE = re.compile(r"github\.com/([^/\s]+)/([^/\s]+)/issues/(\d+)")
_REPO_RE = re.compile(r"github\.com[/:]([^/\s]+)/([^/\s#?]+)")


@dataclass
class Issue:
    title: str
    body: str
    url: str | None = None
    number: int | None = None
    comments: list[str] = field(default_factory=list)

    def as_prompt(self) -> str:
        parts = [f"Title: {self.title}", "", self.body.strip() or "(no description)"]
        for i, comment in enumerate(self.comments, 1):
            parts += ["", f"--- comment {i} ---", comment.strip()]
        return "\n".join(parts)


@dataclass
class RepoRef:
    """Where the code comes from: a GitHub repo at a commit, or a local directory."""

    label: str
    owner: str | None = None
    name: str | None = None
    sha: str | None = None
    local_path: Path | None = None

    @property
    def tarball_url(self) -> str:
        return f"https://codeload.github.com/{self.owner}/{self.name}/tar.gz/{self.sha}"


class EditError(ValueError):
    pass


@dataclass
class Repo:
    files: dict[str, str]
    ref: RepoRef

    def paths(self) -> list[str]:
        return sorted(self.files)

    def tree(self, limit: int = 1500) -> str:
        paths = self.paths()
        shown = paths[:limit]
        extra = f"\n... and {len(paths) - limit} more files" if len(paths) > limit else ""
        return "\n".join(shown) + extra

    def read(self, path: str, start: int | None = None, end: int | None = None) -> str:
        path = normalize_path(path)
        if path not in self.files:
            raise KeyError(path)
        lines = self.files[path].splitlines()
        start = max(1, start or 1)
        end = min(len(lines), end or len(lines))
        width = len(str(end))
        return "\n".join(f"{n:>{width}}  {lines[n - 1]}" for n in range(start, end + 1))

    def grep(self, pattern: str, glob: str | None = None, limit: int = 60) -> list[str]:
        try:
            regex = re.compile(pattern)
        except re.error:
            regex = re.compile(re.escape(pattern))
        hits = []
        for path in self.paths():
            if glob and not fnmatch.fnmatch(path, glob):
                continue
            for n, line in enumerate(self.files[path].splitlines(), 1):
                if regex.search(line):
                    hits.append(f"{path}:{n}: {line.strip()[:200]}")
                    if len(hits) >= limit:
                        return hits
        return hits

    def test_dirs(self) -> list[str]:
        dirs = {str(PurePosixPath(p).parent) for p in self.files if PurePosixPath(p).name.startswith("test")}
        return sorted(d for d in dirs if d != ".") or ["."]

    def with_edits(self, edits: list[tuple[str, str, str]]) -> tuple[Repo, list[str]]:
        """Apply SEARCH/REPLACE edits; returns the new repo and the changed paths."""
        files = dict(self.files)
        changed: list[str] = []
        for path, search, replace in edits:
            path = normalize_path(path)
            if path not in files:
                if search.strip():
                    raise EditError(f"{path} does not exist (to create a file, leave SEARCH empty)")
                files[path] = replace
            else:
                files[path] = _replace_once(files[path], search, replace, path)
            if path not in changed:
                changed.append(path)
        return Repo(files=files, ref=self.ref), changed

    def diff_against(self, base: Repo, paths: list[str]) -> str:
        chunks = []
        for path in sorted(paths):
            before = base.files.get(path)
            after = self.files.get(path)
            if before == after:
                continue
            chunks.append("".join(difflib.unified_diff(
                (before or "").splitlines(keepends=True),
                (after or "").splitlines(keepends=True),
                fromfile="/dev/null" if before is None else f"a/{path}",
                tofile=f"b/{path}",
            )))
        return "".join(c if c.endswith("\n") else c + "\n" for c in chunks)


def normalize_path(path: str) -> str:
    path = path.strip().strip("`").removeprefix("./")
    clean = PurePosixPath(path)
    if clean.is_absolute() or ".." in clean.parts:
        raise EditError(f"path must be relative to the repo root: {path}")
    return str(clean)


def _replace_once(text: str, search: str, replace: str, path: str) -> str:
    if not search.strip():
        raise EditError(f"SEARCH block for existing file {path} is empty")
    count = text.count(search)
    if count == 1:
        return text.replace(search, replace, 1)
    if count > 1:
        raise EditError(f"SEARCH block matches {count} places in {path}; include more surrounding lines")
    # Tolerate trailing-whitespace and indentation-preserving differences line by line.
    lines = text.splitlines(keepends=True)
    needle = [line.rstrip() for line in search.strip("\n").splitlines()]
    matches = [
        i for i in range(len(lines) - len(needle) + 1)
        if [line.rstrip() for line in lines[i : i + len(needle)]] == needle
    ]
    if len(matches) == 1:
        i = matches[0]
        tail = "\n" if lines[i + len(needle) - 1].endswith("\n") else ""
        return "".join(lines[:i]) + replace.strip("\n") + tail + "".join(lines[i + len(needle):])
    raise EditError(f"SEARCH block not found in {path}; copy the existing lines exactly")


# ---------------------------------------------------------------------------
# Loading

def _is_text(data: bytes) -> bool:
    return b"\0" not in data[:4096]


def _keep(path: str) -> bool:
    return not any(part in SKIP_DIRS for part in PurePosixPath(path).parts)


def load_local(path: Path) -> Repo:
    root = path.resolve()
    files = {}
    for file in sorted(root.rglob("*")):
        rel = file.relative_to(root).as_posix()
        if not file.is_file() or not _keep(rel) or file.stat().st_size > MAX_FILE_BYTES:
            continue
        data = file.read_bytes()
        if _is_text(data):
            files[rel] = data.decode("utf-8", errors="replace")
    return Repo(files=files, ref=RepoRef(label=root.name, local_path=root))


def _gh_headers(token: str | None) -> dict[str, str]:
    headers = {"Accept": "application/vnd.github+json", "User-Agent": "failfirst"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def parse_issue_url(url: str) -> tuple[str, str, int] | None:
    match = _ISSUE_RE.search(url)
    if not match:
        return None
    return match.group(1), match.group(2), int(match.group(3))


def parse_repo_url(url: str) -> tuple[str, str] | None:
    match = _REPO_RE.search(url)
    if not match:
        return None
    return match.group(1), match.group(2).removesuffix(".git")


async def fetch_issue(owner: str, name: str, number: int, token: str | None) -> Issue:
    async with httpx.AsyncClient(headers=_gh_headers(token), timeout=30) as client:
        base = f"https://api.github.com/repos/{owner}/{name}/issues/{number}"
        issue = (await client.get(base)).raise_for_status().json()
        comments = []
        if issue.get("comments"):
            data = (await client.get(f"{base}/comments", params={"per_page": 10})).raise_for_status().json()
            comments = [c.get("body") or "" for c in data]
    return Issue(
        title=issue["title"],
        body=issue.get("body") or "",
        url=issue.get("html_url"),
        number=number,
        comments=comments,
    )


async def fetch_github_repo(owner: str, name: str, token: str | None, ref: str = "HEAD") -> Repo:
    async with httpx.AsyncClient(headers=_gh_headers(token), timeout=60, follow_redirects=True) as client:
        commit = (await client.get(f"https://api.github.com/repos/{owner}/{name}/commits/{ref}")).raise_for_status().json()
        sha = commit["sha"]
        repo_ref = RepoRef(label=f"{owner}/{name}@{sha[:7]}", owner=owner, name=name, sha=sha)
        archive = (await client.get(repo_ref.tarball_url)).raise_for_status().content

    files = {}
    with tarfile.open(fileobj=io.BytesIO(archive), mode="r:gz") as tar:
        for member in tar:
            if not member.isfile() or member.size > MAX_FILE_BYTES:
                continue
            rel = "/".join(member.name.split("/")[1:])  # strip "<repo>-<sha>/"
            if not rel or not _keep(rel):
                continue
            data = tar.extractfile(member).read()
            if _is_text(data):
                files[rel] = data.decode("utf-8", errors="replace")
    return Repo(files=files, ref=repo_ref)
