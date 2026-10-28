"""End-to-end run of the real pipeline and a real Podman sandbox, with a scripted model."""

import json
import shutil
from dataclasses import replace
from pathlib import Path

import pytest

from failfirst.agent import Agent, Job
from failfirst.config import Settings
from failfirst.events import RunLog
from failfirst.sandbox.podman import PodmanSandbox

DEMO = Path(__file__).resolve().parent.parent / "examples" / "tinyledger"

pytestmark = pytest.mark.skipif(not shutil.which("podman"), reason="podman not installed")

REPRO_TEST = '''```python
from tinyledger import Invoice


def test_half_cent_tax_rounds_up():
    inv = Invoice(tax_rate=0.05)
    inv.add("Sticker pack", 2.50)
    assert inv.tax() == 0.13
    assert inv.total() == 2.63
```'''

GOOD_FIX = '''Round half up with Decimal.

src/tinyledger/money.py
<<<<<<< SEARCH
def round_money(amount: float) -> float:
    """Round an amount to whole cents."""
    return round(amount, 2)
=======
def round_money(amount: float) -> float:
    """Round an amount to whole cents, half a cent rounding up."""
    cents = Decimal(repr(amount)).quantize(Decimal("0.01"), rounding=ROUND_HALF_UP)
    return float(cents)
>>>>>>> REPLACE

src/tinyledger/money.py
<<<<<<< SEARCH
"""Helpers for working with currency amounts."""
=======
"""Helpers for working with currency amounts."""

from decimal import ROUND_HALF_UP, Decimal
>>>>>>> REPLACE
'''

USELESS_FIX = '''src/tinyledger/invoice.py
<<<<<<< SEARCH
    def tax(self) -> float:
=======
    def tax(self) -> float:  # computed after discount
>>>>>>> REPLACE
'''


class ScriptedLLM:
    def __init__(self):
        self.calls = []

    async def chat(self, role, messages, *, purpose, **kwargs):
        self.calls.append((role, purpose))
        if purpose.startswith("Investigate"):
            return json.dumps({
                "action": "finish", "root_cause": "round_money uses round(), which is banker's rounding on floats",
                "locations": [{"path": "src/tinyledger/money.py", "symbol": "round_money", "why": "rounding"}],
                "how_to_trigger": "Invoice(tax_rate=0.05) with a 2.50 item",
            })
        if purpose.startswith("Write reproduction"):
            return REPRO_TEST
        if purpose.startswith("B1"):
            return GOOD_FIX
        if purpose.startswith("B2"):
            return USELESS_FIX
        raise AssertionError(f"unexpected chat: {purpose}")

    async def chat_json(self, role, messages, *, purpose, **kwargs):
        self.calls.append((role, purpose))
        if purpose.startswith("Triage"):
            return {"summary": "rounding", "is_bug": True, "candidate_files": ["src/tinyledger/money.py"]}
        if purpose.startswith("Judge"):
            return {"reproduces": True, "reason": "tax is 0.12 instead of 0.13"}
        if purpose.startswith("Plan"):
            return {"strategies": [
                {"title": "Decimal half-up", "approach": "Use Decimal ROUND_HALF_UP in round_money", "files": ["src/tinyledger/money.py"]},
                {"title": "Integer cents", "approach": "Do the arithmetic in integer cents", "files": ["src/tinyledger/invoice.py"]},
            ]}
        if purpose.startswith("Review"):
            return {"winner": "B1", "rationale": "root cause", "risk": "none", "pr_title": "Round money half up", "pr_body": "..."}
        raise AssertionError(f"unexpected chat_json: {purpose}")

    def usage_summary(self):
        return {}


async def test_pipeline_reproduces_then_fixes_tinyledger(tmp_path):
    settings = replace(Settings.from_env(), runs_dir=tmp_path, max_fix_rounds=2, branches=2)
    log = RunLog(tmp_path)
    agent = Agent(settings, log, sandbox=PodmanSandbox(settings.base_image), llm=ScriptedLLM())

    result = await agent.run(Job(repo=str(DEMO), description=(DEMO / "ISSUE.md").read_text(), branches=2))

    errors = [e for e in log.events if e["type"] == "error"]
    assert result["status"] == "fixed", (result, errors)
    assert result["winner"] == "B1"
    assert "ROUND_HALF_UP" in result["diff"]
    assert "tests/test_failfirst_repro.py" in result["diff"]

    repro = [e for e in log.events if e["type"] == "repro"]
    assert repro[0]["verdict"] == "confirmed" and repro[0]["exit_code"] == 1
    statuses = {e["branch"]: e["status"] for e in log.events if e["type"] == "branch"}
    assert statuses["B1"] == "passed"
    assert statuses["B2"] in {"failing", "gave_up", "stopped"}
