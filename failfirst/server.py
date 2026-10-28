"""FastAPI app: start runs, stream their events over SSE, and replay saved runs."""

from __future__ import annotations

import asyncio
import json
from pathlib import Path

from fastapi import FastAPI, HTTPException
from fastapi.responses import FileResponse, PlainTextResponse, StreamingResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel

from .agent import Agent, Job
from .config import Settings
from .events import RunLog, load_events

WEB_DIR = Path(__file__).resolve().parent.parent / "web"
EXAMPLES_DIR = Path(__file__).resolve().parent.parent / "examples" / "runs"

settings = Settings.from_env()
app = FastAPI(title="FailFirst")
live: dict[str, RunLog] = {}
_tasks: set[asyncio.Task] = set()


class RunRequest(BaseModel):
    issue_url: str | None = None
    repo: str | None = None
    description: str | None = None
    branches: int | None = None


def _run_file(run_id: str) -> Path | None:
    for folder in (settings.runs_dir, EXAMPLES_DIR):
        path = folder / f"{run_id}.jsonl"
        if path.resolve().parent == folder.resolve() and path.exists():
            return path
    return None


def _summarize(path: Path, recorded: bool) -> dict:
    events = load_events(path)
    issue = next((e for e in events if e["type"] == "issue"), {})
    result = next((e for e in reversed(events) if e["type"] == "result"), {})
    return {
        "id": path.stem,
        "title": issue.get("title", "(no issue)"),
        "repo": issue.get("repo"),
        "status": result.get("status", "running"),
        "seconds": events[-1]["t"] if events else 0,
        "recorded": recorded,
    }


@app.get("/api/config")
def config() -> dict:
    return {
        "models": settings.models,
        "sandbox": settings.sandbox,
        "branches": settings.branches,
        "live_runs": settings.allow_live_runs and bool(settings.api_key),
    }


@app.get("/api/runs")
def list_runs() -> list[dict]:
    runs = []
    for folder, recorded in ((EXAMPLES_DIR, True), (settings.runs_dir, False)):
        if folder.exists():
            runs += [_summarize(p, recorded) for p in sorted(folder.glob("*.jsonl"), reverse=True)]
    return runs


@app.post("/api/runs")
async def start_run(request: RunRequest) -> dict:
    if not (settings.allow_live_runs and settings.api_key):
        raise HTTPException(403, "Live runs are disabled on this deployment; replay a recorded run instead.")
    if any(not log.done for log in live.values()):
        raise HTTPException(429, "A run is already in progress; watch it or try again shortly.")
    if not request.issue_url and not (request.repo and request.description):
        raise HTTPException(400, "Provide a GitHub issue URL, or a repository and a bug description.")
    branches = max(1, min(request.branches or settings.branches, 4))
    log = RunLog(settings.runs_dir)
    live[log.id] = log
    agent = Agent(settings, log)
    job = Job(issue_url=request.issue_url, repo=request.repo, description=request.description, branches=branches)
    task = asyncio.create_task(agent.run(job))
    _tasks.add(task)
    task.add_done_callback(_tasks.discard)
    return {"id": log.id}


@app.get("/api/runs/{run_id}/events")
async def events(run_id: str, speed: float = 0.0) -> StreamingResponse:
    """Server-sent events. For a saved run, `speed` > 0 replays it with its original pacing scaled by speed."""

    async def stream():
        if run_id in live:
            async for event in live[run_id].follow():
                yield f"data: {json.dumps(event)}\n\n"
        else:
            path = _run_file(run_id)
            if not path:
                yield f"data: {json.dumps({'type': 'error', 'message': 'run not found'})}\n\n"
                return
            previous = 0.0
            for event in load_events(path):
                if speed > 0:
                    await asyncio.sleep(min((event["t"] - previous) / speed, 1.5))
                    previous = event["t"]
                yield f"data: {json.dumps(event)}\n\n"
        yield "event: end\ndata: {}\n\n"

    return StreamingResponse(stream(), media_type="text/event-stream", headers={"Cache-Control": "no-cache"})


@app.get("/api/runs/{run_id}/patch", response_class=PlainTextResponse)
def patch(run_id: str) -> PlainTextResponse:
    events = live[run_id].events if run_id in live else load_events(_run_file(run_id) or Path("/nonexistent"))
    result = next((e for e in reversed(events) if e["type"] == "result" and e.get("diff")), None)
    if not result:
        raise HTTPException(404, "This run has no patch")
    return PlainTextResponse(
        result["diff"], media_type="text/x-diff",
        headers={"Content-Disposition": f'attachment; filename="failfirst-{run_id}.patch"'},
    )


@app.get("/")
def index() -> FileResponse:
    return FileResponse(WEB_DIR / "index.html")


app.mount("/static", StaticFiles(directory=WEB_DIR), name="static")
