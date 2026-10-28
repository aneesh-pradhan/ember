"""The FailFirst pipeline: reproduce the bug with a failing test, then fix it in parallel branches."""

from __future__ import annotations

import asyncio
import json
import re
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath
from typing import Any

from . import prompts
from .config import Settings
from .events import RunLog
from .llm import LLM, extract_json
from .pytest_output import PYTEST, describe, failing_ids
from .repo import (
    EditError, Issue, Repo, fetch_github_repo, fetch_issue, load_local, normalize_path, parse_issue_url, parse_repo_url,
)
from .sandbox import ExecResult, Sandbox, Snapshot, make_sandbox

REPRO_NAME = "test_failfirst_repro.py"

# Runs inside the sandbox: download the exact commit and unpack it into the repo dir.
FETCH_SCRIPT = """python - <<'EOF'
import io, tarfile, urllib.request
data = urllib.request.urlopen("{url}").read()
with tarfile.open(fileobj=io.BytesIO(data)) as tar:
    members = []
    for member in tar.getmembers():
        parts = member.name.split("/", 1)
        if len(parts) == 2 and parts[1]:
            member.name = parts[1]
            members.append(member)
    tar.extractall(".", members=members, filter="data")
EOF"""

INSTALL_SCRIPT = """pip install -q pytest
for f in requirements.txt requirements-dev.txt requirements_dev.txt requirements-test.txt \\
         requirements_test.txt test-requirements.txt dev-requirements.txt \\
         requirements/test.txt requirements/dev.txt requirements/tests.txt; do
  [ -f "$f" ] && pip install -q -r "$f"
done
if [ -f pyproject.toml ] || [ -f setup.py ]; then
  pip install -q -e '.[test,tests,testing,dev]' || pip install -q -e .
fi
python -m pytest --version"""
CONTEXT_BUDGET = 40_000  # characters of source code sent with a single prompt

_CODE_BLOCK_RE = re.compile(r"```(?:python|py)?\s*\n(.*?)```", re.DOTALL)
_EDIT_RE = re.compile(
    r"^([^\n`<>=]+?\.[\w]+)\s*\n(?:```\w*\n)?<<<<<<< SEARCH\n(.*?)^=======\n(.*?)^>>>>>>> REPLACE",
    re.DOTALL | re.MULTILINE,
)


class Stop(Exception):
    """Ends the run early with an honest, non-error outcome."""

    def __init__(self, status: str, message: str):
        super().__init__(message)
        self.status = status


@dataclass
class Job:
    issue_url: str | None = None
    repo: str | None = None  # GitHub URL or local path, when there is no issue URL
    description: str | None = None
    branches: int | None = None


@dataclass
class Branch:
    id: str
    strategy: dict[str, Any]
    repo: Repo
    changed: list[str] = field(default_factory=list)
    status: str = "pending"
    rounds: int = 0


def is_test_path(path: str) -> bool:
    p = PurePosixPath(path)
    return (
        p.name.startswith("test_") or p.name.endswith("_test.py") or p.name == "conftest.py"
        or any(part in {"test", "tests", "testing"} for part in p.parts[:-1])
    )


def parse_edits(text: str) -> list[tuple[str, str, str]]:
    return [(m.group(1).strip(), m.group(2), m.group(3)) for m in _EDIT_RE.finditer(text)]


def last_code_block(text: str) -> str | None:
    blocks = _CODE_BLOCK_RE.findall(text)
    return blocks[-1] if blocks else None


class Agent:
    def __init__(self, settings: Settings, log: RunLog, sandbox: Sandbox | None = None, llm: LLM | None = None):
        self.settings = settings
        self.log = log
        self.emit = log.emit
        self.llm = llm or LLM(settings, log.emit)
        self.sandbox = sandbox or make_sandbox(settings)
        self.exec_count = 0

    # ------------------------------------------------------------------ helpers

    async def stage(self, name: str, status: str, **data: Any) -> None:
        await self.emit("stage", stage=name, status=status, **data)

    async def note(self, stage: str, text: str, **data: Any) -> None:
        await self.emit("note", stage=stage, text=text, **data)

    async def exec(
        self, snapshot: Snapshot, command: str, *, stage: str, label: str,
        files: dict[str, str] | None = None, timeout: int | None = None,
        save: bool = False, network: bool = False, branch: str | None = None,
    ) -> tuple[ExecResult, Snapshot | None]:
        self.exec_count += 1
        await self.emit("exec_start", stage=stage, branch=branch, label=label, command=command)
        result, saved = await snapshot.run(
            command, files=files, timeout=timeout or self.settings.test_timeout, save=save, network=network,
        )
        await self.emit(
            "exec", stage=stage, branch=branch, label=label, command=command,
            exit_code=result.exit_code, seconds=round(result.seconds, 1),
            timed_out=result.timed_out, output=result.output(4000),
        )
        return result, saved

    # ---------------------------------------------------------------- pipeline

    async def run(self, job: Job) -> dict[str, Any]:
        await self.emit(
            "run", id=self.log.id, job=job.__dict__, models=self.settings.models, sandbox=self.sandbox.name,
        )
        try:
            result = await self._pipeline(job)
        except Stop as stop:
            result = {"status": stop.status, "message": str(stop)}
        except Exception as exc:  # surfaced to the UI rather than crashing the server
            await self.emit("error", message=f"{type(exc).__name__}: {exc}")
            result = {"status": "error", "message": str(exc)}
        finally:
            await self.sandbox.close()
        result.update(usage=self.llm.usage_summary(), sandbox_runs=self.exec_count)
        await self.emit("result", **result)
        await self.log.finish()
        return result

    async def _pipeline(self, job: Job) -> dict[str, Any]:
        issue, repo = await self.fetch(job)
        ready = await self.setup(repo)
        baseline = asyncio.create_task(self.baseline(ready))
        try:
            triage = await self.triage(issue, repo)
            report = await self.investigate(issue, repo, triage)
            repro_path, repro_code, repro_output, reproduced = await self.reproduce(issue, repo, report, ready)
            baseline_failures = await baseline
        finally:
            if not baseline.done():
                baseline.cancel()
        strategies = await self.plan(issue, repo, report, repro_code, repro_output, job.branches or self.settings.branches)
        winners = await self.fix(issue, repo, report, strategies, repro_path, repro_code, repro_output, reproduced, baseline_failures)
        return await self.review(issue, repo, winners, repro_path, repro_code)

    async def fetch(self, job: Job) -> tuple[Issue, Repo]:
        await self.stage("fetch", "running")
        token = self.settings.github_token
        if job.issue_url:
            parsed = parse_issue_url(job.issue_url)
            if not parsed:
                raise ValueError("Expected a GitHub issue URL like https://github.com/owner/repo/issues/123")
            owner, name, number = parsed
            issue, repo = await asyncio.gather(fetch_issue(owner, name, number, token), fetch_github_repo(owner, name, token))
        else:
            if not (job.repo and job.description):
                raise ValueError("Provide a GitHub issue URL, or a repository plus a bug description")
            parsed_repo = parse_repo_url(job.repo)
            if parsed_repo:
                repo = await fetch_github_repo(*parsed_repo, token)
            else:
                repo = load_local(Path(job.repo).expanduser())
            first_line = job.description.strip().splitlines()[0][:120]
            issue = Issue(title=first_line, body=job.description)
        await self.emit("issue", title=issue.title, url=issue.url, body=issue.body[:4000], repo=repo.ref.label, files=len(repo.files))
        await self.stage("fetch", "done", detail=f"{repo.ref.label} · {len(repo.files)} files")
        return issue, repo

    async def setup(self, repo: Repo) -> Snapshot:
        await self.stage("setup", "running", detail=self.sandbox.name)
        base = await self.sandbox.start()
        if repo.ref.local_path:
            files, fetch = repo.files, "true"
        else:
            files, fetch = None, FETCH_SCRIPT.format(url=repo.ref.tarball_url)
        install = f"{fetch}\n{INSTALL_SCRIPT}"
        result, ready = await self.exec(
            base, install, stage="setup", label="Install dependencies and checkpoint",
            files=files, timeout=self.settings.setup_timeout, save=True, network=True,
        )
        if result.exit_code != 0 or ready is None:
            raise RuntimeError(f"Sandbox setup failed:\n{result.output(2000)}")
        await self.emit("checkpoint", name="env-ready", id=ready.label, detail="dependencies installed")
        await self.stage("setup", "done", detail=f"checkpoint {ready.label}")
        return ready

    async def baseline(self, ready: Snapshot) -> set[str] | None:
        result, _ = await self.exec(ready, f"{PYTEST} --tb=no", stage="setup", label="Baseline test suite")
        if result.timed_out or result.exit_code not in (0, 1, 2, 5):
            await self.note("setup", f"Baseline suite unusable ({describe(result)}); regressions will not be checked.")
            return None
        failures = failing_ids(result.stdout)
        await self.note(
            "setup",
            "Baseline: all existing tests pass." if not failures
            else f"Baseline: {len(failures)} test(s) already fail before any change; they are ignored.",
            failures=sorted(failures),
        )
        return failures

    async def triage(self, issue: Issue, repo: Repo) -> dict[str, Any]:
        await self.stage("triage", "running")
        triage = await self.llm.chat_json("fast", [
            {"role": "system", "content": prompts.TRIAGE},
            {"role": "user", "content": f"ISSUE\n{issue.as_prompt()}\n\nFILES\n{repo.tree()}"},
        ], purpose="Triage the issue")
        triage["candidate_files"] = [p for p in triage.get("candidate_files", []) if p in repo.files][:8]
        await self.emit("triage", **triage)
        if triage.get("is_bug") is False:
            await self.stage("triage", "failed", detail="not a bug report")
            raise Stop("not_a_bug", "This issue reads as a feature request or question, not a reproducible bug.")
        await self.stage("triage", "done", detail=triage.get("summary", ""))
        return triage

    async def investigate(self, issue: Issue, repo: Repo, triage: dict[str, Any]) -> dict[str, Any]:
        await self.stage("investigate", "running")
        steps = self.settings.investigate_steps
        messages = [
            {"role": "system", "content": prompts.INVESTIGATE.replace("{steps}", str(steps))},
            {"role": "user", "content": (
                f"ISSUE\n{issue.as_prompt()}\n\nTRIAGE\n{json.dumps(triage, indent=2)}\n\n"
                f"FILES\n{repo.tree(600)}"
            )},
        ]
        for step in range(1, steps + 1):
            final = step == steps
            if final:
                messages.append({"role": "user", "content": "This is your last action: you must reply with action \"finish\"."})
            reply = await self.llm.chat("coder", messages, purpose=f"Investigate (step {step})", json_mode=True)
            messages.append({"role": "assistant", "content": reply})
            try:
                action = extract_json(reply)
            except ValueError:
                messages.append({"role": "user", "content": "Reply with exactly one JSON action object."})
                continue
            kind = action.get("action")
            await self.emit("investigate_step", step=step, action=kind, thought=action.get("thought", ""),
                            target=action.get("path") or action.get("pattern") or action.get("prefix"))
            if kind == "finish":
                action["locations"] = [loc for loc in action.get("locations", []) if loc.get("path") in repo.files]
                await self.emit("investigation", **action)
                await self.stage("investigate", "done", detail=action.get("root_cause", "")[:200])
                return action
            messages.append({"role": "user", "content": self._observe(repo, action)})
        raise RuntimeError("Investigation did not converge on a root cause")

    def _observe(self, repo: Repo, action: dict[str, Any]) -> str:
        kind = action.get("action")
        try:
            if kind == "read":
                text = repo.read(action["path"], action.get("start"), action.get("end"))
                return text[:12_000] + ("\n...(truncated; read a narrower range)" if len(text) > 12_000 else "")
            if kind == "grep":
                hits = repo.grep(action["pattern"], action.get("glob"))
                return "\n".join(hits) or "no matches"
            if kind == "list":
                prefix = action.get("prefix") or ""
                return "\n".join(p for p in repo.paths() if p.startswith(prefix))[:6000] or "no files"
        except KeyError as exc:
            return f"error: no such file {exc}"
        except (EditError, TypeError) as exc:
            return f"error: {exc}"
        return "error: unknown action; use read, grep, list or finish"

    def _source_context(self, repo: Repo, paths: list[str]) -> str:
        chunks, used = [], 0
        for path in dict.fromkeys(paths):
            if path not in repo.files:
                continue
            text = repo.files[path]
            if used + len(text) > CONTEXT_BUDGET:
                text = text[: max(0, CONTEXT_BUDGET - used)] + "\n...(truncated)"
            chunks.append(f"### {path}\n```python\n{text}\n```")
            used += len(text)
            if used >= CONTEXT_BUDGET:
                break
        return "\n\n".join(chunks)

    def _repro_path(self, repo: Repo) -> str:
        dirs = repo.test_dirs()
        preferred = next((d for d in dirs if PurePosixPath(d).name in {"tests", "test"}), dirs[0])
        return str(PurePosixPath(preferred) / REPRO_NAME) if preferred != "." else REPRO_NAME

    async def reproduce(self, issue: Issue, repo: Repo, report: dict[str, Any], ready: Snapshot):
        await self.stage("reproduce", "running")
        path = self._repro_path(repo)
        example_test = next((p for p in repo.paths() if is_test_path(p) and p.endswith(".py")
                             and PurePosixPath(p).name.startswith("test")), None)
        context = self._source_context(repo, [loc["path"] for loc in report.get("locations", [])])
        conventions = f"\n\nEXISTING TEST FOR CONVENTIONS ({example_test})\n```python\n{repo.files[example_test][:3000]}\n```" if example_test else ""
        messages = [
            {"role": "system", "content": prompts.REPRODUCE},
            {"role": "user", "content": (
                f"ISSUE\n{issue.as_prompt()}\n\nINVESTIGATION\n{json.dumps(report, indent=2)}\n\n"
                f"RELEVANT SOURCE\n{context}{conventions}\n\nThe file will be saved as {path} and run with pytest from the repo root."
            )},
        ]
        for attempt in range(1, self.settings.max_repro_attempts + 1):
            reply = await self.llm.chat("coder", messages, purpose=f"Write reproduction test (attempt {attempt})", temperature=0.3)
            messages.append({"role": "assistant", "content": reply})
            code = last_code_block(reply)
            if not code:
                messages.append({"role": "user", "content": "Reply with the complete test file in a ```python block."})
                continue
            result, _ = await self.exec(
                ready, f"{PYTEST} --tb=short {path}", stage="reproduce",
                label=f"Run reproduction test (attempt {attempt})", files={path: code},
            )
            verdict, reason = await self._judge_repro(issue, code, result)
            await self.emit("repro", attempt=attempt, path=path, code=code, exit_code=result.exit_code,
                            output=result.output(4000), verdict=verdict, reason=reason)
            if verdict == "confirmed":
                _, checkpoint = await self.exec(
                    ready, "true", stage="reproduce", label="Checkpoint with reproduction test",
                    files={path: code}, save=True,
                )
                await self.emit("checkpoint", name="bug-reproduced", id=checkpoint.label, detail=f"{path} fails on current code")
                await self.stage("reproduce", "done", detail=f"Bug reproduced in {path}")
                return path, code, result.output(4000), checkpoint
            messages.append({"role": "user", "content": (
                f"pytest result: {describe(result)}\n{result.output(3000)}\n\n"
                f"This does not count as a reproduction: {reason}\nWrite a corrected complete test file."
            )})
        await self.stage("reproduce", "failed", detail="could not reproduce")
        raise Stop("not_reproduced", "Could not write a test that reproduces this bug, so no fix was attempted. "
                                     "FailFirst never ships a patch it cannot verify.")

    async def _judge_repro(self, issue: Issue, code: str, result: ExecResult) -> tuple[str, str]:
        if result.timed_out:
            return "rejected", "The test timed out."
        if result.exit_code == 0:
            return "rejected", "The test passed on the current code, so it does not demonstrate the bug."
        if result.exit_code != 1:
            return "rejected", f"pytest could not run the test ({describe(result)})."
        judgement = await self.llm.chat_json("fast", [
            {"role": "system", "content": prompts.JUDGE},
            {"role": "user", "content": (
                f"ISSUE\n{issue.as_prompt()}\n\nTEST\n```python\n{code}\n```\n\nPYTEST OUTPUT\n{result.output(4000)}"
            )},
        ], purpose="Judge: does the failure match the issue?", temperature=0.0)
        ok = bool(judgement.get("reproduces"))
        return ("confirmed" if ok else "rejected"), judgement.get("reason", "")

    async def plan(self, issue: Issue, repo: Repo, report: dict[str, Any], repro_code: str, repro_output: str, n: int):
        await self.stage("plan", "running")
        context = self._source_context(repo, [loc["path"] for loc in report.get("locations", [])])
        data = await self.llm.chat_json("reasoner", [
            {"role": "system", "content": prompts.PLAN.replace("{n}", str(n))},
            {"role": "user", "content": (
                f"ISSUE\n{issue.as_prompt()}\n\nROOT CAUSE\n{json.dumps(report, indent=2)}\n\n"
                f"FAILING TEST\n```python\n{repro_code}\n```\n\nFAILURE\n{repro_output[:3000]}\n\nSOURCE\n{context}"
            )},
        ], purpose=f"Plan {n} distinct fix strategies", temperature=0.4, max_tokens=8192)
        strategies = [s for s in data.get("strategies", []) if isinstance(s, dict) and s.get("approach")][:n]
        if not strategies:
            raise RuntimeError("The planner returned no usable strategies")
        await self.emit("strategies", items=[{"branch": f"B{i}", **s} for i, s in enumerate(strategies, 1)])
        await self.stage("plan", "done", detail=f"{len(strategies)} strategies")
        return strategies

    async def fix(self, issue, repo, report, strategies, repro_path, repro_code, repro_output, reproduced, baseline_failures):
        await self.stage("fix", "running", detail=f"{len(strategies)} branches from checkpoint {reproduced.label}")
        branches = [Branch(id=f"B{i}", strategy=s, repo=repo) for i, s in enumerate(strategies, 1)]
        solved = asyncio.Event()
        await asyncio.gather(*(
            self._run_branch(b, issue, repo, report, repro_path, repro_code, repro_output, reproduced, baseline_failures, solved)
            for b in branches
        ))
        winners = [b for b in branches if b.status == "passed"]
        if not winners:
            await self.stage("fix", "failed", detail="no branch passed")
            raise Stop("not_fixed", "The bug is reproduced, but no candidate fix passed verification. "
                                    "The reproduction test is still a useful starting point.")
        await self.stage("fix", "done", detail=f"{len(winners)} of {len(branches)} branches passed")
        return winners

    async def _run_branch(self, branch, issue, base_repo, report, repro_path, repro_code, repro_output,
                          reproduced, baseline_failures, solved):
        async def update(status, note="", **data):
            branch.status = status
            await self.emit("branch", branch=branch.id, status=status, round=branch.rounds, note=note, **data)

        paths = list(branch.strategy.get("files") or []) + [loc["path"] for loc in report.get("locations", [])]
        messages = [
            {"role": "system", "content": prompts.FIX},
            {"role": "user", "content": (
                f"ISSUE\n{issue.as_prompt()}\n\nROOT CAUSE\n{report.get('root_cause', '')}\n\n"
                f"STRATEGY: {branch.strategy.get('title')}\n{branch.strategy.get('approach')}\n\n"
                f"FAILING TEST ({repro_path})\n```python\n{repro_code}\n```\n\nCURRENT FAILURE\n{repro_output[:3000]}\n\n"
                f"SOURCE\n{self._source_context(base_repo, paths)}"
            )},
        ]
        await update("running", branch.strategy.get("title", ""))
        try:
            for round_no in range(1, self.settings.max_fix_rounds + 1):
                if solved.is_set():
                    await update("stopped", "another branch already passed")
                    return
                branch.rounds = round_no
                reply = await self.llm.chat("coder", messages, purpose=f"{branch.id}: write patch (round {round_no})",
                                            temperature=0.4, max_tokens=6000)
                messages.append({"role": "assistant", "content": reply})
                feedback = await self._try_patch(branch, base_repo, reply, repro_path, reproduced, baseline_failures, update)
                if feedback is None:
                    solved.set()
                    return
                messages.append({"role": "user", "content": feedback})
            await update("gave_up", f"no passing patch after {self.settings.max_fix_rounds} rounds")
        except Exception as exc:
            await update("error", f"{type(exc).__name__}: {exc}")

    async def _try_patch(self, branch, base_repo, reply, repro_path, reproduced, baseline_failures, update):
        """Apply and verify one patch. Returns feedback for the model, or None when the branch passes."""
        edits = parse_edits(reply)
        if not edits:
            await update("invalid", "no SEARCH/REPLACE blocks in reply")
            return "No SEARCH/REPLACE blocks found. Reply with edits in exactly the required format."
        blocked = [path for path, _, _ in edits if is_test_path(normalize_path(path))]
        if blocked:
            await update("invalid", f"tried to edit test files: {', '.join(blocked)}")
            return f"You may not edit test files ({', '.join(blocked)}). Fix the source code instead."
        try:
            new_repo, changed = branch.repo.with_edits(edits)
        except EditError as exc:
            await update("invalid", str(exc))
            return f"Your edit could not be applied: {exc}\nThe file has not changed; resend corrected blocks."
        branch.repo = new_repo
        branch.changed = list(dict.fromkeys(branch.changed + changed))
        diff = new_repo.diff_against(base_repo, branch.changed)
        files = {p: new_repo.files[p] for p in branch.changed}
        await update("testing", "running reproduction test", diff=diff)

        result, _ = await self.exec(reproduced, f"{PYTEST} --tb=short {repro_path}", stage="fix", branch=branch.id,
                                    label="Reproduction test", files=files)
        if result.exit_code != 0:
            await update("failing", f"reproduction test still fails ({describe(result)})", diff=diff)
            return (f"Your patch is applied (cumulative diff below) but the reproduction test still fails:\n"
                    f"{result.output(3000)}\n\nCURRENT DIFF\n{diff}\nSend further SEARCH/REPLACE blocks against the patched files.")

        await update("testing", "reproduction passes; running full suite", diff=diff)
        result, _ = await self.exec(reproduced, f"{PYTEST} --tb=short", stage="fix", branch=branch.id,
                                    label="Full test suite", files=files)
        if baseline_failures is None:
            await update("passed", "reproduction passes (full suite unavailable for comparison)", diff=diff)
            return None
        if result.timed_out or result.exit_code not in (0, 1):
            await update("regressed", f"full suite did not complete ({describe(result)})", diff=diff)
            return f"The full test suite did not complete with your patch ({describe(result)}):\n{result.output(3000)}"
        new_failures = sorted(failing_ids(result.stdout) - baseline_failures)
        if new_failures:
            await update("regressed", f"{len(new_failures)} new failure(s)", diff=diff, failures=new_failures)
            return (f"The reproduction passes, but these tests now fail and passed before: {', '.join(new_failures)}\n"
                    f"{result.output(3000)}\n\nCURRENT DIFF\n{diff}\nAdjust the patch with further SEARCH/REPLACE blocks.")
        await update("passed", "reproduction passes and no existing test regressed", diff=diff)
        return None

    async def review(self, issue: Issue, repo: Repo, winners: list[Branch], repro_path: str, repro_code: str):
        await self.stage("review", "running")
        candidates = "\n\n".join(
            f"## Branch {b.id}: {b.strategy.get('title')}\n{b.strategy.get('approach')}\n```diff\n{b.repo.diff_against(repo, b.changed)}```"
            for b in winners
        )
        review = await self.llm.chat_json("reasoner", [
            {"role": "system", "content": prompts.REVIEW},
            {"role": "user", "content": (
                f"ISSUE\n{issue.as_prompt()}\n\nREPRODUCTION TEST ({repro_path})\n```python\n{repro_code}\n```\n\n"
                f"CANDIDATES\n{candidates}"
            )},
        ], purpose="Review candidates and write the PR", temperature=0.2, max_tokens=6000)
        winner = next((b for b in winners if b.id == str(review.get("winner", "")).strip()), winners[0])
        final_repo = Repo(files={**winner.repo.files, repro_path: repro_code}, ref=repo.ref)
        diff = final_repo.diff_against(repo, winner.changed + [repro_path])
        await self.stage("review", "done", detail=f"selected {winner.id}")
        return {
            "status": "fixed",
            "winner": winner.id,
            "strategy": winner.strategy.get("title"),
            "rationale": review.get("rationale", ""),
            "risk": review.get("risk", ""),
            "pr_title": review.get("pr_title") or f"Fix: {issue.title}",
            "pr_body": review.get("pr_body", ""),
            "diff": diff,
            "candidates": [b.id for b in winners],
        }
