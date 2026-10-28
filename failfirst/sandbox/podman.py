"""Local fallback backend: rootless Podman containers, checkpoints via `podman commit`."""

from __future__ import annotations

import asyncio
import tempfile
import time
import uuid
from pathlib import Path

from . import REPO_DIR, SANDBOX_ENV, ExecResult, Sandbox, Snapshot


async def _podman(*args: str, timeout: float | None = None) -> tuple[int, str, str]:
    proc = await asyncio.create_subprocess_exec(
        "podman", *args, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
    )
    try:
        out, err = await asyncio.wait_for(proc.communicate(), timeout)
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise
    return proc.returncode, out.decode(errors="replace"), err.decode(errors="replace")


class PodmanSandbox(Sandbox):
    name = "Podman (local)"

    def __init__(self, image: str):
        self.image = image
        self._snapshots: list[str] = []

    async def start(self) -> Snapshot:
        code, _, _ = await _podman("image", "exists", self.image)
        if code != 0:
            code, _, err = await _podman("pull", "-q", self.image, timeout=600)
            if code != 0:
                raise RuntimeError(f"podman pull {self.image} failed: {err.strip()}")
        return PodmanSnapshot(self, self.image, "base")

    async def close(self) -> None:
        if self._snapshots:
            await _podman("rmi", "-f", *self._snapshots)
            self._snapshots.clear()


class PodmanSnapshot(Snapshot):
    def __init__(self, sandbox: PodmanSandbox, image: str, label: str):
        self.sandbox = sandbox
        self.image = image
        self.label = label

    async def run(self, shell, *, files=None, timeout=300, save=False, network=False):
        name = f"failfirst-{uuid.uuid4().hex[:12]}"
        env_args = [arg for k, v in SANDBOX_ENV.items() for arg in ("-e", f"{k}={v}")]
        script = f"mkdir -p {REPO_DIR} && cd {REPO_DIR} && {shell}"
        code, _, err = await _podman(
            "create", "--name", name,
            "--memory", "4g", "--cpus", "2", "--pids-limit", "2048",
            *([] if network else ["--network", "none"]),
            *env_args, self.image, "sh", "-c", script,
        )
        if code != 0:
            raise RuntimeError(f"podman create failed: {err.strip()}")
        try:
            if files:
                with tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    for rel, content in files.items():
                        target = root / REPO_DIR.lstrip("/") / rel
                        target.parent.mkdir(parents=True, exist_ok=True)
                        target.write_text(content)
                    code, _, err = await _podman("cp", f"{tmp}/.", f"{name}:/")
                    if code != 0:
                        raise RuntimeError(f"podman cp failed: {err.strip()}")

            started = time.monotonic()
            timed_out = False
            try:
                _, out, err = await _podman("start", "--attach", name, timeout=timeout)
            except TimeoutError:
                timed_out = True
                await _podman("kill", name)
                out, err = "", f"command timed out after {timeout}s"
            seconds = time.monotonic() - started
            _, exit_code, _ = await _podman("inspect", "-f", "{{.State.ExitCode}}", name)
            result = ExecResult(
                exit_code=124 if timed_out else int(exit_code.strip() or 1),
                stdout=out, stderr=err, seconds=seconds, timed_out=timed_out,
            )

            snapshot = None
            if save:
                tag = f"localhost/failfirst-snap:{name.removeprefix('failfirst-')}"
                code, _, err = await _podman("commit", "-q", name, tag)
                if code != 0:
                    raise RuntimeError(f"podman commit failed: {err.strip()}")
                self.sandbox._snapshots.append(tag)
                snapshot = PodmanSnapshot(self.sandbox, tag, tag.rsplit(":", 1)[1])
            return result, snapshot
        finally:
            await _podman("rm", "-f", name)
