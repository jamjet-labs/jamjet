# The mistake: run an agent that needs approval, without the engine.
from agent import PROMPT, agent

agent.run_sync(PROMPT)
