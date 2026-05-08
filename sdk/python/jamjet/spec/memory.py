from typing import Literal

from pydantic import BaseModel, ConfigDict

from jamjet.spec.llm import LLMConfig


class MemoryConfig(BaseModel):
    """Engram v2 integration config. Zero-arg default produces a working config."""

    model_config = ConfigDict(frozen=True, extra="forbid")

    enabled: bool = True
    backend: Literal["engram_embedded", "engram_remote", "none"] = "engram_embedded"
    default_mode: Literal["recall", "context", "synthesis"] = "context"
    default_role_filter: tuple[str, ...] | None = None
    default_token_budget: int | None = None
    use_classifier: bool = True
    decompose: bool = False
    db_path: str | None = None
    remote_url: str | None = None
    llm: LLMConfig | None = None
