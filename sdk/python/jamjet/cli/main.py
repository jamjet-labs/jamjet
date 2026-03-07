"""
JamJet CLI — the main entry point for all `jamjet` commands.

Commands:
  jamjet init           Create a new project from a template
  jamjet dev            Start local dev runtime (SQLite)
  jamjet run            Submit and run a workflow
  jamjet validate       Validate a workflow definition
  jamjet inspect        Show execution state and history
  jamjet events         Show event timeline for an execution
  jamjet agents         Manage agents (list, inspect, activate, deactivate)
  jamjet mcp connect    Test MCP server connectivity
  jamjet a2a discover   Fetch and display a remote Agent Card
  jamjet workers        List active workers
"""

from __future__ import annotations

import asyncio
import json

import typer
from rich.console import Console
from rich.table import Table

from jamjet.client import JamjetClient

app = typer.Typer(
    name="jamjet",
    help="JamJet — agent-native workflow runtime CLI",
    no_args_is_help=True,
)
agents_app = typer.Typer(help="Manage agents", no_args_is_help=True)
mcp_app = typer.Typer(help="MCP server tools", no_args_is_help=True)
a2a_app = typer.Typer(help="A2A agent tools", no_args_is_help=True)

app.add_typer(agents_app, name="agents")
app.add_typer(mcp_app, name="mcp")
app.add_typer(a2a_app, name="a2a")

console = Console()


def _client(runtime: str = "http://localhost:7700") -> JamjetClient:
    return JamjetClient(base_url=runtime)


# ── init ─────────────────────────────────────────────────────────────────────


@app.command()
def init(
    project_name: str = typer.Argument(..., help="Name of the new project"),
) -> None:
    """Create a new JamJet project."""
    import os

    project_dir = os.path.join(os.getcwd(), project_name)
    if os.path.exists(project_dir):
        console.print(f"[red]Error:[/red] directory '{project_name}' already exists")
        raise typer.Exit(1)

    os.makedirs(project_dir)

    workflow_yaml = f"""# {project_name}/workflow.yaml
# Edit this file, then run: jamjet dev  (in another terminal: jamjet run workflow.yaml)
name: {project_name}
version: 0.1.0

state_schema:
  query: str
  result: str

nodes:
  start:
    type: model
    model: default_chat
    prompt: "Answer this question clearly: {{{{ state.query }}}}"
    output_key: result
    next: end

  end:
    type: end
"""

    readme = f"""# {project_name}

A JamJet agent workflow.

## Run

```bash
# Terminal 1 — start runtime
jamjet dev

# Terminal 2 — run the workflow
jamjet run workflow.yaml --input '{{"query": "What is JamJet?"}}'\n```

## Edit

Open `workflow.yaml` to change the workflow. See the [JamJet docs](https://jamjet.dev/docs) for all node types.
"""

    with open(os.path.join(project_dir, "workflow.yaml"), "w") as f:
        f.write(workflow_yaml)
    with open(os.path.join(project_dir, "README.md"), "w") as f:
        f.write(readme)

    console.print(f"[green]✓[/green] Created [bold]{project_name}/[/bold]")
    console.print("  [dim]workflow.yaml[/dim]   ← your workflow (edit this)")
    console.print("  [dim]README.md[/dim]")
    console.print()
    console.print("[bold]Next steps:[/bold]")
    console.print(f"  cd {project_name}")
    console.print("  jamjet dev              [dim]# start the runtime[/dim]")
    console.print("  jamjet run workflow.yaml [dim]# run in another terminal[/dim]")


# ── dev ──────────────────────────────────────────────────────────────────────


def _find_server_binary() -> str:
    """Locate the jamjet-server binary, auto-downloading if necessary."""
    import os
    import shutil

    # 1. Check PATH first (installed wheel or symlinked)
    which = shutil.which("jamjet-server")
    if which:
        return which

    # 2. Look in Rust workspace target directories (contributors building from source)
    cwd = os.getcwd()
    candidates = [
        # Running from repo root
        os.path.join(cwd, "runtime", "target", "debug", "jamjet-server"),
        os.path.join(cwd, "runtime", "target", "release", "jamjet-server"),
        # Running from sdk/python
        os.path.join(cwd, "..", "..", "runtime", "target", "debug", "jamjet-server"),
        os.path.join(cwd, "..", "..", "runtime", "target", "release", "jamjet-server"),
    ]
    for path in candidates:
        if os.path.isfile(path) and os.access(path, os.X_OK):
            return os.path.abspath(path)

    # 3. Check ~/.jamjet/bin/ cache (previously auto-downloaded)
    cache_dir = os.path.join(os.path.expanduser("~"), ".jamjet", "bin")
    cached = os.path.join(cache_dir, "jamjet-server")
    if os.path.isfile(cached) and os.access(cached, os.X_OK):
        return cached

    # 4. Auto-download from GitHub Releases
    return _download_server_binary(cache_dir)


def _download_server_binary(cache_dir: str) -> str:
    """Download the jamjet-server binary for the current platform from GitHub Releases."""
    import os
    import platform
    import stat
    import urllib.request

    system = platform.system().lower()  # darwin, linux, windows
    machine = platform.machine().lower()  # x86_64, arm64, aarch64

    # Normalise arch
    if machine in ("arm64", "aarch64"):
        arch = "aarch64"
    elif machine in ("x86_64", "amd64"):
        arch = "x86_64"
    else:
        raise FileNotFoundError(
            f"No pre-built binary available for {system}/{machine}.\n"
            "Build from source: cd runtime && cargo build -p jamjet-api"
        )

    ext = ".exe" if system == "windows" else ""
    filename = f"jamjet-server-{system}-{arch}{ext}"

    # TODO: replace with real release tag when publishing to PyPI
    version = "latest"
    url = f"https://github.com/jamjet/jamjet/releases/download/{version}/{filename}"

    os.makedirs(cache_dir, exist_ok=True)
    dest = os.path.join(cache_dir, f"jamjet-server{ext}")

    console.print(f"[dim]Downloading runtime binary for {system}/{arch}...[/dim]")
    try:
        urllib.request.urlretrieve(url, dest)
    except Exception as exc:
        raise FileNotFoundError(
            f"Auto-download failed ({exc}).\n"
            "Build from source: cd runtime && cargo build -p jamjet-api\n"
            "Or set JAMJET_SERVER_PATH to the binary path."
        ) from exc

    os.chmod(dest, os.stat(dest).st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)
    console.print(f"[green]✓[/green] Runtime cached at {dest}")
    return dest


@app.command()
def dev(
    port: int = typer.Option(7700, help="Port to run the runtime on"),
    db: str | None = typer.Option(None, "--db", help="SQLite database path (default: .jamjet/runtime.db)"),
    build: bool = typer.Option(False, "--build", help="Build the runtime binary before starting"),
) -> None:
    """Start the local dev runtime (SQLite mode)."""
    import os
    import signal
    import subprocess

    if build:
        console.print("[dim]Building jamjet-server...[/dim]")
        result = subprocess.run(
            ["cargo", "build", "-p", "jamjet-api"],
            cwd=os.path.join(os.path.dirname(__file__), "..", "..", "..", "..", "runtime"),
        )
        if result.returncode != 0:
            console.print("[red]Build failed.[/red]")
            raise typer.Exit(1)

    try:
        binary = _find_server_binary()
    except FileNotFoundError as e:
        console.print(f"[red]Error:[/red] {e}")
        raise typer.Exit(1)

    db_url = f"sqlite://{db}" if db else None
    env = os.environ.copy()
    env["PORT"] = str(port)
    env["RUST_LOG"] = env.get("RUST_LOG", "info")
    if db_url:
        env["DATABASE_URL"] = db_url

    console.rule("[bold green]JamJet Dev Runtime[/bold green]")
    console.print(f"  [bold]Binary:[/bold] {binary}")
    console.print(f"  [bold]Port:[/bold]   {port}")
    console.print("  [bold]Mode:[/bold]   local (SQLite)")
    console.print(f"  [bold]API:[/bold]    http://localhost:{port}")
    console.print()
    console.print("[dim]Press Ctrl+C to stop.[/dim]")
    console.rule()

    proc = subprocess.Popen([binary], env=env)
    try:
        proc.wait()
    except KeyboardInterrupt:
        console.print("\n[yellow]Stopping runtime...[/yellow]")
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
    console.print("[green]Runtime stopped.[/green]")


# ── validate ─────────────────────────────────────────────────────────────────


@app.command()
def validate(
    path: str = typer.Argument(..., help="Path to workflow.yaml or .py"),
    output: bool = typer.Option(False, "--output", "-o", help="Print compiled IR"),
) -> None:
    """Validate a workflow definition (YAML or Python)."""
    import os

    from jamjet.workflow.ir_compiler import compile_yaml

    console.print(f"Validating: [bold]{path}[/bold]")

    if not os.path.isfile(path):
        console.print(f"[red]File not found:[/red] {path}")
        raise typer.Exit(1)

    try:
        if path.endswith(".yaml") or path.endswith(".yml"):
            with open(path) as f:
                source = f.read()
            ir = compile_yaml(source)
        elif path.endswith(".py"):
            # Import the module and call compile() on the first Workflow object found.
            import importlib.util

            spec = importlib.util.spec_from_file_location("_wf_module", path)
            mod = importlib.util.module_from_spec(spec)  # type: ignore[arg-type]
            spec.loader.exec_module(mod)  # type: ignore[union-attr]
            from jamjet.workflow.workflow import Workflow

            wf = next(
                (v for v in vars(mod).values() if isinstance(v, Workflow)),
                None,
            )
            if wf is None:
                console.print("[red]No Workflow instance found in module.[/red]")
                raise typer.Exit(1)
            ir = wf.compile()
        else:
            console.print("[red]Unsupported file type. Use .yaml or .py[/red]")
            raise typer.Exit(1)

        wf_id = ir.get("workflow_id")
        version = ir.get("version")
        console.print(f"[green]Valid.[/green] workflow_id=[bold]{wf_id}[/bold] version={version}")
        node_count = len(ir.get("nodes", {}))
        edge_count = len(ir.get("edges", []))
        console.print(f"  Nodes: {node_count}  Edges: {edge_count}")

        if output:
            console.print_json(json.dumps(ir, indent=2))

    except Exception as e:
        console.print(f"[red]Validation error:[/red] {e}")
        raise typer.Exit(1)


# ── run ───────────────────────────────────────────────────────────────────────


@app.command()
def run(
    workflow: str = typer.Argument(..., help="Workflow id or path to workflow.yaml"),
    input: str | None = typer.Option(None, "--input", "-i", help="JSON input"),
    runtime: str = typer.Option("http://localhost:7700", "--runtime", "-r"),
    follow: bool = typer.Option(True, "--follow/--no-follow", help="Follow execution progress"),
) -> None:
    """Submit and run a workflow execution."""
    input_data = json.loads(input) if input else {}

    async def _run() -> None:
        async with _client(runtime) as c:
            result = await c.start_execution(workflow_id=workflow, input=input_data)
            exec_id = result.get("execution_id", "unknown")
            console.print(f"[green]Execution started:[/green] {exec_id}")

            if not follow:
                return

            terminal = {"completed", "failed", "cancelled"}
            while True:
                await asyncio.sleep(1)
                state = await c.get_execution(exec_id)
                status = state.get("status", "unknown")
                console.print(f"  [dim]Status:[/dim] {status}")
                if status in terminal:
                    break

            if state.get("status") == "completed":
                console.print("[green]Execution completed.[/green]")
            else:
                console.print(f"[red]Execution ended:[/red] {state.get('status')}")

    asyncio.run(_run())


# ── inspect ───────────────────────────────────────────────────────────────────


@app.command()
def inspect(
    execution_id: str = typer.Argument(..., help="Execution ID (exec_...)"),
    runtime: str = typer.Option("http://localhost:7700", "--runtime", "-r"),
    tokens: bool = typer.Option(True, "--tokens/--no-tokens", help="Show per-node token/cost breakdown"),
) -> None:
    """Show the current state of a workflow execution."""

    async def _inspect() -> None:
        async with _client(runtime) as c:
            data = await c.get_execution(execution_id)
            console.print_json(json.dumps(data, indent=2))

            if not tokens:
                return

            # Fetch events and extract per-node GenAI telemetry.
            events_data = await c.get_events(execution_id)
            evts = events_data.get("events", [])
            node_rows = []
            for e in evts:
                kind = e.get("kind", {})
                if kind.get("type") != "node_completed":
                    continue
                node_id = kind.get("node_id", "—")
                model = kind.get("gen_ai_model")
                system = kind.get("gen_ai_system")
                input_tok = kind.get("input_tokens")
                output_tok = kind.get("output_tokens")
                finish = kind.get("finish_reason", "—")
                cost = kind.get("cost_usd")
                duration_ms = kind.get("duration_ms", 0)
                if model or input_tok is not None:
                    row = (node_id, system or "—", model or "—", input_tok, output_tok, finish, cost, duration_ms)
                    node_rows.append(row)

            if node_rows:
                table = Table(title="Per-Node Token & Cost Breakdown")
                table.add_column("Node", style="bold")
                table.add_column("System")
                table.add_column("Model")
                table.add_column("Input Tokens", justify="right")
                table.add_column("Output Tokens", justify="right")
                table.add_column("Finish Reason")
                table.add_column("Cost (USD)", justify="right")
                table.add_column("Duration (ms)", justify="right")
                for node_id, system, model, inp, out, finish, cost, dur in node_rows:
                    table.add_row(
                        node_id,
                        system,
                        model,
                        str(inp) if inp is not None else "—",
                        str(out) if out is not None else "—",
                        finish,
                        f"${cost:.6f}" if cost is not None else "—",
                        str(dur),
                    )
                console.print(table)

                # Totals row.
                total_in = sum(r[3] for r in node_rows if r[3] is not None)
                total_out = sum(r[4] for r in node_rows if r[4] is not None)
                total_cost = sum(r[6] for r in node_rows if r[6] is not None)
                console.print(
                    f"  [bold]Totals:[/bold] "
                    f"input={total_in} tokens, "
                    f"output={total_out} tokens" + (f", cost=${total_cost:.6f}" if total_cost else "")
                )

    asyncio.run(_inspect())


# ── events ────────────────────────────────────────────────────────────────────


@app.command()
def events(
    execution_id: str = typer.Argument(...),
    runtime: str = typer.Option("http://localhost:7700", "--runtime", "-r"),
) -> None:
    """Show the event timeline for a workflow execution."""

    async def _events() -> None:
        async with _client(runtime) as c:
            data = await c.get_events(execution_id)
            evts = data.get("events", [])
            table = Table(title=f"Events: {execution_id}")
            table.add_column("Seq", style="dim")
            table.add_column("Type")
            table.add_column("Node")
            table.add_column("Created at")
            for e in evts:
                kind = e.get("kind", {})
                table.add_row(
                    str(e.get("sequence", "")),
                    kind.get("type", ""),
                    kind.get("node_id", "—"),
                    e.get("created_at", ""),
                )
            console.print(table)

    asyncio.run(_events())


# ── agents ────────────────────────────────────────────────────────────────────


@agents_app.command("list")
def agents_list(
    runtime: str = typer.Option("http://localhost:7700", "--runtime", "-r"),
) -> None:
    """List all registered agents."""

    async def _list() -> None:
        async with _client(runtime) as c:
            data = await c.list_agents()
            console.print_json(json.dumps(data, indent=2))

    asyncio.run(_list())


@agents_app.command("inspect")
def agents_inspect(
    agent_id: str = typer.Argument(...),
    runtime: str = typer.Option("http://localhost:7700", "--runtime", "-r"),
) -> None:
    """Inspect an agent's status and Agent Card."""

    async def _inspect() -> None:
        async with _client(runtime) as c:
            data = await c.get_agent(agent_id)
            console.print_json(json.dumps(data, indent=2))

    asyncio.run(_inspect())


@agents_app.command("activate")
def agents_activate(
    agent_id: str = typer.Argument(...),
    runtime: str = typer.Option("http://localhost:7700", "--runtime", "-r"),
) -> None:
    """Activate an agent."""

    async def _activate() -> None:
        async with _client(runtime) as c:
            data = await c.activate_agent(agent_id)
            console.print(f"[green]Activated:[/green] {agent_id} → {data.get('status')}")

    asyncio.run(_activate())


# ── mcp ───────────────────────────────────────────────────────────────────────


@mcp_app.command("connect")
def mcp_connect(
    url: str = typer.Argument(..., help="MCP server URL or 'stdio:<command>'"),
) -> None:
    """Test MCP server connectivity and list available tools."""
    console.print(f"Connecting to MCP server: {url}")
    # TODO: initialize MCP client, list tools
    console.print("[yellow]MCP client not yet implemented (Phase 1 in progress)[/yellow]")


# ── a2a ───────────────────────────────────────────────────────────────────────


@a2a_app.command("discover")
def a2a_discover(
    url: str = typer.Argument(..., help="Base URL of the remote A2A agent"),
) -> None:
    """Fetch and display a remote Agent Card."""

    async def _discover() -> None:
        import httpx

        card_url = url.rstrip("/") + "/.well-known/agent.json"
        console.print(f"Fetching Agent Card from: [cyan]{card_url}[/cyan]\n")

        async with httpx.AsyncClient(timeout=10.0) as client:
            try:
                resp = await client.get(card_url)
                resp.raise_for_status()
            except httpx.HTTPStatusError as e:
                console.print(f"[red]HTTP {e.response.status_code}: {e.response.text}[/red]")
                raise typer.Exit(1)
            except httpx.RequestError as e:
                console.print(f"[red]Connection error: {e}[/red]")
                raise typer.Exit(1)

        card = resp.json()

        # Display a summary table.
        t = Table(title="Agent Card", show_header=True)
        t.add_column("Field", style="bold")
        t.add_column("Value")
        t.add_row("Name", card.get("name", "—"))
        t.add_row("Description", card.get("description", "—"))
        t.add_row("Version", card.get("version", "—"))
        t.add_row("URL", card.get("url", url))

        protocols = ", ".join(card.get("defaultInputModes", []) or card.get("input_modes", []) or ["—"])
        t.add_row("Input Modes", protocols)
        console.print(t)

        # List skills.
        skills = card.get("skills", [])
        if skills:
            s = Table(title="Skills", show_header=True)
            s.add_column("Name", style="bold")
            s.add_column("Description")
            for sk in skills:
                s.add_row(sk.get("name", "—"), sk.get("description", "—"))
            console.print(s)
        else:
            console.print("[dim]No skills declared.[/dim]")

    asyncio.run(_discover())


# ── workers ───────────────────────────────────────────────────────────────────


@app.command()
def replay(
    execution_id: str = typer.Argument(..., help="Execution ID to replay (exec_...)"),
    from_node: str | None = typer.Option(None, "--from-node", help="Resume from this node id"),
    runtime: str = typer.Option("http://localhost:7700", "--runtime", "-r"),
) -> None:
    """Replay an execution from a checkpoint (H2.7).

    Re-enqueues the specified node (or the start node) for fresh execution.
    Useful for recovering from failures without re-running the whole workflow.
    """

    async def _replay() -> None:
        async with _client(runtime) as c:
            # Fetch events to find the node to replay from.
            resp = await c._client.get(f"/executions/{execution_id}/events")
            events = resp.json().get("events", [])

            if not events:
                console.print(f"[red]No events found for {execution_id}[/red]")
                raise typer.Exit(1)

            target_node = from_node
            if not target_node:
                # Default: replay from the last failed/started node.
                for ev in reversed(events):
                    kind = ev.get("kind", {})
                    if "NodeFailed" in kind or "NodeStarted" in kind:
                        inner = kind.get("NodeFailed") or kind.get("NodeStarted") or {}
                        target_node = inner.get("node_id")
                        break
                if not target_node:
                    console.print("[red]Could not determine node to replay. Use --from-node.[/red]")
                    raise typer.Exit(1)

            console.print(f"Replaying [cyan]{execution_id}[/cyan] from node [cyan]{target_node}[/cyan]")

            # POST an external event to trigger the scheduler to re-enqueue the node.
            resp = await c._client.post(
                f"/executions/{execution_id}/external-event",
                json={"correlation_key": f"replay:{target_node}", "payload": {"replay_node": target_node}},
            )
            if resp.status_code == 200:
                console.print("[green]Replay event sent. The scheduler will re-enqueue the node.[/green]")
            else:
                console.print(f"[red]Replay failed: {resp.status_code} {resp.text}[/red]")
                raise typer.Exit(1)

    asyncio.run(_replay())


@app.command()
def workers(
    runtime: str = typer.Option("http://localhost:7700", "--runtime", "-r"),
) -> None:
    """List active workers."""

    async def _workers() -> None:
        async with _client(runtime) as c:
            r = await c._client.get("/workers")
            console.print_json(r.text)

    asyncio.run(_workers())


if __name__ == "__main__":
    app()
