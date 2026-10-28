"""Runtime settings, read from the environment (and a local .env file)."""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

from dotenv import load_dotenv

load_dotenv()

TOKEN_FACTORY_URL = "https://api.tokenfactory.nebius.com/v1/"

# Three tiers of NVIDIA Nemotron, each used where it pays off:
#   fast     - triage, judging test output (cheap, low latency)
#   coder    - investigating code, writing the repro test and patches
#   reasoner - planning distinct fix strategies and reviewing candidates
DEFAULT_MODELS = {
    "fast": "nvidia/Nemotron-3-Nano-30B-A3B",
    "coder": "nvidia/Nemotron-3-Super-120b-a12b",
    "reasoner": "nvidia/Nemotron-3-Ultra-550b-a55b",
}


def _env_bool(name: str, default: bool) -> bool:
    value = os.environ.get(name)
    if value is None:
        return default
    return value.strip().lower() in {"1", "true", "yes", "on"}


@dataclass(frozen=True)
class Settings:
    api_key: str | None
    base_url: str
    models: dict[str, str]
    sandbox: str  # "auto" | "nebius" | "podman"
    base_image: str
    branches: int
    max_repro_attempts: int
    max_fix_rounds: int
    investigate_steps: int
    test_timeout: int
    setup_timeout: int
    github_token: str | None
    runs_dir: Path
    allow_live_runs: bool
    json_mode: bool

    @classmethod
    def from_env(cls) -> Settings:
        models = {
            role: os.environ.get(f"FAILFIRST_MODEL_{role.upper()}", default)
            for role, default in DEFAULT_MODELS.items()
        }
        return cls(
            api_key=os.environ.get("NEBIUS_API_KEY"),
            base_url=os.environ.get("NEBIUS_BASE_URL", TOKEN_FACTORY_URL),
            models=models,
            sandbox=os.environ.get("FAILFIRST_SANDBOX", "auto"),
            base_image=os.environ.get("FAILFIRST_BASE_IMAGE", "docker.io/library/python:3.12-slim"),
            branches=int(os.environ.get("FAILFIRST_BRANCHES", "3")),
            max_repro_attempts=int(os.environ.get("FAILFIRST_MAX_REPRO_ATTEMPTS", "4")),
            max_fix_rounds=int(os.environ.get("FAILFIRST_MAX_FIX_ROUNDS", "3")),
            investigate_steps=int(os.environ.get("FAILFIRST_INVESTIGATE_STEPS", "8")),
            test_timeout=int(os.environ.get("FAILFIRST_TEST_TIMEOUT", "300")),
            setup_timeout=int(os.environ.get("FAILFIRST_SETUP_TIMEOUT", "900")),
            github_token=os.environ.get("GITHUB_TOKEN"),
            runs_dir=Path(os.environ.get("FAILFIRST_RUNS_DIR", "runs")),
            allow_live_runs=_env_bool("FAILFIRST_ALLOW_LIVE_RUNS", True),
            json_mode=_env_bool("FAILFIRST_JSON_MODE", False),
        )
