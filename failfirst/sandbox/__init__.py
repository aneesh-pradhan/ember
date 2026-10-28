"""Isolated execution with checkpoints.

Both backends share one model, taken from Nebius Sandboxes: a Snapshot is an
immutable filesystem image. Running a command on it starts fresh compute and,
if asked, saves the result as a new Snapshot. Branching is just running several
commands from the same Snapshot.
"""

from __future__ import annotations

import os
import shutil
from abc import ABC, abstractmethod
from dataclasses import dataclass

from ..config import Settings

REPO_DIR = "/work/repo"

SANDBOX_ENV = {
    "PIP_DISABLE_PIP_VERSION_CHECK": "1",
    "PIP_ROOT_USER_ACTION": "ignore",
    "PIP_NO_INPUT": "1",
    "PYTHONDONTWRITEBYTECODE": "1",
    "PYTHONUNBUFFERED": "1",
    # Lets setuptools-scm based projects install from a tarball without .git.
    "SETUPTOOLS_SCM_PRETEND_VERSION": "0.0.0",
}


@dataclass
class ExecResult:
    exit_code: int
    stdout: str
    stderr: str
    seconds: float
    timed_out: bool = False

    def output(self, limit: int = 6000) -> str:
        text = self.stdout
        if self.stderr.strip():
            text = f"{text}\n[stderr]\n{self.stderr}" if text.strip() else self.stderr
        return text if len(text) <= limit else "...(truncated)...\n" + text[-limit:]


class Snapshot(ABC):
    label: str

    @abstractmethod
    async def run(
        self,
        shell: str,
        *,
        files: dict[str, str] | None = None,
        timeout: int = 300,
        save: bool = False,
        network: bool = False,
    ) -> tuple[ExecResult, Snapshot | None]:
        """Run `shell` in REPO_DIR on fresh compute from this snapshot.

        `files` maps repo-relative paths to contents written before the command runs.
        With save=True the resulting filesystem is returned as a new Snapshot.
        """


class Sandbox(ABC):
    name: str

    @abstractmethod
    async def start(self) -> Snapshot:
        """Return the base image snapshot."""

    async def close(self) -> None:
        """Release anything this sandbox created."""


def make_sandbox(settings: Settings) -> Sandbox:
    choice = settings.sandbox
    if choice == "auto":
        choice = "nebius" if os.environ.get("NEBIUS_PROJECT_ID") else "podman"
    if choice == "nebius":
        from .nebius import NebiusSandbox

        return NebiusSandbox(settings.base_image)
    if choice == "podman":
        if not shutil.which("podman"):
            raise RuntimeError("No sandbox available: set NEBIUS_PROJECT_ID for Nebius Sandboxes or install podman.")
        from .podman import PodmanSandbox

        return PodmanSandbox(settings.base_image)
    raise ValueError(f"unknown FAILFIRST_SANDBOX value: {settings.sandbox}")
