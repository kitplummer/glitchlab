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
