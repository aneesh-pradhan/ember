#!/usr/bin/env python3
"""ember coding benchmark: run ember on small coding tasks and score them with hidden tests.

Each run copies examples/tinyledger to a fresh directory, gives ember the task in print mode with
edits allowed, then adds the task's hidden test file and runs the full test suite. A task passes only
if every test passes (the hidden ones and the existing ones). Cost uses Nebius Token Factory prices.

    python3 bench/run.py --ember target/release/ember --configs super super-low --runs 2

Needs `uvx` (for pytest) and a configured NEBIUS_API_KEY.
"""
import argparse
import concurrent.futures as cf
import json
import os
import re
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TASKS = ROOT / "bench" / "tasks"
PROJECT = ROOT / "examples" / "tinyledger"
TEST_CMD = "uvx -q pytest -q -p no:cacheprovider"

SUPER = "nvidia/nemotron-3-super-120b-a12b"
ULTRA = "nvidia/Nemotron-3-Ultra-550b-a55b"
NANO = "nvidia/NVIDIA-Nemotron-3-Nano-30B-A3B"
LIGHTNING = "nvidia/Nemotron-3_5-Lightning"

# USD per 1M tokens: input, cached input, output (Token Factory price list, eu-north1, Oct 2026).
PRICES = {
    SUPER: (0.30, 0.06, 0.90),
    ULTRA: (1.00, 0.12, 3.00),
    NANO: (0.06, 0.012, 0.24),
    LIGHTNING: (0.06, 0.03, 0.24),
}

CONFIGS = {
    "super": {"EMBER_MODEL_CODER": SUPER},
    "super-low": {"EMBER_MODEL_CODER": SUPER, "EMBER_THINK": "low"},
    "super-off": {"EMBER_MODEL_CODER": SUPER, "EMBER_THINK": "off"},
    "super-keep": {"EMBER_MODEL_CODER": SUPER, "EMBER_KEEP_REASONING": "1"},
    "super-nokeep": {"EMBER_MODEL_CODER": SUPER, "EMBER_KEEP_REASONING": "0"},
    "super-nomap": {"EMBER_MODEL_CODER": SUPER, "EMBER_REPO_MAP": "0"},
    "super-nobudget": {"EMBER_MODEL_CODER": SUPER, "EMBER_THINK_BUDGET": "off"},
    "ultra": {"EMBER_MODEL_CODER": ULTRA},
    "nano": {"EMBER_MODEL_CODER": NANO},
    "lightning": {"EMBER_MODEL_CODER": LIGHTNING},
}

MODEL_USAGE_RE = re.compile(r"\[usage:model\] (\S+): (\d+) calls, (\d+) input, (\d+) cached, (\d+) output")
USAGE_RE = re.compile(r"\[usage\] (\d+) model calls, (\d+) tool calls, (\d+) input tokens \((\d+)% cached\), (\d+) output tokens")


def usage_and_cost(stderr: str, model: str) -> dict:
    """Parse ember's `[usage]` lines from print mode and price them with Token Factory rates.

    Newer builds also print one `[usage:model]` line per model, which prices runs that mix tiers (like /fix) and
    uses exact cached counts; older builds only have the total, priced as `model`.
    """
    m = USAGE_RE.search(stderr)
    calls, tools, inp, pct, out = (int(x) for x in m.groups()) if m else (0, 0, 0, 0, 0)
    per_model = [(name, *(int(x) for x in rest)) for name, *rest in MODEL_USAGE_RE.findall(stderr)]
    if not per_model:
        per_model = [(model, calls, inp, inp * pct // 100, out)]
    cost, cached = 0.0, 0
    for name, _, m_in, m_cached, m_out in per_model:
        price_in, price_cached, price_out = PRICES.get(name, PRICES[SUPER])
        cost += ((m_in - m_cached) * price_in + m_cached * price_cached + m_out * price_out) / 1e6
        cached += m_cached
    return {"model_calls": calls, "tool_calls": tools, "input": inp, "cached": cached, "output": out, "cost": round(cost, 5)}


def run_one(ember: str, config: str, task: Path, timeout: int) -> dict:
    work = Path(tempfile.mkdtemp(prefix=f"ember-bench-{config}-{task.name}-"))
    try:
        repo = work / "repo"
        shutil.copytree(PROJECT, repo, ignore=shutil.ignore_patterns("__pycache__", ".ember"))
        (repo / "AGENTS.md").write_text(f"Run the tests with: {TEST_CMD}\n")
        prompt = task.joinpath("task.md").read_text().strip() + "\nRun the tests when you're done."
        env = {**os.environ, **CONFIGS[config], "EMBER_MOUSE": "off"}
        started = time.time()
        try:
            proc = subprocess.run([ember, "--yes", "-p", prompt], cwd=repo, env=env, capture_output=True, text=True, timeout=timeout)
            stderr, timed_out = proc.stderr, False
        except subprocess.TimeoutExpired as e:
            stderr, timed_out = (e.stderr or b"").decode() if isinstance(e.stderr, bytes) else (e.stderr or ""), True
        seconds = time.time() - started
        shutil.copy(task / "hidden_test.py", repo / "tests" / "test_hidden.py")
        tests = subprocess.run(f"{TEST_CMD} tests", shell=True, cwd=repo, capture_output=True, text=True, timeout=300)
        return {
            "config": config, "task": task.name, "passed": tests.returncode == 0 and not timed_out,
            "seconds": round(seconds, 1), **usage_and_cost(stderr, CONFIGS[config]["EMBER_MODEL_CODER"]),
            "timed_out": timed_out,
        }
    finally:
        shutil.rmtree(work, ignore_errors=True)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ember", default=str(ROOT / "target" / "release" / "ember"))
    ap.add_argument("--configs", nargs="+", default=["super"], choices=sorted(CONFIGS))
    ap.add_argument("--tasks", nargs="*", help="task names (default: all)")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--jobs", type=int, default=5)
    ap.add_argument("--timeout", type=int, default=600)
    ap.add_argument("--out", default="bench/results.jsonl")
    args = ap.parse_args()
    args.ember = str(Path(args.ember).resolve()) if os.sep in args.ember else (shutil.which(args.ember) or args.ember)

    tasks = [t for t in sorted(TASKS.iterdir()) if t.is_dir() and (not args.tasks or t.name in args.tasks)]
    jobs = [(c, t) for c in args.configs for _ in range(args.runs) for t in tasks]
    results = []
    with cf.ThreadPoolExecutor(max_workers=args.jobs) as pool, open(args.out, "a") as log:
        futures = [pool.submit(run_one, args.ember, c, t, args.timeout) for c, t in jobs]
        for f in cf.as_completed(futures):
            r = f.result()
            results.append(r)
            log.write(json.dumps(r) + "\n")
            log.flush()
            print(f"{'PASS' if r['passed'] else 'FAIL'}  {r['config']:<15} {r['task']:<15} {r['seconds']:6.1f}s  "
                  f"calls={r['model_calls']:<3} in={r['input']:<7} cached={r['cached']:<7} out={r['output']:<6} ${r['cost']:.4f}", flush=True)

    print(f"\n{'config':<15} {'pass':>7} {'avg s':>7} {'calls':>6} {'in':>8} {'cached':>7} {'out':>7} {'$/task':>8} {'$/pass':>8}")
    for c in args.configs:
        rs = [r for r in results if r["config"] == c]
        n, passed = len(rs), sum(r["passed"] for r in rs)
        avg = lambda k: sum(r[k] for r in rs) / n
        cache_pct = 100 * sum(r["cached"] for r in rs) / max(1, sum(r["input"] for r in rs))
        cost = sum(r["cost"] for r in rs)
        print(f"{c:<15} {passed:>3}/{n:<3} {avg('seconds'):7.1f} {avg('model_calls'):6.1f} {avg('input'):8.0f} {cache_pct:6.0f}% "
              f"{avg('output'):7.0f} {cost / n:8.4f} {cost / passed if passed else float('nan'):8.4f}")


if __name__ == "__main__":
    main()
