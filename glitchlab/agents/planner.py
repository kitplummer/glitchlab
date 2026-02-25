"""
🧠 Professor Zap — The Planner

Breaks down tasks into execution steps.
Identifies risks, maps impacted files, decides scope.
Never writes code. Only plans.

Energy: manic genius with whiteboard chaos.
"""

from __future__ import annotations

import json
from typing import Any, Literal, NotRequired, TypedDict

from loguru import logger

from glitchlab.agents import AgentContext, BaseAgent
from glitchlab.router import RouterResponse


class Step(TypedDict):
    step_number: int
    description: str
    files: list[str]
    action: Literal["modify", "create", "delete"]


class DecompositionItem(TypedDict):
    step_number: int
    description: str
    files: NotRequired[list[str]]
    action: NotRequired[Literal["modify", "create", "delete"]]
    # TODO: Add more fields as needed for decomposition, e.g., "sub_steps" or "plan_id"



class PlannerAgent(BaseAgent):
    role = "planner"

    system_prompt = """You are Professor Zap, the planning engine inside GLITCHLAB.

Your job is to take a development task and produce a precise, actionable execution plan.

You MUST respond with valid JSON only. No markdown, no commentary.

Output schema:
{
  "steps": [
    {
      "step_number": 1,
      "description": "What to do",
      "files": ["path/to/file.rs"],
      "action": "modify|create|delete"
    }
  ],
  "files_likely_affected": ["path/to/file1", "path/to/file2"],
  "requires_core_change": false,
  "risk_level": "low|medium|high",
  "risk_notes": "Why this risk level",
  "test_strategy": ["What tests to add or run"],
  "estimated_complexity": "trivial|small|medium|large",
  "dependencies_affected": false,
  "public_api_changed": false
}

Rules:
- Be precise about file paths. Use the file context provided.
- Keep steps minimal. Fewer steps = fewer errors.
- Flag core changes honestly — this triggers human review.
- If the task is ambiguous, say so in risk_notes.
- Never suggest changes outside the task scope.
- Consider test strategy for every plan.
"""

    def build_messages(self, context: AgentContext) -> list[dict[str, str]]:
        file_context = ""
        if context.file_context:
            file_context = "\n\nRelevant file contents:\n"
            for fname, content in context.file_context.items():
                file_context += f"\n--- {fname} ---\n{content}\n"

        user_content = f"""Task: {context.objective}

Repository: {context.repo_path}
Task ID: {context.task_id}

Constraints:
{chr(10).join(f'- {c}' for c in context.constraints) if context.constraints else '- None specified'}

Acceptance Criteria:
{chr(10).join(f'- {c}' for c in context.acceptance_criteria) if context.acceptance_criteria else '- Tests pass, clean diff'}
{file_context}

Produce your execution plan as JSON."""

        return [self._system_msg(), self._user_msg(user_content)]

    def parse_response(self, response: RouterResponse, context: AgentContext) -> dict[str, Any]:
        """Parse the JSON plan from Professor Zap."""
        content = response.content.strip()

        # Strip markdown code fences if present
        if content.startswith("```"):
            lines = content.split("\n")
            lines = [l for l in lines if not l.strip().startswith("```")]
            content = "\n".join(lines)

        try:
            plan = json.loads(content)
        except json.JSONDecodeError as e:
            logger.error(f"[ZAP] Failed to parse plan JSON: {e}")
            logger.debug(f"[ZAP] Raw response: {content[:500]}")
            plan = {
                "steps": [],
                "files_likely_affected": [],
                "requires_core_change": False,
                "risk_level": "high",
                "risk_notes": f"Failed to parse planner output: {e}",
                "test_strategy": [],
                "estimated_complexity": "unknown",
                "parse_error": True,
                "raw_response": content[:1000],
            }

        plan["_agent"] = "planner"
        plan["_model"] = response.model
        plan["_tokens"] = response.tokens_used
        plan["_cost"] = response.cost

        logger.info(
            f"[ZAP] Plan ready — "
            f"{len(plan.get('steps', []))} steps, "
            f"risk={plan.get('risk_level', '?')}, "
            f"core_change={plan.get('requires_core_change', False)}"
        )

        plan[\"_agent\"] = \"planner\"
        plan[\"_model\"] = response.model
        plan[\"_tokens\"] = response.tokens_used
        plan[\"_cost\"] = response.cost

        # Simulate decomposition for medium and large complexity plans
        chunked_steps = self._chunk_steps(plan)
        if chunked_steps is not None:
            plan["decomposition"] = chunked_steps
            plan["steps"] = []  # Ensure steps field is empty if decomposed
        else:
            plan["decomposition"] = [] # Ensure decomposition field is empty if not decomposed

        logger.info(
            f\"[ZAP] Plan ready — \"
            f\"{len(plan.get(\'steps\', []))} steps, \"
            f\"risk={plan.get(\'risk_level\', \'?\')}, \"
            f\"core_change={plan.get(\'requires_core_change\', False)}\"\
        )

        return plan

    def _chunk_steps(self, plan: dict[str, Any]) -> list[DecompositionItem] | None:
        \"\"\"
        Simulates the decomposition of a plan into smaller chunks based on complexity.
        For 'medium' and 'large' complexity, it returns a list of DecompositionItem.
        Otherwise, it returns None, indicating no decomposition.
        \"\"\"
        complexity = plan.get("estimated_complexity", "unknown")
        steps = plan.get("steps", [])

        if complexity in ["medium", "large"]:
            decomposed_items: list[DecompositionItem] = []
            for i, step in enumerate(steps):
                decomposed_items.append(
                    DecompositionItem(
                        step_number=step["step_number"],
                        description=f"Decomposed step {step['step_number']}",
                        # files=step.get("files", []), # Optional, depending on how granular decomposition gets
                        # action=step.get("action", "modify"),
                    )
                )
            return decomposed_items
        return None
