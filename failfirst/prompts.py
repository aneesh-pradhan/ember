"""Prompt templates. Code travels in fenced blocks, never inside JSON strings."""

TRIAGE = """You triage bug reports for a Python repository.

Read the issue and the repository file list. Reply with one JSON object:
{
  "summary": "one sentence: what is broken",
  "expected": "the behavior the reporter expects",
  "actual": "the behavior they observe",
  "is_bug": true,
  "candidate_files": ["up to 8 repo paths most likely involved, most likely first"],
  "keywords": ["identifiers or strings worth grepping for"]
}
Set "is_bug" to false only if the issue is clearly a feature request or question, not a defect."""

INVESTIGATE = """You are investigating a bug in a Python repository before anyone writes a fix.
Find the code responsible and understand how to trigger the bug from a test.

Each reply is ONE JSON object choosing one action:
  {"thought": "...", "action": "read", "path": "pkg/mod.py", "start": 1, "end": 200}
  {"thought": "...", "action": "grep", "pattern": "regex", "glob": "*.py"}
  {"thought": "...", "action": "list", "prefix": "src/"}
  {"thought": "...", "action": "finish",
   "root_cause": "precise explanation of what goes wrong and where",
   "locations": [{"path": "pkg/mod.py", "symbol": "func_name", "why": "..."}],
   "how_to_trigger": "the public API call and inputs that expose the bug"}

You have at most {steps} actions. Finish as soon as you can explain the bug and how to trigger it."""

REPRODUCE = """You write a single pytest file that REPRODUCES a reported bug.

Requirements:
- The test must FAIL on the current code, specifically because of the reported bug,
  and must PASS once the bug is fixed.
- Assert the expected (correct) behavior from the issue. Do not assert the buggy behavior.
- Use the package's public API and import it the way the existing tests do.
- Keep it small: 1-3 focused test functions, deterministic, no network, no sleeping.
- Do not mock the code under test.

Reply with a one-paragraph explanation, then the complete file in one ```python block."""

JUDGE = """You check whether a failing test genuinely reproduces a bug report.

A genuine reproduction fails BECAUSE of the reported misbehavior (a wrong value, the reported exception, etc).
It is NOT genuine if it fails because of a mistake in the test itself: ImportError/NameError in the test,
a wrong function name, a missing fixture, a syntax error, or asserting something unrelated to the issue.

Reply with one JSON object: {"reproduces": true|false, "reason": "one or two sentences"}"""

PLAN = """You are a senior engineer planning fixes for a bug that already has a failing reproduction test.
Propose {n} genuinely DIFFERENT fix strategies (different locations, mechanisms, or scopes; not
rewordings of one idea). Prefer root-cause fixes over special-casing the test inputs.

Reply with one JSON object:
{"strategies": [{"title": "short name", "approach": "2-4 sentences: what to change and why",
                 "files": ["repo paths to edit"]}]}"""

FIX = """You fix a bug in a Python repository by editing source files.

A reproduction test already fails because of the bug. Your change must make it pass without
breaking the existing test suite. You may NOT edit any test files or conftest.py.
Follow the strategy you are given. Keep the change minimal and in the style of the codebase.

Express every change as one or more SEARCH/REPLACE blocks:

path/to/file.py
<<<<<<< SEARCH
exact lines currently in the file (enough context to be unique)
=======
the new lines
>>>>>>> REPLACE

To create a new file, use an empty SEARCH section. Write a short explanation first, then the blocks."""

REVIEW = """You are reviewing candidate patches for a bug. Every candidate makes the reproduction test pass and
causes no new test failures. Choose the best one for a maintainer to merge: correct at the root cause,
minimal, idiomatic, and unlikely to break other callers.

Reply with one JSON object:
{
  "winner": "branch id",
  "rationale": "why this candidate over the others (2-4 sentences)",
  "risk": "what a reviewer should double-check",
  "pr_title": "concise pull request title",
  "pr_body": "markdown PR description: root cause, the fix, and how it was verified"
}"""
