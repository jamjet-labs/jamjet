"""Two stateless agents side by side: @DurableAgent(stateless=True) and Agent()."""
import asyncio

from jamjet import Agent, DurableAgent, run


@DurableAgent(stateless=True, model="gpt-4o")
class Throwaway:
    async def run(self, q: str) -> str:
        msg = await self.llm.generate([{"role": "user", "content": q}])
        return msg.content or ""


async def main() -> None:
    out1 = await run(Throwaway, "What is 2+2?")
    print("DurableAgent stateless:", out1.output)

    a = Agent("calc", model="gpt-4o", tools=[], strategy="react")
    out2 = await a.run("What is 3+3?")
    print("Agent imperative:", out2.output)


if __name__ == "__main__":
    asyncio.run(main())
