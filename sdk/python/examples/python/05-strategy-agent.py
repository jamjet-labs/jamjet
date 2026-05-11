"""Existing Agent() class (now backed by LocalRuntime) with strategy='react'."""

import asyncio

from jamjet import Agent
from jamjet.tools.decorators import tool


@tool
async def reverse(text: str) -> str:
    """Reverse a string."""
    return text[::-1]


async def main() -> None:
    a = Agent(
        "reverser",
        model="gpt-4o",
        tools=[reverse],
        instructions="Use the reverse tool to reverse strings.",
        strategy="react",
    )
    result = await a.run("Reverse the word 'hello'.")
    print(result.output)


if __name__ == "__main__":
    asyncio.run(main())
