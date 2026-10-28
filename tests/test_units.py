import pytest

from failfirst.agent import is_test_path, last_code_block, parse_edits
from failfirst.llm import extract_json
from failfirst.pytest_output import failing_ids
from failfirst.repo import EditError, Repo, RepoRef, parse_issue_url, parse_repo_url


def make_repo(**files):
    return Repo(files={k.replace("__", "/"): v for k, v in files.items()}, ref=RepoRef(label="t"))


def test_extract_json_handles_fences_and_prose():
    assert extract_json('Sure!\n```json\n{"a": 1}\n```') == {"a": 1}
    assert extract_json('thinking... {"b": [1, 2]} done') == {"b": [1, 2]}
    with pytest.raises(ValueError):
        extract_json("no json here")


def test_parse_edits_reads_search_replace_blocks():
    reply = """Use Decimal.

src/pkg/money.py
<<<<<<< SEARCH
def f():
    return 1
=======
def f():
    return 2
>>>>>>> REPLACE

new_file.py
<<<<<<< SEARCH
=======
X = 1
>>>>>>> REPLACE
"""
    assert parse_edits(reply) == [
        ("src/pkg/money.py", "def f():\n    return 1\n", "def f():\n    return 2\n"),
        ("new_file.py", "", "X = 1\n"),
    ]


def test_with_edits_applies_exact_and_whitespace_tolerant_matches():
    repo = make_repo(**{"a.py": "x = 1   \ny = 2\n", "b.py": "z = 3\n"})
    new, changed = repo.with_edits([("a.py", "x = 1\ny = 2\n", "x = 10\ny = 20\n"), ("c.py", "", "w = 0\n")])
    assert new.files["a.py"] == "x = 10\ny = 20\n"
    assert new.files["c.py"] == "w = 0\n"
    assert changed == ["a.py", "c.py"]
    assert repo.files["a.py"] == "x = 1   \ny = 2\n"  # original untouched


def test_with_edits_rejects_ambiguous_missing_and_escaping_paths():
    repo = make_repo(**{"a.py": "x = 1\nx = 1\n"})
    with pytest.raises(EditError, match="matches 2"):
        repo.with_edits([("a.py", "x = 1\n", "x = 2\n")])
    with pytest.raises(EditError, match="not found"):
        repo.with_edits([("a.py", "nope\n", "x\n")])
    with pytest.raises(EditError, match="relative"):
        repo.with_edits([("../etc/passwd", "", "x")])


def test_diff_and_grep():
    repo = make_repo(**{"pkg__m.py": "def add(a, b):\n    return a - b\n"})
    new, changed = repo.with_edits([("pkg/m.py", "return a - b", "return a + b")])
    diff = new.diff_against(repo, changed)
    assert "-    return a - b" in diff and "+    return a + b" in diff
    assert repo.grep(r"def add") == ["pkg/m.py:1: def add(a, b):"]


def test_test_path_detection_and_code_blocks():
    assert is_test_path("tests/test_x.py") and is_test_path("conftest.py") and is_test_path("pkg/x_test.py")
    assert not is_test_path("src/pkg/utils.py")
    assert last_code_block("a\n```python\nx = 1\n```\nb\n```python\ny = 2\n```") == "y = 2\n"


def test_failing_ids_and_url_parsing():
    out = "FAILED tests/test_a.py::test_x - AssertionError\nERROR tests/test_b.py - ImportError\n1 failed"
    assert failing_ids(out) == {"tests/test_a.py::test_x", "tests/test_b.py"}
    assert parse_issue_url("https://github.com/psf/requests/issues/6711") == ("psf", "requests", 6711)
    assert parse_repo_url("https://github.com/psf/requests.git") == ("psf", "requests")
    assert parse_repo_url("not a url") is None
