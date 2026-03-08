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
import sys

import typer
from rich.console import Console
from rich.table import Table

from jamjet.client import JamjetClient

# ── Pixel art logo — J-shaped vertical jet ready to launch ────────────────────
# 7×15 grid: body IS the letter J. Nose up, swept wings, J-hook nozzle, flames.
# 0=bg  1=yellow#f5c518  2=orange#ea580c  3=red#dc2626  4=white(cockpit)
_LOGO_PIXELS = [
    "0001000",  # nose tip
    "0011100",  # nose cone
    "0014100",  # cockpit window
    "0011100",  # upper body
    "1011101",  # swept wings
    "0011100",  # body
    "0011100",  # body
    "0011100",  # lower body
    "0011100",  # engine
    "0111000",  # J nozzle — curves left
    "1110000",  # J base
    "2220000",  # exhaust orange
    "0330000",  # flame core
    "0232000",  # flame mid
    "0030000",  # flame tip
]
_LC = {
    "1": "\033[38;2;245;197;24m",
    "2": "\033[38;2;234;88;12m",
    "3": "\033[38;2;220;38;38m",
    "4": "\033[38;2;255;255;255m",
}
_LR = "\033[0m"


def _render_logo() -> str:
    lines = []
    for row in _LOGO_PIXELS:
        line = ""
        for ch in row:
            if ch == "0":
                line += "  "
            else:
                line += _LC[ch] + "██" + _LR
        lines.append(line)
    return "\n".join(lines)


def _print_logo() -> None:
    if sys.stdout.isatty():
        sys.stdout.write(_render_logo() + "\n")
        sys.stdout.flush()


# ── Version callback ──────────────────────────────────────────────────────────


def _version_callback(value: bool) -> None:
    if value:
        _print_logo()
        typer.echo("\nJamJet v0.1.0  —  agent-native workflow runtime")
        raise typer.Exit()


app = typer.Typer(
    name="jamjet",
    help="JamJet — agent-native workflow runtime CLI",
    no_args_is_help=True,
)


@app.callback()
def _main(
    version: bool = typer.Option(  # noqa: FBT001
        False,
        "--version",
        "-V",
        callback=_version_callback,
        is_eager=True,
        help="Print version and exit.",
    ),
) -> None:
    pass


agents_app = typer.Typer(help="Manage agents", no_args_is_help=True)
mcp_app = typer.Typer(help="MCP server tools", no_args_is_help=True)
a2a_app = typer.Typer(help="A2A agent tools", no_args_is_help=True)
eval_app = typer.Typer(help="Eval harness — batch scoring and CI regression", no_args_is_help=True)

app.add_typer(agents_app, name="agents")
app.add_typer(mcp_app, name="mcp")
app.add_typer(a2a_app, name="a2a")
app.add_typer(eval_app, name="eval")

console = Console()


def _client(runtime: str = "http://localhost:7700") -> JamjetClient:
    return JamjetClient(base_url=runtime)


# ── init ─────────────────────────────────────────────────────────────────────


@app.command()
def init(
    project_name: str | None = typer.Argument(
        None,
        help="Name of the new project (omit to initialise in the current directory)",
    ),
) -> None:
    """Initialise a JamJet project.

    Pass a name to create a new directory, or omit to set up in the current directory.
    """
    import os

    if project_name:
        project_dir = os.path.join(os.getcwd(), project_name)
        if os.path.exists(project_dir):
            console.print(f"[red]Error:[/red] directory '{project_name}' already exists")
            raise typer.Exit(1)
        os.makedirs(project_dir)
    else:
        project_name = os.path.basename(os.getcwd())
        project_dir = os.getcwd()

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

    console.print(f"[green]✓[/green] Initialised [bold]{project_name}[/bold]")
    console.print("  [dim]workflow.yaml[/dim]   ← your workflow (edit this)")
    console.print("  [dim]README.md[/dim]")
    console.print()
    console.print("[bold]Next steps:[/bold]")
    if project_name != os.path.basename(os.getcwd()):
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

    # Resolve the latest release tag from GitHub API.
    api_url = "https://api.github.com/repos/jamjet-labs/jamjet/releases/latest"
    try:
        gh_headers = {
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        }
        req = urllib.request.Request(api_url, headers=gh_headers)  # noqa: S310
        with urllib.request.urlopen(req, timeout=10) as resp:  # noqa: S310
            import json as _json

            release = _json.loads(resp.read())
        tag = release["tag_name"]
    except Exception as exc:
        raise FileNotFoundError(
            f"Could not fetch latest release info from GitHub ({exc}).\n"
            "Build from source: cd runtime && cargo build -p jamjet-api\n"
            "Or set JAMJET_SERVER_PATH to the binary path."
        ) from exc

    url = f"https://github.com/jamjet-labs/jamjet/releases/download/{tag}/{filename}"
    os.makedirs(cache_dir, exist_ok=True)
    dest = os.path.join(cache_dir, f"jamjet-server{ext}")
    dest_tmp = dest + ".tmp"

    console.print(f"[dim]Downloading jamjet-server {tag} for {system}/{arch}...[/dim]")

    # Stream download with a Rich progress bar.
    from rich.progress import BarColumn, DownloadColumn, Progress, TextColumn, TransferSpeedColumn

    try:
        with urllib.request.urlopen(url, timeout=60) as response:  # noqa: S310
            total = int(response.headers.get("Content-Length", 0)) or None
            with Progress(
                TextColumn("[bold blue]{task.description}"),
                BarColumn(),
                DownloadColumn(),
                TransferSpeedColumn(),
                console=console,
                transient=True,
            ) as progress:
                task = progress.add_task(filename, total=total)
                with open(dest_tmp, "wb") as out:
                    while True:
                        chunk = response.read(65536)
                        if not chunk:
                            break
                        out.write(chunk)
                        progress.advance(task, len(chunk))
    except Exception as exc:
        if os.path.exists(dest_tmp):
            os.remove(dest_tmp)
        raise FileNotFoundError(
            f"Auto-download failed ({exc}).\n"
            "Build from source: cd runtime && cargo build -p jamjet-api\n"
            "Or set JAMJET_SERVER_PATH to the binary path."
        ) from exc

    os.replace(dest_tmp, dest)
    os.chmod(dest, os.stat(dest).st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)
    console.print(f"[green]✓[/green] Runtime binary cached at {dest}")
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

    _print_logo()
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
    stream: bool = typer.Option(False, "--stream", help="Stream structured output chunks progressively"),
) -> None:
    """Submit and run a workflow execution."""
    input_data = json.loads(input) if input else {}

    async def _run() -> None:
        async with _client(runtime) as c:
            result = await c.start_execution(workflow_id=workflow, input=input_data)
            exec_id = result.get("execution_id", "unknown")
            console.print(f"[green]Execution started:[/green] {exec_id}")

            if stream:
                await _stream_execution(c, exec_id)
                return

            if not follow:
                return

            terminal = {"completed", "failed", "cancelled", "limit_exceeded"}
            state: dict = {}
            while True:
                await asyncio.sleep(1)
                state = await c.get_execution(exec_id)
                status = state.get("status", "unknown")
                console.print(f"  [dim]Status:[/dim] {status}")
                if status in terminal:
                    break

            final_status = state.get("status")
            if final_status == "completed":
                console.print("[green]Execution completed.[/green]")
            elif final_status == "limit_exceeded":
                console.print("[yellow]Execution halted: strategy limit exceeded.[/yellow]")
            else:
                console.print(f"[red]Execution ended:[/red] {final_status}")

    async def _stream_execution(c: JamjetClient, exec_id: str) -> None:
        """
        Poll events and render structured stream chunks progressively (I2.7).

        Polls ``GET /executions/{id}/events`` every 500ms and renders new
        events as typed stream chunks with Rich formatting.
        """
        from rich.live import Live

        terminal = {"completed", "failed", "cancelled", "limit_exceeded"}
        seen_seq: int = -1
        console.print(f"[dim]Streaming chunks for[/dim] {exec_id}\n")

        with Live(console=console, refresh_per_second=4) as live:
            while True:
                await asyncio.sleep(0.5)

                state = await c.get_execution(exec_id)
                status = state.get("status", "unknown")

                events_data = await c.get_events(exec_id)
                new_events = [e for e in events_data.get("events", []) if e.get("sequence", 0) > seen_seq]

                for evt in sorted(new_events, key=lambda e: e.get("sequence", 0)):
                    seq = evt.get("sequence", 0)
                    seen_seq = max(seen_seq, seq)
                    kind = evt.get("kind", {})
                    etype = kind.get("type", "")

                    chunk = _event_to_stream_chunk(etype, kind)
                    if chunk:
                        live.console.print(chunk)

                if status in terminal:
                    live.console.print()
                    if status == "completed":
                        live.console.print("[green]✓ Stream complete[/green]")
                    elif status == "limit_exceeded":
                        live.console.print("[yellow]⚠ Strategy limit exceeded[/yellow]")
                    else:
                        live.console.print(f"[red]✗ {status}[/red]")
                    break

    def _event_to_stream_chunk(etype: str, kind: dict) -> str | None:
        """Map an event kind to a human-readable stream chunk line."""

        if etype == "node_started":
            node_id = kind.get("node_id", "?")
            return f"[dim cyan]→ node_started[/dim cyan]  [cyan]{node_id}[/cyan]"
        if etype == "node_completed":
            node_id = kind.get("node_id", "?")
            dur = kind.get("duration_ms", 0)
            model = kind.get("gen_ai_model")
            tokens = ""
            if kind.get("input_tokens") or kind.get("output_tokens"):
                inp = kind.get("input_tokens", 0)
                out = kind.get("output_tokens", 0)
                tokens = f" [dim]({inp}→{out} tokens)[/dim]"
            model_tag = f" [dim]{model}[/dim]" if model else ""
            return f"[green]✓ node_completed[/green]  [bold]{node_id}[/bold]{model_tag}  [dim]{dur}ms{tokens}[/dim]"
        if etype == "node_failed":
            node_id = kind.get("node_id", "?")
            err = kind.get("error", "")
            return f"[red]✗ node_failed[/red]    [bold]{node_id}[/bold]  [dim red]{err}[/dim red]"
        if etype == "strategy_started":
            strategy = kind.get("strategy", "?")
            return f"[magenta]⚡ strategy_started[/magenta]  [bold]{strategy}[/bold]"
        if etype == "plan_generated":
            steps = kind.get("steps", [])
            return f"[blue]📋 plan_generated[/blue]  {len(steps)} steps"
        if etype == "iteration_started":
            it = kind.get("iteration", "?")
            return f"[blue]↻ iteration_started[/blue]  #{it}"
        if etype == "iteration_completed":
            it = kind.get("iteration", "?")
            cost = kind.get("cost_delta_usd")
            cost_str = f"  [dim]cost=${cost:.4f}[/dim]" if cost else ""
            return f"[blue]✓ iteration_done[/blue]   #{it}{cost_str}"
        if etype == "critic_verdict":
            score = kind.get("score", 0.0)
            passed = kind.get("passed", False)
            icon = "✓" if passed else "✗"
            color = "green" if passed else "yellow"
            return f"[{color}]{icon} critic_verdict[/{color}]  score={score:.2f}"
        if etype == "strategy_limit_hit":
            limit_type = kind.get("limit_type", "?")
            limit_val = kind.get("limit_value", "?")
            actual = kind.get("actual_value", "?")
            return f"[yellow]⚠ limit_hit[/yellow]  {limit_type}: {actual} ≥ {limit_val}"
        if etype == "strategy_completed":
            iters = kind.get("iterations", "?")
            cost = kind.get("total_cost_usd")
            cost_str = f"  total=${cost:.4f}" if cost else ""
            return f"[green]✓ strategy_done[/green]  {iters} iterations{cost_str}"
        return None

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
    url: str = typer.Argument(..., help="MCP server URL (http/https) or 'stdio:<command>'"),
    timeout: float = typer.Option(10.0, "--timeout", help="Connection timeout in seconds"),
) -> None:
    """Test MCP server connectivity and list available tools."""

    async def _connect() -> None:
        import shlex

        import httpx

        console.print(f"Connecting to MCP server: [cyan]{url}[/cyan]\n")

        if url.startswith("stdio:"):
            # stdio:<command> — spawn subprocess, speak JSON-RPC over stdin/stdout
            command = url[len("stdio:") :].strip()
            args = shlex.split(command)
            proc = await asyncio.create_subprocess_exec(
                *args,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )

            async def _rpc(method: str, params: dict | None = None) -> dict:
                import json as _json

                msg = _json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}}) + "\n"
                assert proc.stdin and proc.stdout  # noqa: S101
                proc.stdin.write(msg.encode())
                await proc.stdin.drain()
                line = await asyncio.wait_for(proc.stdout.readline(), timeout=timeout)
                return _json.loads(line)["result"]

            try:
                init_params = {
                    "protocolVersion": "2024-11-05",
                    "clientInfo": {"name": "jamjet-cli", "version": "0.1.0"},
                    "capabilities": {},
                }
                info = await _rpc("initialize", init_params)
                tools_resp = await _rpc("tools/list")
            finally:
                proc.terminate()
                await proc.wait()
        else:
            # HTTP + SSE — POST JSON-RPC to the MCP endpoint
            async with httpx.AsyncClient(timeout=timeout) as client:

                async def _rpc(method: str, params: dict | None = None) -> dict:  # type: ignore[misc]
                    endpoint = url.rstrip("/") + "/mcp"
                    payload = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params or {}}
                    try:
                        resp = await client.post(endpoint, json=payload)
                        resp.raise_for_status()
                    except httpx.HTTPStatusError as e:
                        console.print(f"[red]HTTP {e.response.status_code}: {e.response.text}[/red]")
                        raise typer.Exit(1)
                    except httpx.RequestError as e:
                        console.print(f"[red]Connection error: {e}[/red]")
                        raise typer.Exit(1)
                    body = resp.json()
                    if "error" in body:
                        console.print(f"[red]MCP error: {body['error']}[/red]")
                        raise typer.Exit(1)
                    return body["result"]

                init_params = {
                    "protocolVersion": "2024-11-05",
                    "clientInfo": {"name": "jamjet-cli", "version": "0.1.0"},
                    "capabilities": {},
                }
                info = await _rpc("initialize", init_params)
                tools_resp = await _rpc("tools/list")

        # Server info
        server_info = info.get("serverInfo", {})
        name = server_info.get("name", "MCP server")
        ver = server_info.get("version", "?")
        console.print(f"[green]✓[/green] Connected to [bold]{name}[/bold] v{ver}")
        console.print(f"  Protocol: {info.get('protocolVersion', '?')}\n")

        # Tools table
        tools = tools_resp.get("tools", [])
        if not tools:
            console.print("[dim]No tools exposed by this server.[/dim]")
            return

        t = Table(title=f"{len(tools)} tool(s) available", show_header=True)
        t.add_column("Name", style="bold")
        t.add_column("Description")
        t.add_column("Input schema")
        for tool in tools:
            schema = tool.get("inputSchema", {})
            props = ", ".join(schema.get("properties", {}).keys()) if schema else "—"
            t.add_row(tool.get("name", "?"), tool.get("description", "—"), props or "—")
        console.print(t)

    asyncio.run(_connect())


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


# ── eval ──────────────────────────────────────────────────────────────────────


@eval_app.command("run")
def eval_run(
    dataset: str = typer.Argument(..., help="Path to .jsonl or .json dataset file"),
    workflow: str = typer.Option(..., "--workflow", "-w", help="Workflow ID to evaluate"),
    runtime: str = typer.Option("http://localhost:7700", "--runtime", "-r"),
    rubric: str | None = typer.Option(None, "--rubric", help="LLM-judge rubric (enables LLM scoring)"),
    model: str = typer.Option("claude-haiku-4-5-20251001", "--model", help="Model for LLM judge"),
    min_score: int = typer.Option(3, "--min-score", help="Minimum passing score for LLM judge (1-5)"),
    assertions: list[str] = typer.Option([], "--assert", "-a", help="Python assertion expression (repeatable)"),
    latency_ms: float | None = typer.Option(None, "--latency-ms", help="Maximum allowed latency in ms"),
    cost_usd: float | None = typer.Option(None, "--cost-usd", help="Maximum allowed cost in USD"),
    concurrency: int = typer.Option(4, "--concurrency", "-c", help="Parallel executions"),
    output_json: str | None = typer.Option(None, "--output", "-o", help="Write full results to JSON file"),
    fail_below: float | None = typer.Option(None, "--fail-below", help="Exit 1 if pass rate below this % (e.g. 80)"),
    timeout_s: float = typer.Option(120.0, "--timeout", help="Per-row execution timeout in seconds"),
) -> None:
    """Run batch evaluation of a workflow against a dataset.

    \b
    Examples:
      jamjet eval run qa_pairs.jsonl --workflow my_workflow --rubric "Rate accuracy 1-5"
      jamjet eval run dataset.jsonl -w summarizer --assert "'summary' in output" --latency-ms 5000
      jamjet eval run evals.jsonl -w rag --fail-below 80 --output results.json
    """
    from jamjet.eval.dataset import EvalDataset
    from jamjet.eval.runner import EvalRunner
    from jamjet.eval.scorers import AssertionScorer, CostScorer, LatencyScorer, LlmJudgeScorer

    scorers = []

    if rubric:
        scorers.append(LlmJudgeScorer(rubric=rubric, model=model, min_score=min_score))

    if assertions:
        scorers.append(AssertionScorer(checks=list(assertions)))

    if latency_ms is not None:
        scorers.append(LatencyScorer(threshold_ms=latency_ms))

    if cost_usd is not None:
        scorers.append(CostScorer(threshold_usd=cost_usd))

    if not scorers:
        console.print(
            "[yellow]Warning:[/yellow] No scorers configured. Add --rubric, --assert, --latency-ms, or --cost-usd."
        )

    try:
        ds = EvalDataset.from_file(dataset)
    except (FileNotFoundError, ValueError) as e:
        console.print(f"[red]Error loading dataset:[/red] {e}")
        raise typer.Exit(1)

    console.print(f"Running eval: [bold]{len(ds)}[/bold] rows × [bold]{workflow}[/bold] with {len(scorers)} scorer(s)")

    runner = EvalRunner(
        workflow_id=workflow,
        scorers=scorers,
        runtime=runtime,
        concurrency=concurrency,
        timeout_s=timeout_s,
    )

    results = asyncio.run(runner.run(ds))
    EvalRunner.print_summary(results, console=console)

    if output_json:
        import dataclasses

        out_data = [
            {
                "row_id": r.row_id,
                "passed": r.passed,
                "overall_score": r.overall_score,
                "duration_ms": r.duration_ms,
                "cost_usd": r.cost_usd,
                "error": r.error,
                "scorers": [dataclasses.asdict(s) for s in r.scorers],
                "output": r.output,
                "input": r.input,
                "expected": r.expected,
            }
            for r in results
        ]
        with open(output_json, "w") as f:
            json.dump(out_data, f, indent=2, default=str)
        console.print(f"[dim]Results written to {output_json}[/dim]")

    if fail_below is not None:
        total = len(results)
        passed = sum(1 for r in results if r.passed)
        pass_rate = passed / total * 100 if total else 0
        if pass_rate < fail_below:
            console.print(f"[red]FAIL:[/red] pass rate {pass_rate:.1f}% < {fail_below:.1f}% threshold")
            raise typer.Exit(1)


if __name__ == "__main__":
    app()
