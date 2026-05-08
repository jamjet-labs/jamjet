from pydantic import BaseModel, ConfigDict


class DurabilityConfig(BaseModel):
    """Durability knobs for a DurableAgentSpec or WorkflowSpec."""

    model_config = ConfigDict(frozen=True, extra="forbid")

    checkpoint_every_step: bool = True
    seed: int | None = None
    db_path: str | None = None
