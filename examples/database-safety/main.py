"""
JamJet Runtime Safety — destructive tool calls, intercepted before the LLM.

A simulated agent handling the request "clean up old customer accounts"
proposes six tool calls. JamJet's runtime policy:

  - blocks any call matching delete_*, drop_*, or *truncate*
  - routes any call matching payment.* for human approval

Three of the six calls are intercepted before they ever reach the LLM.

This demo runs offline with no API key — it uses the local PolicyEvaluator
directly. In a production app you would call:

    import jamjet
    jamjet.cloud.configure(api_key="jj_...", project="my-agent")
    jamjet.cloud.policy("block", "delete_*")
    jamjet.cloud.policy("require_approval", "payment.*")

After which every OpenAI / Anthropic call in the process is automatically
filtered by the same policy engine this script exercises directly.
"""

from jamjet.cloud.policy import PolicyEvaluator


def main() -> None:
    print("┌──────────────────────────────────────────────────────────────────────┐")
    print("│  JamJet Runtime Safety  —  destructive tool calls, intercepted       │")
    print("└──────────────────────────────────────────────────────────────────────┘")
    print()

    evaluator = PolicyEvaluator()
    evaluator.add("block", "delete_*")
    evaluator.add("block", "drop_*")
    evaluator.add("block", "*truncate*")
    evaluator.add("require_approval", "payment.*")

    print("Runtime policy:")
    print("  blocked_tools         delete_*  ·  drop_*  ·  *truncate*")
    print("  require_approval_for  payment.*")
    print()

    print('User request: "clean up old customer accounts"')
    print()

    print("Agent proposed these tool calls:")
    tools = [
        {"function": {"name": "list_users"}},
        {"function": {"name": "delete_users"}},
        {"function": {"name": "delete_user_sessions"}},
        {"function": {"name": "fetch_user_metadata"}},
        {"function": {"name": "drop_inactive_accounts_table"}},
        {"function": {"name": "payment.refund"}},
    ]
    for t in tools:
        print(f"  - {t['function']['name']}")
    print()

    print("Runtime policy decision:")
    for t in tools:
        name = t["function"]["name"]
        decision = evaluator.evaluate(name)
        if decision.blocked:
            print(f"  ✗ BLOCKED      {name:<34}  matched {decision.pattern}")
        elif decision.policy_kind == "require_approval":
            print(f"  ⏸  APPROVAL    {name:<34}  matched {decision.pattern}")
        else:
            print(f"  ✓ ALLOWED      {name:<34}")
    print()

    allowed, blocked = evaluator.filter_tools(tools)
    print(f"  {len(blocked)} destructive call(s) intercepted before reaching the LLM.")
    print(f"  {len(allowed)} safe tool(s) passed through.")
    print()
    print("The model never sees the blocked tools. They cannot be invoked.")


if __name__ == "__main__":
    main()
