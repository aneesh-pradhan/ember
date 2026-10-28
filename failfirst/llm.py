"""Thin client for Nemotron on Nebius Token Factory (OpenAI-compatible API)."""

from __future__ import annotations

import json
import re
import time
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from typing import Any

from openai import AsyncOpenAI

from .config import Settings

Emit = Callable[..., Awaitable[None]]

_THINK_RE = re.compile(r"<think>.*?</think>", re.DOTALL)
_FENCED_JSON_RE = re.compile(r"```(?:json)?\s*(\{.*?\})\s*```", re.DOTALL)


class LLMError(RuntimeError):
    pass


@dataclass
class Usage:
    calls: int = 0
    prompt_tokens: int = 0
    completion_tokens: int = 0
    seconds: float = 0.0


@dataclass
class LLM:
    """Routes each call to a model tier and records per-model usage."""

    settings: Settings
    emit: Emit
    usage: dict[str, Usage] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if not self.settings.api_key:
            raise LLMError("NEBIUS_API_KEY is not set. Add it to .env (see .env.example).")
        self._client = AsyncOpenAI(
            api_key=self.settings.api_key,
            base_url=self.settings.base_url,
            timeout=300,
            max_retries=2,
        )

    def model_for(self, role: str) -> str:
        return self.settings.models[role]

    async def chat(
        self,
        role: str,
        messages: list[dict[str, str]],
        *,
        purpose: str,
        temperature: float = 0.2,
        max_tokens: int = 4096,
        json_mode: bool = False,
    ) -> str:
        model = self.model_for(role)
        kwargs: dict[str, Any] = {}
        if json_mode and self.settings.json_mode:
            kwargs["response_format"] = {"type": "json_object"}
        started = time.monotonic()
        response = await self._client.chat.completions.create(
            model=model,
            messages=messages,
            temperature=temperature,
            max_tokens=max_tokens,
            **kwargs,
        )
        elapsed = time.monotonic() - started

        usage = self.usage.setdefault(role, Usage())
        usage.calls += 1
        usage.seconds += elapsed
        if response.usage:
            usage.prompt_tokens += response.usage.prompt_tokens or 0
            usage.completion_tokens += response.usage.completion_tokens or 0

        message = response.choices[0].message
        text = _THINK_RE.sub("", message.content or "").strip()
        await self.emit(
            "llm_call",
            role=role,
            model=model,
            purpose=purpose,
            seconds=round(elapsed, 2),
            prompt_tokens=response.usage.prompt_tokens if response.usage else None,
            completion_tokens=response.usage.completion_tokens if response.usage else None,
        )
        if not text:
            raise LLMError(f"{model} returned an empty response for: {purpose}")
        return text

    async def chat_json(
        self,
        role: str,
        messages: list[dict[str, str]],
        *,
        purpose: str,
        temperature: float = 0.2,
        max_tokens: int = 4096,
    ) -> dict[str, Any]:
        """Ask for a JSON object; on a parse failure, ask the model once to repair it."""
        text = await self.chat(
            role, messages, purpose=purpose, temperature=temperature,
            max_tokens=max_tokens, json_mode=True,
        )
        try:
            return extract_json(text)
        except ValueError as exc:
            repair = messages + [
                {"role": "assistant", "content": text},
                {"role": "user", "content": f"That was not valid JSON ({exc}). Reply with only the corrected JSON object."},
            ]
            text = await self.chat(
                role, repair, purpose=f"{purpose} (repair JSON)", temperature=0.0,
                max_tokens=max_tokens, json_mode=True,
            )
            return extract_json(text)

    def usage_summary(self) -> dict[str, dict[str, Any]]:
        return {
            role: {
                "model": self.model_for(role),
                "calls": u.calls,
                "prompt_tokens": u.prompt_tokens,
                "completion_tokens": u.completion_tokens,
                "seconds": round(u.seconds, 1),
            }
            for role, u in self.usage.items()
        }


def extract_json(text: str) -> dict[str, Any]:
    """Pull the first JSON object out of a model reply (bare or inside a code fence)."""
    candidates = [m.group(1) for m in _FENCED_JSON_RE.finditer(text)]
    start = text.find("{")
    end = text.rfind("}")
    if start != -1 and end > start:
        candidates.append(text[start : end + 1])
    for candidate in candidates:
        try:
            value = json.loads(candidate)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            return value
    raise ValueError("no JSON object found in reply")
