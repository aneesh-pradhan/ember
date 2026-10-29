#!/usr/bin/env python3
"""Accuracy eval for the /fix judge: does a failing test genuinely reproduce the bug report?

Each case is a bug report about examples/tinyledger plus a test that fails on the current code, labelled genuine
(it fails because of the reported bug) or broken (import errors, wrong names, unrelated or wrong assertions).
The tests really run, so the judge sees real pytest output, and the request matches `judge` in src/fix.rs
(same prompt, forced `verdict` tool call, Nemotron sampling and reasoning budget).

    python3 bench/judge_eval.py --configs nano lightning lightning-256 lightning-off --runs 3

Needs `uvx` (for pytest) and NEBIUS_API_KEY (read from ~/.config/ember/env if unset).
"""
import argparse
import concurrent.futures as cf
import json
import os
import shutil
import subprocess
import tempfile
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PROJECT = ROOT / "examples" / "tinyledger"
BASE_URL = os.environ.get("EMBER_BASE_URL") or "https://api.tokenfactory.nebius.com/v1/"

NANO = "nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B"
LIGHTNING = "nvidia/Nemotron-3_5-Lightning"
PRICES = {NANO: (0.06, 0.24), LIGHTNING: (0.06, 0.24)}  # USD per 1M input, output tokens

# name: (model, extra request fields). ember's default judge is Nano with reasoning on and a 4,096-token budget.
CONFIGS = {
    "nano": (NANO, {"thinking_token_budget": 4096}),
    "nano-off": (NANO, {"chat_template_kwargs": {"enable_thinking": False}}),
    "lightning": (LIGHTNING, {"thinking_token_budget": 4096}),
    "lightning-256": (LIGHTNING, {"thinking_token_budget": 256}),
    "lightning-off": (LIGHTNING, {"chat_template_kwargs": {"enable_thinking": False}}),
}

SYSTEM = (
    "You check whether a failing test genuinely reproduces a bug report. It is genuine only if it fails "
    "BECAUSE of the reported misbehavior (wrong value, the reported error...). It is NOT genuine if it "
    "fails because the test itself is broken: compile/import errors, wrong names, missing fixtures, syntax "
    "errors, or asserting something unrelated to the report."
)
VERDICT = {
    "type": "function",
    "function": {
        "name": "verdict",
        "description": "Decide whether the failing test genuinely reproduces the bug.",
        "parameters": {
            "type": "object",
            "properties": {
                "reproduces": {"type": "boolean"},
                "reason": {"type": "string", "description": "One or two sentences"},
            },
            "required": ["reproduces", "reason"],
        },
    },
}

HALF_UP = (ROOT / "examples" / "tinyledger" / "ISSUE.md").read_text()
NEG_TAX = "Invoice accepts a negative tax rate: Invoice(tax_rate=-0.1) is created without complaint and then reports " \
          "negative tax. A tax rate below 0 (or above 1) should raise ValueError."
NEG_ZERO = "format_money(-0.001) returns '-$0.00'. Amounts that round to zero should never show a minus sign: it should " \
           "be '$0.00'."
FLOAT_QTY = "Invoice.add accepts fractional quantities: inv.add('Widget', 2.0, 1.5) works and bills 3.0. Quantities are " \
            "whole units, so a non-integer quantity should raise TypeError."

IMPORTS = "import pytest\nfrom tinyledger import Invoice, LineItem, format_money, round_money\n\n"

# (name, issue, test body, genuine?)
CASES = [
    ("half-up-tax", HALF_UP, """
def test_half_cent_tax_rounds_up():
    inv = Invoice(tax_rate=0.05)
    inv.add("Sticker pack", 2.50)
    assert inv.tax() == 0.13
    assert inv.total() == 2.63
""", True),
    ("half-up-discount", HALF_UP, """
def test_half_cent_discount_rounds_up():
    inv = Invoice()
    inv.add("Pin", 0.25)
    inv.apply_discount(50)
    assert inv.discount() == 0.13
""", True),
    ("half-up-round-money", HALF_UP, """
def test_round_money_rounds_half_up():
    assert round_money(0.125) == 0.13
    assert round_money(1.005) == 1.01
""", True),
    ("half-up-summary", HALF_UP, """
def test_summary_matches_receipt():
    inv = Invoice(tax_rate=0.05)
    inv.add("Sticker pack", 2.50)
    assert inv.summary() == {"subtotal": 2.5, "discount": 0.0, "tax": 0.13, "total": 2.63}
""", True),
    ("neg-tax-raises", NEG_TAX, """
def test_negative_tax_rate_is_rejected():
    with pytest.raises(ValueError):
        Invoice(tax_rate=-0.1)
""", True),
    ("neg-zero", NEG_ZERO, """
def test_tiny_negative_amount_has_no_minus_sign():
    assert format_money(-0.001) == "$0.00"
""", True),
    ("float-qty", FLOAT_QTY, """
def test_fractional_quantity_is_rejected():
    inv = Invoice()
    with pytest.raises(TypeError):
        inv.add("Widget", 2.0, 1.5)
""", True),
    ("neg-tax-above-one", NEG_TAX, """
def test_tax_rate_above_one_is_rejected():
    with pytest.raises(ValueError):
        Invoice(tax_rate=1.5)
""", True),
    # Broken tests: they fail on the current code, but not because of the reported bug.
    ("import-error", HALF_UP, """
from tinyledger.rounding import round_half_up

def test_half_cent_rounds_up():
    assert round_half_up(0.125) == 0.13
""", False),
    ("wrong-method", HALF_UP, """
def test_half_cent_tax_rounds_up():
    inv = Invoice(tax_rate=0.05)
    inv.add_item("Sticker pack", 2.50)
    assert inv.tax() == 0.13
""", False),
    ("wrong-kwarg", NEG_TAX, """
def test_negative_tax_rate_is_rejected():
    with pytest.raises(ValueError):
        Invoice(tax=-0.1)
""", False),
    ("missing-fixture", HALF_UP, """
def test_half_cent_tax_rounds_up(invoice):
    invoice.add("Sticker pack", 2.50)
    assert invoice.tax() == 0.13
""", False),
    ("syntax-error", HALF_UP, """
def test_half_cent_tax_rounds_up():
    inv = Invoice(tax_rate=0.05
    inv.add("Sticker pack", 2.50)
    assert inv.tax() == 0.13
""", False),
    ("wrong-arithmetic", HALF_UP, """
def test_half_cent_tax_rounds_up():
    inv = Invoice(tax_rate=0.05)
    inv.add("Sticker pack", 2.50)
    assert inv.total() == 2.75
""", False),
    ("unrelated-assert", HALF_UP, """
def test_money_formatting():
    inv = Invoice(tax_rate=0.05)
    inv.add("Sticker pack", 2.50)
    assert format_money(inv.subtotal()) == "2.50"
""", False),
    ("wrong-expected-string", NEG_ZERO, """
def test_tiny_negative_amount_has_no_minus_sign():
    assert format_money(-0.001) == "0.00"
""", False),
    ("swapped-args", FLOAT_QTY, """
def test_fractional_quantity_bills_nothing():
    inv = Invoice()
    inv.add(2.0, "Widget", 1.5)
    assert inv.total() == 0
""", False),
    ("unrelated-key", HALF_UP, """
def test_summary_has_currency():
    inv = Invoice(tax_rate=0.05)
    inv.add("Sticker pack", 2.50)
    assert inv.summary()["currency"] == "USD"
""", False),
]


def api_key() -> str:
    if os.environ.get("NEBIUS_API_KEY"):
        return os.environ["NEBIUS_API_KEY"]
    for line in (Path.home() / ".config" / "ember" / "env").read_text().splitlines():
        if line.startswith("NEBIUS_API_KEY="):
            return line.split("=", 1)[1].strip().strip("'\"")
    raise SystemExit("NEBIUS_API_KEY is not set")


def run_case(code: str) -> tuple[bool, str]:
    """Run one test file against a fresh copy of tinyledger; return (failed, output)."""
    work = Path(tempfile.mkdtemp(prefix="ember-judge-"))
    try:
        repo = work / "repo"
        shutil.copytree(PROJECT, repo, ignore=shutil.ignore_patterns("__pycache__", ".ember"))
        (repo / "tests" / "test_repro.py").write_text(code)
        out = subprocess.run(
            ["uvx", "-q", "pytest", "-q", "-p", "no:cacheprovider", "tests/test_repro.py"],
            cwd=repo, capture_output=True, text=True, timeout=120,
        )
        return out.returncode != 0, (out.stdout + out.stderr)[-4000:]
    finally:
        shutil.rmtree(work, ignore_errors=True)


def judge(key: str, config: str, issue: str, code: str, output: str) -> dict:
    model, extra = CONFIGS[config]
    body = {
        "model": model,
        "messages": [
            {"role": "system", "content": SYSTEM},
            {"role": "user", "content": f"BUG REPORT\n{issue}\n\nTEST\n```\n{code}\n```\n\nOUTPUT\n{output}"},
        ],
        "tools": [VERDICT],
        "tool_choice": {"type": "function", "function": {"name": "verdict"}},
        "max_tokens": 8192,
        "temperature": 1.0,
        "top_p": 0.95,
        **extra,
    }
    req = urllib.request.Request(
        BASE_URL + "chat/completions",
        data=json.dumps(body).encode(),
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    start = time.time()
    for attempt in range(3):
        try:
            with urllib.request.urlopen(req, timeout=180) as resp:
                data = json.load(resp)
            break
        except Exception as e:  # retry drops and 5xx, like ember does
            if attempt == 2:
                return {"verdict": None, "error": str(e), "seconds": time.time() - start, "cost": 0.0}
            time.sleep(2 * (attempt + 1))
    seconds = time.time() - start
    msg = data["choices"][0]["message"]
    verdict = None
    for call in msg.get("tool_calls") or []:
        try:
            verdict = json.loads(call["function"]["arguments"]).get("reproduces")
        except (json.JSONDecodeError, AttributeError):
            pass
    usage = data.get("usage", {})
    price_in, price_out = PRICES[model]
    cost = (usage.get("prompt_tokens", 0) * price_in + usage.get("completion_tokens", 0) * price_out) / 1e6
    return {"verdict": verdict, "seconds": seconds, "output_tokens": usage.get("completion_tokens", 0), "cost": cost}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--configs", nargs="+", default=["nano", "lightning"], choices=sorted(CONFIGS))
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--jobs", type=int, default=8)
    ap.add_argument("--out", help="write every judgement as JSON lines")
    args = ap.parse_args()
    key = api_key()

    cases = []
    with cf.ThreadPoolExecutor(args.jobs) as pool:
        for (name, issue, code, genuine), (failed, output) in zip(
            CASES, pool.map(lambda c: run_case(IMPORTS + c[2].lstrip()), CASES)
        ):
            if not failed:
                print(f"skipping {name}: the test passes on the current code, so the judge never sees it")
                continue
            cases.append((name, issue, IMPORTS + code.lstrip(), output, genuine))
    print(f"{len(cases)} cases ({sum(c[4] for c in cases)} genuine), {args.runs} runs per config\n")

    jobs = [(cfg, case, run) for cfg in args.configs for case in cases for run in range(args.runs)]
    with cf.ThreadPoolExecutor(args.jobs) as pool:
        results = list(pool.map(lambda j: (j, judge(key, j[0], j[1][1], j[1][2], j[1][3])), jobs))

    out = open(args.out, "w") if args.out else None
    by_config: dict[str, list] = {}
    for (cfg, case, run), r in results:
        r.update(config=cfg, case=case[0], run=run, genuine=case[4])
        by_config.setdefault(cfg, []).append(r)
        if out:
            out.write(json.dumps(r) + "\n")

    print(f"{'config':15} {'correct':>9} {'false+':>7} {'false-':>7} {'no ans':>7} {'median s':>9} {'out tok':>8} {'$/1k':>7}")
    for cfg in args.configs:
        rs = by_config[cfg]
        correct = sum(r["verdict"] == r["genuine"] for r in rs)
        false_pos = sum(r["verdict"] is True and not r["genuine"] for r in rs)
        false_neg = sum(r["verdict"] is False and r["genuine"] for r in rs)
        missing = sum(r["verdict"] is None for r in rs)
        secs = sorted(r["seconds"] for r in rs)[len(rs) // 2]
        tok = sum(r.get("output_tokens", 0) for r in rs) / len(rs)
        cost = sum(r["cost"] for r in rs) / len(rs) * 1000
        print(f"{cfg:15} {correct:>4}/{len(rs):<4} {false_pos:>7} {false_neg:>7} {missing:>7} {secs:>9.1f} {tok:>8.0f} {cost:>7.3f}")
    wrong = sorted({(r["config"], r["case"]) for rs in by_config.values() for r in rs if r["verdict"] != r["genuine"]})
    if wrong:
        print("\nwrong or missing answers:", ", ".join(f"{c}:{n}" for c, n in wrong))


if __name__ == "__main__":
    main()
