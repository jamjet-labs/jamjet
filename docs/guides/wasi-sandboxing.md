# Tool Sandboxing

JamJet does not sandbox tools. There is no WASI or Wasm runtime in this repo
and no `wasi_sandbox` tool type.

A Python step (`python_fn`) runs inside the `jamjet worker` process that
claimed it, with that process's filesystem, network access and environment.
A Java step runs the same way in the Java worker. Contain workers the way you
would contain any service that runs code you did not write:

- Run each worker as its own unprivileged user, in a container or VM with only
  the mounts and network routes its tools need.
- Give a worker only the environment variables its tools need. A tool can read
  everything in the worker's environment.
- Gate the tools that change things with `blocked_tools`,
  `require_approval_for` or an Agent Card's `allowed_tools`. The runtime
  checks these when the step is about to be handed to a worker, so a blocked
  step never reaches one and a gated step waits for a decision first. See
  [Security](security.md) and [Human-in-the-Loop](hitl.md).
