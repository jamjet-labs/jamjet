"""Experiment grids must apply each row's trajectory assertions."""

from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock

import pytest

from jamjet.eval.dataset import EvalDataset, EvalRow
from jamjet.eval.grid import ExperimentGrid
from jamjet.eval.scorers import AssertionScorer


@pytest.mark.parametrize("mode", ["runtime", "agent"])
@pytest.mark.parametrize("tool", ["search", "delete"])
async def test_grid_scores_trajectory_for_each_condition_and_seed(monkeypatch, mode, tool):
    output = {"answer": "42"}
    if mode == "runtime":
        import jamjet.client

        client = MagicMock()
        client.__aenter__ = AsyncMock(return_value=client)
        client.__aexit__ = AsyncMock(return_value=None)
        client.start_execution = AsyncMock(return_value={"execution_id": "exec-1"})
        client.get_execution = AsyncMock(return_value={"status": "completed", "output": output})
        client.get_events = AsyncMock(
            return_value={
                "events": [
                    {
                        "kind": {
                            "type": "node_completed",
                            "node_id": "model",
                            "output": {"tool_calls": [{"id": "call-1", "name": tool, "arguments": {}}]},
                        }
                    }
                ]
            }
        )
        monkeypatch.setattr(jamjet.client, "JamjetClient", lambda **kwargs: client)
    else:
        import jamjet.agents.agent

        agent = MagicMock()
        agent.run = AsyncMock(
            return_value=SimpleNamespace(output=output, tool_calls=[{"tool": tool, "input": {}, "output": None}])
        )
        monkeypatch.setattr(jamjet.agents.agent, "Agent", lambda **kwargs: agent)

    row = EvalRow(
        id="with-trajectory",
        input={"task": "Find the answer"},
        expected=output,
        expected_trajectory={"used_tool": "search", "did_not_use": "delete"},
    )
    output_only_row = EvalRow(id="output-only", input=row.input, expected=output)
    grid = ExperimentGrid(
        workflow_id="test-workflow",
        conditions={"model": ["model-a", "model-b"]},
        dataset=EvalDataset([row, output_only_row]),
        scorers=[AssertionScorer(checks=["output == expected"])],
        seeds=[1, 2],
        mode=mode,
        poll_interval_s=0.0,
    )

    results = await grid.run()

    assert len(results.results) == 4
    for condition_result in results.results:
        checked, output_only = condition_result.eval_results
        assert checked.error is None
        assert checked.trajectory == [tool]
        assert checked.input["_condition"] == condition_result.condition
        assert checked.input["_seed"] == condition_result.seed
        assert [scorer.scorer for scorer in checked.scorers] == ["assertion", "trajectory"]
        assert checked.scorers[0].passed
        assert checked.passed is (tool == "search")
        assert output_only.passed
        assert [scorer.scorer for scorer in output_only.scorers] == ["assertion"]
    assert row.input == {"task": "Find the answer"}
