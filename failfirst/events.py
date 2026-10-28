"""Append-only event log for a run: streamed live to the UI and saved for replay."""

from __future__ import annotations

import asyncio
import json
import time
import uuid
from pathlib import Path
from typing import Any


class RunLog:
    def __init__(self, runs_dir: Path, run_id: str | None = None):
        self.id = run_id or time.strftime("%Y%m%d-%H%M%S-") + uuid.uuid4().hex[:6]
        self.events: list[dict[str, Any]] = []
        self.done = False
        self._started = time.monotonic()
        self._changed = asyncio.Condition()
        runs_dir.mkdir(parents=True, exist_ok=True)
        self.path = runs_dir / f"{self.id}.jsonl"

    async def emit(self, type: str, **data: Any) -> None:
        event = {"type": type, "t": round(time.monotonic() - self._started, 2), **data}
        self.events.append(event)
        with self.path.open("a") as f:
            f.write(json.dumps(event) + "\n")
        async with self._changed:
            self._changed.notify_all()

    async def finish(self) -> None:
        self.done = True
        async with self._changed:
            self._changed.notify_all()

    async def follow(self):
        """Yield every event so far, then new ones as they arrive, until the run ends."""
        index = 0
        while True:
            while index < len(self.events):
                yield self.events[index]
                index += 1
            if self.done:
                return
            async with self._changed:
                await self._changed.wait_for(lambda: self.done or index < len(self.events))


def load_events(path: Path) -> list[dict[str, Any]]:
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
