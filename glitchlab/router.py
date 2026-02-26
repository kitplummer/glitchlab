"""
GLITCHLAB Router — Vendor-Agnostic Model Abstraction

Routes agent calls through LiteLLM so agents never know
which vendor is backing them. Handles budget tracking,
retries, and structured logging.
"""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from typing import Any

import litellm
from loguru import logger
from pydantic import BaseModel
from tenacity import retry, stop_after_attempt, wait_exponential

from glitchlab.config_loader import GlitchLabConfig


# ---------------------------------------------------------------------------
# Usage Tracking
# ---------------------------------------------------------------------------

@dataclass
class UsageRecord:
    prompt_tokens: int = 0
    completion_tokens: int = 0
    total_tokens: int = 0
    estimated_cost: float = 0.0
    call_count: int = 0


@dataclass
class BudgetTracker:
    """Tracks token + dollar spend per task."""
    max_tokens: int = 150_000
    max_dollars: float = 10.0
    usage: UsageRecord = field(default_factory=UsageRecord)

    @property
    def tokens_remaining(self) -> int:
        return max(0, self.max_tokens - self.usage.total_tokens)

    @property
    def dollars_remaining(self) -> float:
        return max(0.0, self.max_dollars - self.usage.estimated_cost)

    @property
    def budget_exceeded(self) -> bool:
        return self.usage.total_tokens >= self.max_tokens or self.usage.estimated_cost >= self.max_dollars

    def record(self, response: Any) -> None:
        """Record usage from a LiteLLM response."""
        usage = getattr(response, "usage", None)
        if usage:
            self.usage.prompt_tokens += getattr(usage, "prompt_tokens", 0)
            self.usage.completion_tokens += getattr(usage, "completion_tokens", 0)
            self.usage.total_tokens += getattr(usage, "total_tokens", 0)

        # LiteLLM cost estimation
        try:
            cost = litellm.completion_cost(completion_response=response)
            self.usage.estimated_cost += cost
        except Exception:
            pass  # Cost estimation isn't always available

        self.usage.call_count += 1

    def summary(self) -> dict:
        return {
            "total_tokens": self.usage.total_tokens,
            "estimated_cost": round(self.usage.estimated_cost, 4),
            "call_count": self.usage.call_count,
            "tokens_remaining": self.tokens_remaining,
            "dollars_remaining": round(self.dollars_remaining, 4),
        }


# ---------------------------------------------------------------------------
# Router
# ---------------------------------------------------------------------------

class AgentMessage(BaseModel):
    role: str  # "system" | "user" | "assistant"
    content: str


class RouterResponse(BaseModel):
    content: str
    model: str
    tokens_used: int = 0
    cost: float = 0.0
    latency_ms: int = 0


class Router:
    """
    Vendor-agnostic model router.

    Agents call `router.complete(role, messages)`.
    The router resolves the model, enforces budget, and returns structured output.
    """

    def __init__(self, config: GlitchLabConfig):
        self.config = config
        self.budget = BudgetTracker(
            max_tokens=config.limits.max_tokens_per_task,
            max_dollars=config.limits.max_dollars_per_task,
        )
        self._role_model_map = {
            "planner": config.routing.planner,
            "implementer": config.routing.implementer,
            "debugger": config.routing.debugger,
            "security": config.routing.security,
            "release": config.routing.release,
            "archivist": config.routing.archivist,
        }

        # Suppress litellm verbose logging
        litellm.suppress_debug_info = True

    def resolve_model(self, role: str) -> str:
        """Resolve agent role → model string."""
        model = self._role_model_map.get(role)
        if not model:
            raise ValueError(f"Unknown agent role: {role}. Known: {list(self._role_model_map)}")
        return model

    @retry(stop=stop_after_attempt(3), wait=wait_exponential(min=1, max=10))
    def complete(
        self,
        role: str,
        messages: list[dict[str, str]],
        temperature: float = 0.2,
        max_tokens: int = 4096,
        response_format: dict | None = None,
    ) -> RouterResponse:
        """
        Send a completion request through LiteLLM.

        Args:
            role: Agent role name (planner, implementer, etc.)
            messages: Standard chat messages [{"role": ..., "content": ...}]
            temperature: Sampling temperature
            max_tokens: Max response tokens
            response_format: Optional JSON schema for structured output
        """
        if self.budget.budget_exceeded:
            raise BudgetExceededError(
                f"Budget exceeded: {self.budget.summary()}"
            )

        model = self.resolve_model(role)
        start = time.monotonic()

        logger.debug(f"[ROUTER] {role} → {model} ({len(messages)} messages)")

        kwargs: dict[str, Any] = {
            "model": model,
            "messages": messages,
            "temperature": temperature,
            "max_tokens": max_tokens,
        }
        if response_format:
            kwargs["response_format"] = response_format

        response = litellm.completion(**kwargs)
        elapsed_ms = int((time.monotonic() - start) * 1000)

        # Track budget
        self.budget.record(response)

        content = response.choices[0].message.content or ""

        logger.debug(
            f"[ROUTER] {role} complete — "
            f"{self.budget.usage.total_tokens} tokens, "
            f"${self.budget.usage.estimated_cost:.4f}, "
            f"{elapsed_ms}ms"
        )

        return RouterResponse(
            content=content,
            model=model,
            tokens_used=getattr(response.usage, "total_tokens", 0),
            cost=self.budget.usage.estimated_cost,
            latency_ms=elapsed_ms,
        )


def select_with_fallbacks(primary_model: str, fallback_models: list[str]) -> list[str]:
    """
    Returns an ordered list of models, with the primary model first,
    followed by unique fallback models.
    """
    models = [primary_model]
    for model in fallback_models:
        if model not in models:
            models.append(model)
    return models


class BudgetExceededError(Exception):
    pass
