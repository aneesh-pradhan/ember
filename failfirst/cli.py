"""Command line: `failfirst run <issue-url>`, `failfirst serve`, `failfirst models`."""

from __future__ import annotations

import argparse
import asyncio
import sys

from .config import Settings


def _print_event(event: dict) -> None:
    kind = event["type"]
    t = f"[{event['t']:7.1f}s]"
    if kind == "stage":
        print(f"{t} == {event['stage']}: {event['status']} {event.get('detail', '')}")
    elif kind == "llm_call":
        print(f"{t}    {event['model']} · {event['purpose']} ({event['seconds']}s)")
    elif kind == "exec":
        branch = f"{event['branch']} " if event.get("branch") else ""
        print(f"{t}    sandbox {branch}{event['label']}: exit {event['exit_code']} ({event['seconds']}s)")
    elif kind == "repro":
        print(f"{t}    repro attempt {event['attempt']}: {event['verdict']} - {event['reason']}")
    elif kind == "branch":
        print(f"{t}    {event['branch']} [{event['status']}] {event.get('note', '')}")
    elif kind in ("note", "error"):
        print(f"{t}    {event.get('text') or event.get('message')}")


async def _run(args: argparse.Namespace) -> int:
    from .agent import Agent, Job
    from .events import RunLog

    settings = Settings.from_env()
    log = RunLog(settings.runs_dir)
    agent = Agent(settings, log)
    job = Job(issue_url=args.issue_url, repo=args.repo, description=args.description, branches=args.branches)

    async def tail():
        async for event in log.follow():
            _print_event(event)

    printer = asyncio.create_task(tail())
    result = await agent.run(job)
    await printer
    print(f"\nStatus: {result['status']}  (events saved to {log.path})")
    if result.get("diff"):
        print(f"\n{result['pr_title']}\n\n{result['diff']}")
    elif result.get("message"):
        print(result["message"])
    return 0 if result["status"] == "fixed" else 1


async def _models() -> int:
    from openai import AsyncOpenAI

    settings = Settings.from_env()
    client = AsyncOpenAI(api_key=settings.api_key, base_url=settings.base_url)
    page = await client.models.list()
    for model in sorted(m.id for m in page.data):
        if "nemotron" in model.lower():
            print(model)
    return 0


def main() -> None:
    parser = argparse.ArgumentParser(prog="failfirst", description="Reproduce a bug with a failing test, then fix it.")
    sub = parser.add_subparsers(dest="command", required=True)

    run = sub.add_parser("run", help="run the agent on an issue")
    run.add_argument("issue_url", nargs="?", help="GitHub issue URL")
    run.add_argument("--repo", help="GitHub repo URL or local path (with --description instead of an issue URL)")
    run.add_argument("--description", help="bug description")
    run.add_argument("--branches", type=int, help="parallel fix branches")

    serve = sub.add_parser("serve", help="start the web UI")
    serve.add_argument("--host", default="127.0.0.1")
    serve.add_argument("--port", type=int, default=8000)

    sub.add_parser("models", help="list Nemotron models available to your key")

    args = parser.parse_args()
    if args.command == "run":
        if not args.issue_url and not (args.repo and args.description):
            parser.error("give an issue URL, or --repo and --description")
        sys.exit(asyncio.run(_run(args)))
    if args.command == "models":
        sys.exit(asyncio.run(_models()))
    import uvicorn

    uvicorn.run("failfirst.server:app", host=args.host, port=args.port)


if __name__ == "__main__":
    main()
