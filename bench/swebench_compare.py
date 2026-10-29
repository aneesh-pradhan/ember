#!/usr/bin/env python3
"""Compare SWE-bench runs instance by instance: what each run resolved, what it cost, and which instances flipped.

    python3 bench/swebench_compare.py baseline-super test-cmd-r1 test-cmd-r2

Each argument is a run id under bench/swebench/runs/ (or a path to a run directory). Repeat a configuration under
several run ids to see how much of a difference is noise: an instance that passes in one repeat and fails in
another is unstable, not fixed. Flips list the agent log to read for each one, which is where harness changes
should come from.
"""
import csv
import sys
from pathlib import Path

RUNS = Path(__file__).resolve().parent / "swebench" / "runs"


def load(run: str) -> tuple[str, Path, dict[str, dict]]:
    path = Path(run) if Path(run).is_dir() else RUNS / run
    with open(path / "results.csv") as f:
        rows = {r["instance_id"]: r for r in csv.DictReader(f) if not r["error"]}  # later rows win on reruns
    return path.name, path, rows


def main() -> None:
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    runs = [load(r) for r in sys.argv[1:]]
    ids = sorted(set().union(*(rows for _, _, rows in runs)))
    width = max(12, *(len(name) for name, _, _ in runs))

    print(f"{'instance':<40} " + " ".join(f"{name:>{width}}" for name, _, _ in runs))
    for i in ids:
        cells = []
        for _, _, rows in runs:
            r = rows.get(i)
            cells.append("-" if r is None else f"{'PASS' if r['resolved'] == 'True' else 'fail'} ${float(r['cost']):.3f}")
        print(f"{i:<40} " + " ".join(f"{c:>{width}}" for c in cells))

    print()
    for name, _, rows in runs:
        n = len(rows)
        resolved = sum(r["resolved"] == "True" for r in rows.values())
        cost = sum(float(r["cost"]) for r in rows.values())
        calls = sum(int(r["model_calls"]) for r in rows.values())
        inp = sum(int(r["input"]) for r in rows.values())
        cached = sum(int(r["cached"]) for r in rows.values())
        per = f"${cost / resolved:.4f}" if resolved else "n/a"
        print(f"{name:<{width}}  resolved {resolved}/{n}  ${cost:.3f} total  {per} per resolved  "
              f"{calls / max(n, 1):.1f} calls/instance  {100 * cached / max(inp, 1):.0f}% cached")

    if len(runs) > 1:
        print("\nFlips (resolved in some runs but not others); read these logs:")
        for i in ids:
            results = [rows[i]["resolved"] == "True" for _, _, rows in runs if i in rows]
            if len(set(results)) > 1:
                logs = [str(path / "agent" / f"{i}.{rows[i]['config']}.log") for _, path, rows in runs if i in rows]
                print(f"  {i}: " + " ".join("PASS" if x else "fail" for x in results))
                for log in logs:
                    print(f"      {log}")


if __name__ == "__main__":
    main()
