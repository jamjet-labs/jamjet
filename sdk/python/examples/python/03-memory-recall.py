"""Record then recall, in one durable agent invocation."""

import asyncio

from jamjet import DurableAgent, run


@DurableAgent(model="gpt-4o")
class Concierge:
    async def run(self, topic: str) -> str:
        await self.memory.record("User likes window seats.", role="user")
        return await self.memory.ask(topic)


async def main() -> None:
    result = await run(Concierge, "seating preference")
    print("Recalled context:", result.output)


if __name__ == "__main__":
    asyncio.run(main())
