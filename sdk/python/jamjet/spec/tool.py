from typing import Any

from pydantic import BaseModel, ConfigDict


class ToolSpec(BaseModel):
    """A tool callable by an agent. Resolved at runtime via handler_ref."""

    model_config = ConfigDict(frozen=True, extra="forbid")

    name: str
    description: str
    input_schema: dict[str, Any]
    handler_ref: str
    is_mcp: bool = False
