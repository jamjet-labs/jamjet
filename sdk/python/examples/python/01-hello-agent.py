"""Minimal @DurableAgent — class with one method, run via top-level run().

Usage:
    OPENAI_API_KEY=sk-... python examples/python/01-hello-agent.py
"""
import asyncio

from jamjet import DurableAgent, run


@DurableAgent(model="gpt-4o", instructions="Greet warmly.")
class Greeter:
    async def run(self, name: str) -> str:
        msg = await self.llm.generate([
            {"role": "system", "content": "Greet warmly."},
            {"role": "user", "content": f"Say hello to {name}."},
        ])
        return msg.content or ""


async def main() -> None:
    result = await run(Greeter, "Sunil")
    print(result.output)


if __name__ == "__main__":
    asyncio.run(main())
