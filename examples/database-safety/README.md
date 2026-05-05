# Database Safety — Block Destructive Tool Calls at Runtime

Demonstrates JamJet's policy engine intercepting destructive tool calls
before they reach the LLM. The model never sees blocked tools, so it
cannot invoke them.

## Run

```bash
python main.py
```

No API key required — uses the local policy evaluator directly so the
output is deterministic and offline-runnable.

## What it shows

A simulated agent handling the request *"clean up old customer accounts"*
proposes six tool calls. JamJet's runtime policy:

- **Blocks** any call matching `delete_*`, `drop_*`, or `*truncate*`
- **Routes for approval** any call matching `payment.*`

Three of the six tool calls are intercepted before reaching the LLM.

## In production

This demo exercises the policy evaluator directly. In a real app you
would write:

```python
import jamjet
jamjet.cloud.configure(api_key="jj_...", project="my-agent")
jamjet.cloud.policy("block", "delete_*")
jamjet.cloud.policy("require_approval", "payment.*")
```

After which every OpenAI/Anthropic call in the process is automatically
filtered by the same policy engine — no changes to the agent code.

The Rust runtime evaluates the equivalent policies declared in
`workflow.yaml`:

```yaml
policy:
  blocked_tools:
    - "*delete*"
    - "payments.refund"
  require_approval_for:
    - "database.*"
    - "payment.transfer"
    - "user.suspend"
```

See [`examples/hitl-approval`](../hitl-approval) for a full runnable
approval workflow.

## Recording the demo GIF

The repo's top-level `demo.gif` is generated from this example using
[vhs](https://github.com/charmbracelet/vhs):

```bash
# Install vhs if you don't have it:
#   brew install vhs        (macOS)
#   go install github.com/charmbracelet/vhs@latest
#   (or see https://github.com/charmbracelet/vhs#installation)

cd examples/database-safety
vhs demo.tape
mv demo.gif ../../demo.gif
git add ../../demo.gif
git commit -m "chore(demo): refresh demo.gif from database-safety example"
```

`demo.tape` is fully declarative — running `vhs demo.tape` produces the
same GIF every time.
