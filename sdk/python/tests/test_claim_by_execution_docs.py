"""Every place that offers ``claim_by_execution=True`` says who can serve it.

A node compiled with ``claim_by_execution=True`` is handed out only to a claim
that names its execution. ``jamjet worker`` and ``JamjetClient.claim_work_item``
claim oldest-first, so they never get such an item and the run waits with no
error. The docs must say so next to the flag, and the client must still be the
oldest-first one they describe.
"""

from __future__ import annotations

import json
import re
from pathlib import Path

import pytest
import respx

from jamjet.client import JamjetClient
from jamjet.compiler.agent_ir import compile_agent_to_ir

REPO = Path(__file__).resolve().parents[3]
BASE = "http://localhost:7700"

# What a reader must learn next to the flag.
REQUIRED = (
    "`jamjet worker`",
    "claim_work_item`",
    "do not claim by execution",
    "`POST /work-items/claim`",
    "`execution_id`",
)


def _block_with(text: str, needle: str) -> str:
    """The paragraph or top-level list item that holds *needle*, on one line."""
    blocks = [b for para in re.split(r"\n\s*\n", text) for b in re.split(r"\n(?=- )", para)]
    found = [b for b in blocks if needle in b]
    assert len(found) == 1, f"expected one block with {needle!r}, found {len(found)}"
    return " ".join(found[0].split())


@pytest.mark.parametrize(
    "path",
    ["docs/guides/deployment.md", "sdk/python/CHANGELOG.md", "CHANGELOG.md"],
)
def test_the_docs_next_to_claim_by_execution_say_the_stock_worker_cannot_serve_it(path: str) -> None:
    block = _block_with((REPO / path).read_text(), "claim_by_execution=True")
    missing = [p for p in REQUIRED if p not in block]
    assert not missing, f"{path}: the claim_by_execution=True text does not say {missing}"


def test_the_docstring_of_claim_by_execution_says_the_stock_worker_cannot_serve_it() -> None:
    doc = " ".join((compile_agent_to_ir.__doc__ or "").replace("``", "`").split())
    section = doc[doc.index("claim_by_execution:") : doc.index("Returns ---")]
    missing = [p for p in REQUIRED if p not in section]
    assert not missing, f"compile_agent_to_ir docstring does not say {missing}"


# Guard: passes today. It pins the fact the docs above state. When
# claim_work_item learns to claim by execution, this fails, and the docs change
# with it.
@respx.mock
@pytest.mark.asyncio
async def test_claim_work_item_claims_oldest_first_only() -> None:
    route = respx.post(f"{BASE}/work-items/claim").respond(200, json={"claimed": False})
    async with JamjetClient(base_url=BASE) as c:
        assert await c.claim_work_item("w1", ["python_tool"], lease_secs=30) is None
    body = json.loads(route.calls[0].request.content)
    assert body == {"worker_id": "w1", "queue_types": ["python_tool"], "lease_secs": 30}
