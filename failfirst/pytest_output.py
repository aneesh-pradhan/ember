"""Interpreting pytest runs."""

from __future__ import annotations

import re

from .sandbox import ExecResult

PYTEST = "python -m pytest -p no:cacheprovider -q -rfE"

_SUMMARY_RE = re.compile(r"^(FAILED|ERROR) (\S+)", re.MULTILINE)

EXIT_MEANING = {
    0: "all tests passed",
    1: "some tests failed",
    2: "test run interrupted (usually a collection or import error)",
    3: "pytest internal error",
    4: "pytest usage error",
    5: "no tests were collected",
}


def failing_ids(output: str) -> set[str]:
    """Test ids listed in pytest's short summary (`-rfE`)."""
    return {m.group(2) for m in _SUMMARY_RE.finditer(output)}


def describe(result: ExecResult) -> str:
    if result.timed_out:
        return "timed out"
    return EXIT_MEANING.get(result.exit_code, f"exit code {result.exit_code}")
