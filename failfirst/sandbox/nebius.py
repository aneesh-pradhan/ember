"""Nebius Sandboxes backend (contree-sdk): every run is a microVM, checkpoints are image UUIDs."""

from __future__ import annotations

import time

from contree_sdk import Contree

from . import REPO_DIR, SANDBOX_ENV, ExecResult, Sandbox, Snapshot

_REGISTRY_PREFIXES = ("docker://", "docker.io/", "ghcr.io/", "quay.io/", "registry.")


def _text(value) -> str:
    if value is None:
        return ""
    if isinstance(value, bytes):
        return value.decode(errors="replace")
    return str(value)


def _timed_out(image) -> bool:
    raw = getattr(getattr(image, "result", None), "_raw", None)
    state = getattr(getattr(raw, "result", None), "state", None)
    return bool(getattr(state, "timed_out", False))


class NebiusSandbox(Sandbox):
    name = "Nebius Sandboxes"

    def __init__(self, image: str):
        self.image = image
        # Reads NEBIUS_API_KEY and NEBIUS_PROJECT_ID from the environment.
        self.client = Contree()

    async def start(self) -> Snapshot:
        if self.image.startswith(_REGISTRY_PREFIXES):
            base = await self.client.images.oci(self.image)
        else:  # an image UUID or tag already in the project
            base = await self.client.images.use(self.image, strict=True)
        return NebiusSnapshot(base, "base")


class NebiusSnapshot(Snapshot):
    def __init__(self, image, label: str):
        self.image = image
        self.label = label

    async def run(self, shell, *, files=None, timeout=300, save=False, network=False):
        # Sandboxes enables networking by default; the SDK has no per-run switch yet.
        kwargs = {
            "shell": f"mkdir -p {REPO_DIR} && cd {REPO_DIR} && {shell}",
            "env": SANDBOX_ENV,
            "timeout": timeout,
            "disposable": not save,
        }
        if files:
            kwargs["files"] = {f"{REPO_DIR}/{path}": content.encode() for path, content in files.items()}

        started = time.monotonic()
        done = await self.image.run(**kwargs)
        result = ExecResult(
            exit_code=done.exit_code,
            stdout=_text(done.stdout),
            stderr=_text(done.stderr),
            seconds=time.monotonic() - started,
            timed_out=_timed_out(done),
        )
        snapshot = None
        if save:
            if done.uuid is None:
                raise RuntimeError("Sandbox run did not return a saved image")
            snapshot = NebiusSnapshot(done, str(done.uuid)[:8])
        return result, snapshot
