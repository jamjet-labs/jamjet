"""Shared LLM client — tries Anthropic (async), then OpenAI (async), then raises."""

from __future__ import annotations

from dataclasses import dataclass


@dataclass
class LlmResponse:
    text: str
    input_tokens: int = 0
    output_tokens: int = 0


async def call_llm(model: str, prompt: str, max_tokens: int = 512) -> LlmResponse:
    """Call an LLM with auto-detected SDK. Tries Anthropic first, then OpenAI."""
    errors: list[str] = []

    # Anthropic Claude (async)
    try:
        from anthropic import AsyncAnthropic

        client = AsyncAnthropic()
        msg = await client.messages.create(
            model=model,
            max_tokens=max_tokens,
            messages=[{"role": "user", "content": prompt}],
        )
        text_blocks = [b for b in msg.content if hasattr(b, "text")]
        return LlmResponse(
            text=text_blocks[0].text if text_blocks else "",
            input_tokens=getattr(msg.usage, "input_tokens", 0),
            output_tokens=getattr(msg.usage, "output_tokens", 0),
        )
    except ImportError:
        errors.append("anthropic SDK not installed")
    except Exception as e:
        errors.append(f"anthropic: {e}")

    # OpenAI (async)
    try:
        from openai import AsyncOpenAI

        client = AsyncOpenAI()
        resp = await client.chat.completions.create(
            model=model,
            messages=[{"role": "user", "content": prompt}],
            max_tokens=max_tokens,
        )
        usage = resp.usage
        return LlmResponse(
            text=resp.choices[0].message.content or "",
            input_tokens=getattr(usage, "prompt_tokens", 0) if usage else 0,
            output_tokens=getattr(usage, "completion_tokens", 0) if usage else 0,
        )
    except ImportError:
        errors.append("openai SDK not installed")
    except Exception as e:
        errors.append(f"openai: {e}")

    raise RuntimeError(f"LLM call failed: {'; '.join(errors)}")
