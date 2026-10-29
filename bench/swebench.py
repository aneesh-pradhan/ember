#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = ["swebench==5.0.2", "datasets>=3"]
# ///
"""ember on SWE-bench Verified: run ember on real GitHub issues and grade the patches with the official harness.

Each instance runs in its official SWE-bench image (the repo is at /testbed, with the `testbed` conda env), with
ember's static musl binary mounted read-only. ember gets the issue text in print mode with edits allowed and web
tools turned off. Its diff is then graded by `swebench.harness.run_evaluation`, the same check the leaderboard
uses. Each finished instance is appended to the CSV, so a rerun skips instances that already have a result.

    # one-time: a static binary that runs inside the Ubuntu-based SWE-bench images
    podman run --rm -v "$PWD":/src:Z -w /src docker.io/library/rust:1.90-bookworm sh -c \\
      'rustup target add x86_64-unknown-linux-musl && apt-get update && apt-get install -y musl-tools &&
       CARGO_TARGET_DIR=/src/target/musl cargo build --release --locked --target x86_64-unknown-linux-musl'

    uv run bench/swebench.py --instances bench/swebench/verified-20.txt --config super --run-id baseline
    python3 bench/swebench_compare.py baseline other-run   # per-instance flips, cost per resolved task

Needs podman or docker (for podman, `systemctl --user start podman.socket` so the grader can reach it) and
NEBIUS_API_KEY in the environment or ~/.config/ember/env.
"""
import argparse
import concurrent.futures as cf
import csv
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from run import CONFIGS, usage_and_cost  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
HERE = ROOT / "bench" / "swebench"
DATASET = "SWE-bench/SWE-bench_Verified"
DEFAULT_EMBER = ROOT / "target" / "musl" / "x86_64-unknown-linux-musl" / "release" / "ember"
# Repos ship files that aren't valid UTF-8, so decode command output leniently instead of crashing.
TEXT = {"text": True, "encoding": "utf-8", "errors": "replace"}
FIELDS = ["instance_id", "config", "resolved", "patch_lines", "seconds", "model_calls", "tool_calls",
          "input", "cached", "output", "cost", "timed_out", "error"]


def runtime() -> str:
    """The container CLI to use, preferring docker. For podman, point the grader's docker SDK at its socket."""
    if shutil.which("docker"):
        return "docker"
    if not shutil.which("podman"):
        sys.exit("SWE-bench needs docker or podman")
    sock = Path(os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")) / "podman" / "podman.sock"
    if not sock.exists():
        sys.exit("podman's API socket isn't running; start it with: systemctl --user start podman.socket")
    os.environ.setdefault("DOCKER_HOST", f"unix://{sock}")
    return "podman"


def load_api_key() -> None:
    """Make NEBIUS_API_KEY available to pass into containers, reading ember's env file if needed."""
    if os.environ.get("NEBIUS_API_KEY"):
        return
    env_file = Path.home() / ".config" / "ember" / "env"
    for line in env_file.read_text().splitlines() if env_file.exists() else []:
        key, _, value = line.partition("=")
        if key.strip() == "NEBIUS_API_KEY" and value.strip():
            os.environ["NEBIUS_API_KEY"] = value.strip()
            return
    sys.exit("NEBIUS_API_KEY is not set (and not in ~/.config/ember/env)")


def run_agent(rt: str, ember: Path, inst: dict, config: str, prompt: str, timeout: int, logs: Path, run_id: str) -> dict:
    """Run ember on one instance in its SWE-bench image and return its patch plus usage."""
    # The run id keeps concurrent runs (an A/B's two arms) from removing each other's containers.
    name = f"ember-swe-{run_id}-{inst['instance_id']}-{config}".lower()
    env = {**CONFIGS[config], "EMBER_NO_WEB": "1", "EMBER_MOUSE": "off"}
    env_args = [arg for k, v in env.items() for arg in ("-e", f"{k}={v}")]
    subprocess.run([rt, "rm", "-f", name], capture_output=True)
    # `-e NEBIUS_API_KEY` without a value copies it from our environment, so the key never appears in argv.
    started = subprocess.run(
        [rt, "run", "-d", "--name", name, "-e", "NEBIUS_API_KEY", *env_args,
         "-v", f"{ember}:/usr/local/bin/ember:ro,z", f"docker.io/{inst['image']}", "sleep", "infinity"],
        capture_output=True, **TEXT,
    )
    if started.returncode != 0:
        raise RuntimeError(f"couldn't start {inst['image']}: {started.stderr.strip()[-400:]}")
    try:
        # The prompt is passed as an argument ($1) so no shell quoting touches the issue text.
        script = "source /opt/miniconda3/bin/activate testbed && cd /testbed && exec ember --yes -p \"$1\""
        started, timed_out = time.time(), False
        try:
            proc = subprocess.run([rt, "exec", name, "bash", "-c", script, "ember-task", prompt],
                                  capture_output=True, **TEXT, timeout=timeout)
            stdout, stderr = proc.stdout, proc.stderr
        except subprocess.TimeoutExpired as e:
            stdout, stderr, timed_out = _text(e.stdout), _text(e.stderr), True
        seconds = time.time() - started
        (logs / f"{inst['instance_id']}.{config}.log").write_text(f"{stderr}\n--- stdout ---\n{stdout}")
        if not timed_out and "[usage]" not in stderr:
            # ember never finished (container gone, crash, API outage): an error to retry, not a failed attempt.
            raise RuntimeError(f"ember didn't finish: {stderr.strip()[-300:]}")
        # Stage everything (new files included) except ember's own state and binary files (build output such as
        # Sphinx doctrees, which no reference patch contains and the grader can't apply), then diff against HEAD.
        diff = subprocess.run(
            [rt, "exec", "-w", "/testbed", name, "bash", "-c",
             "git add -A -- . ':(exclude).ember' && "
             "git diff --cached --numstat -z HEAD | awk -v RS='\\0' '$1 == \"-\" {print $3}' | "
             "xargs -r -d '\\n' git reset -q -- && git diff --cached --text HEAD"],
            capture_output=True, **TEXT,
        ).stdout
        return {"patch": diff, "seconds": round(seconds, 1), "timed_out": timed_out,
                **usage_and_cost(stderr, CONFIGS[config]["EMBER_MODEL_CODER"])}
    finally:
        subprocess.run([rt, "rm", "-f", name], capture_output=True)


def grade(inst: dict, config: str, patch: str, run_id: str, out: Path) -> bool:
    """Grade one patch with the official SWE-bench harness and return whether it resolves the issue."""
    model = f"ember-{config}"
    preds = out / "preds" / f"{inst['instance_id']}.{config}.jsonl"
    preds.parent.mkdir(exist_ok=True)
    preds.write_text(json.dumps({"instance_id": inst["instance_id"], "model_name_or_path": model,
                                 "model_patch": patch}) + "\n")
    subprocess.run(
        [sys.executable, "-m", "swebench.harness.run_evaluation", "-d", DATASET, "-i", inst["instance_id"],
         "-p", str(preds), "-id", run_id, "--max_workers", "1", "--report_dir", str(out / "reports")],
        cwd=out, capture_output=True, **TEXT,
    )
    report = out / "logs" / "run_evaluation" / run_id / model / inst["instance_id"] / "report.json"
    if not report.exists():
        raise RuntimeError(f"grader wrote no report (see {report.parent})")
    return bool(json.loads(report.read_text())[inst["instance_id"]]["resolved"])


def run_one(rt: str, ember: Path, inst: dict, config: str, prompt: str, timeout: int, run_id: str, out: Path) -> dict:
    row = {"instance_id": inst["instance_id"], "config": config, "resolved": False, "error": ""}
    try:
        r = run_agent(rt, ember, inst, config, prompt, timeout, out / "agent", run_id)
        patch = r.pop("patch")
        row |= r | {"patch_lines": patch.count("\n")}
        if patch.strip():
            row["resolved"] = grade(inst, config, patch, run_id, out)
    except Exception as e:  # keep going; the row records what broke
        row["error"] = f"{type(e).__name__}: {e}"[:300]
    return row


def _text(b) -> str:
    return b.decode("utf-8", errors="replace") if isinstance(b, bytes) else (b or "")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--instances", required=True, help="file of instance ids, one per line (# comments allowed)")
    ap.add_argument("--config", default="super", choices=sorted(CONFIGS))
    ap.add_argument("--ember", default=str(DEFAULT_EMBER), help="a static Linux build of ember")
    ap.add_argument("--prompt", default=str(HERE / "prompt.md"), help="task template with an {issue} placeholder")
    ap.add_argument("--jobs", type=int, default=2, help="instances in parallel (each needs ~2-4 GB of RAM)")
    ap.add_argument("--timeout", type=int, default=1800, help="seconds for ember per instance")
    ap.add_argument("--run-id", default="verified", help="results go in bench/swebench/runs/<run-id>/")
    args = ap.parse_args()

    ember = Path(args.ember).resolve()
    if not ember.exists():
        sys.exit(f"{ember} not found; build the static binary first (see --help)")
    rt = runtime()
    load_api_key()
    from datasets import load_dataset  # slow import; only after the checks above

    ids = [l.split("#")[0].strip() for l in Path(args.instances).read_text().splitlines()]
    ids = [i for i in ids if i]
    by_id = {r["instance_id"]: r for r in load_dataset(DATASET, split="test") if r["instance_id"] in ids}
    if missing := [i for i in ids if i not in by_id]:
        sys.exit(f"not in {DATASET}: {', '.join(missing)}")
    template = Path(args.prompt).read_text()

    out = HERE / "runs" / args.run_id
    (out / "agent").mkdir(parents=True, exist_ok=True)
    results_path = out / "results.csv"
    done = set()
    if results_path.exists():
        with open(results_path) as f:
            done = {(r["instance_id"], r["config"]) for r in csv.DictReader(f) if not r["error"]}
    todo = [i for i in ids if (i, args.config) not in done]
    print(f"{len(ids) - len(todo)} of {len(ids)} already done; running {len(todo)} with {args.config}", flush=True)

    new_file = not results_path.exists()
    with cf.ThreadPoolExecutor(max_workers=args.jobs) as pool, open(results_path, "a", newline="") as f:
        w = csv.DictWriter(f, fieldnames=FIELDS)
        if new_file:
            w.writeheader()
        futures = [pool.submit(run_one, rt, ember, by_id[i], args.config,
                               template.format(issue=by_id[i]["problem_statement"].strip()),
                               args.timeout, args.run_id, out) for i in todo]
        for fut in cf.as_completed(futures):
            row = fut.result()
            w.writerow(row)
            f.flush()
            status = "ERROR" if row["error"] else "PASS " if row["resolved"] else "FAIL "
            print(f"{status} {row['instance_id']:<40} {row.get('seconds', 0):7.1f}s  calls={row.get('model_calls', 0):<3} "
                  f"in={row.get('input', 0):<8} cached={row.get('cached', 0):<8} ${row.get('cost', 0):.4f}  {row['error']}",
                  flush=True)

    with open(results_path) as f:
        rows = [r for r in csv.DictReader(f) if r["config"] == args.config and not r["error"]]
    resolved = sum(r["resolved"] == "True" for r in rows)
    cost = sum(float(r["cost"] or 0) for r in rows)
    print(f"\n{args.config}: resolved {resolved}/{len(rows)}, ${cost:.3f} total, "
          f"${cost / resolved if resolved else float('nan'):.4f} per resolved task")


if __name__ == "__main__":
    main()
