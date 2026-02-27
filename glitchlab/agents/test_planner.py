import pytest
from glitchlab.agents.planner import PlannerAgent, Step, DecompositionItem

@pytest.fixture
def planner_agent():
    return PlannerAgent()

def test_chunk_steps_trivial(planner_agent):
    plan = {"estimated_complexity": "trivial", "steps": []}
    result = planner_agent._chunk_steps(plan)
    assert result is None

def test_chunk_steps_small(planner_agent):
    plan = {"estimated_complexity": "small", "steps": []}
    result = planner_agent._chunk_steps(plan)
    assert result is None

def test_chunk_steps_medium(planner_agent):
    plan = {"estimated_complexity": "medium", "steps": [
        Step(step_number=1, description="Step 1", files=[], action="modify"),
        Step(step_number=2, description="Step 2", files=[], action="modify"),
    ]}
    result = planner_agent._chunk_steps(plan)
    assert isinstance(result, list)
    assert len(result) == 2
    assert all(isinstance(item, DecompositionItem) for item in result)
    assert result[0]["description"] == "Decomposed step 1"
    assert result[1]["description"] == "Decomposed step 2"

def test_chunk_steps_large(planner_agent):
    plan = {"estimated_complexity": "large", "steps": [
        Step(step_number=1, description="Step 1", files=[], action="modify"),
        Step(step_number=2, description="Step 2", files=[], action="modify"),
        Step(step_number=3, description="Step 3", files=[], action="modify"),
    ]}
    result = planner_agent._chunk_steps(plan)
    assert isinstance(result, list)
    assert len(result) == 3
    assert all(isinstance(item, DecompositionItem) for item in result)
    assert result[0]["description"] == "Decomposed step 1"
    assert result[1]["description"] == "Decomposed step 2"
    assert result[2]["description"] == "Decomposed step 3"

def test_parse_response_with_markdown_fences(planner_agent):
    from glitchlab.router import RouterResponse
    from glitchlab.agents import AgentContext

    mock_json_output = """
```json
{
  "steps": [
    {
      "step_number": 1,
      "description": "Test step",
      "files": ["test.py"],
      "action": "modify"
    }
  ],
  "estimated_complexity": "small"
}
```
"""
    mock_response = RouterResponse(
        content=mock_json_output,
        model="mock_model",
        tokens_used=100,
        cost=0.01
    )
    mock_context = AgentContext(
        objective="test objective",
        repo_path="/tmp",
        task_id="test-123"
    )

    parsed_plan = planner_agent.parse_response(mock_response, mock_context)

    assert "parse_error" not in parsed_plan
    assert parsed_plan["estimated_complexity"] == "small"
    assert len(parsed_plan["steps"]) == 1
    assert parsed_plan["steps"][0]["description"] == "Test step"
    assert parsed_plan["_model"] == "mock_model"
