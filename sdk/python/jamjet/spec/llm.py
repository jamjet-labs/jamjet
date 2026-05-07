from typing import Literal
from pydantic import BaseModel, ConfigDict


class LLMConfig(BaseModel):
    """LLM provider + model selection. Used by AgentSpec.llm and MemoryConfig.llm."""

    model_config = ConfigDict(frozen=True, extra="forbid")

    provider: Literal["openai", "anthropic", "google", "ollama", "openai_compatible"]
    model: str
    base_url: str | None = None
    api_key_env: str = "OPENAI_API_KEY"
    temperature: float | None = None
    max_tokens: int | None = None
