"""
JamJet CLI — the main entry point for all `jamjet` commands.

Commands:
  jamjet init           Create a new project from a template
  jamjet dev            Start local dev runtime (SQLite)
  jamjet run            Submit and run a workflow
  jamjet validate       Validate a workflow definition
  jamjet inspect        Show execution state and history
  jamjet events         Show event timeline for an execution
  jamjet replay         Replay an execution (time-travel debugging)
  jamjet fork           Fork an execution with modified input (ablation studies)
  jamjet agents         Manage agents (list, inspect, activate, deactivate)
  jamjet mcp connect    Test MCP server connectivity
  jamjet a2a discover   Fetch and display a remote Agent Card
  jamjet workers        List active workers
  jamjet eval run       Run batch evaluation of a workflow
  jamjet eval export    Export eval results as LaTeX, CSV, or JSON
  jamjet eval compare   Compare two conditions with statistical tests
"""

from __future__ import annotations

import asyncio
import json
import sys
from collections.abc import Mapping
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from jamjet.eval.grid import ComparisonResult

import typer
from rich.console import Console
from rich.table import Table

from jamjet.client import JamjetClient

# ── Logo — block letter J  (0=bg  1=yellow#f5c518  2=orange#ea580c) ──────────
# 6×7 grid: clean J — top bar + vertical stem + orange hook at base.
_LOGO_PIXELS = [
    "011111",  # top bar
    "000001",  # stem
    "000001",  # stem
    "000001",  # stem
    "000001",  # stem
    "020001",  # foot — orange accent + stem
    "002220",  # base curve — orange
]
_LC = {
    "1": "\033[38;2;245;197;24m",  # yellow/gold
    "2": "\033[38;2;234;88;12m",  # orange
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
        typer.echo("\nJamJet v0.1.1  —  agent-native workflow runtime")
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
    template: str = typer.Option(
        "hello-agent",
        "--template",
        "-t",
        help="Starter template to use. Run `jamjet init --list-templates` to see all options.",
    ),
    list_templates: bool = typer.Option(  # noqa: FBT001
        False,
        "--list-templates",
        is_eager=True,
        help="List available templates and exit.",
    ),
) -> None:
    """Initialise a JamJet project from a template.

    Pass a name to create a new directory, or omit to set up in the current directory.

    Available templates: hello-agent (default), research-agent, code-reviewer, approval-workflow
    """
    import os

    from jamjet.templates import AVAILABLE_TEMPLATES, render_template

    if list_templates:
        console.print("[bold]Available templates:[/bold]")
        for t in AVAILABLE_TEMPLATES:
            marker = "  [green]●[/green]" if t == "hello-agent" else "  [dim]●[/dim]"
            console.print(f"{marker} {t}")
        raise typer.Exit()

    if template not in AVAILABLE_TEMPLATES:
        console.print(
            f"[red]Error:[/red] unknown template '{template}'. "
            f"Run [bold]jamjet init --list-templates[/bold] to see options."
        )
        raise typer.Exit(1)

    if project_name:
        project_dir = os.path.join(os.getcwd(), project_name)
        if os.path.exists(project_dir):
            console.print(f"[red]Error:[/red] directory '{project_name}' already exists")
            raise typer.Exit(1)
        os.makedirs(project_dir)
    else:
        project_name = os.path.basename(os.getcwd())
        project_dir = os.getcwd()

    files = render_template(template, project_name)
    written: list[str] = []
    for rel_path, content in files.items():
        abs_path = os.path.join(project_dir, rel_path)
        os.makedirs(os.path.dirname(abs_path), exist_ok=True)
        with open(abs_path, "w") as fh:
            fh.write(content)
        written.append(rel_path)

    console.print(f"[green]✓[/green] Initialised [bold]{project_name}[/bold] from template [bold]{template}[/bold]")
    for rel_path in written:
        console.print(f"  [dim]{rel_path}[/dim]")
    console.print()
    console.print("[bold]Next steps:[/bold]")
    if project_name != os.path.basename(os.getcwd()):
        console.print(f"  cd {project_name}")
    console.print("  jamjet dev               [dim]# start the runtime[/dim]")
    console.print("  jamjet run workflow.yaml  [dim]# run in another terminal[/dim]")


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

            # ── Strategy section ─────────────────────────────────────────
            _print_strategy_section(console, data, evts)

    asyncio.run(_inspect())


def _print_strategy_section(console: Console, execution: dict, events: list) -> None:
    """Print strategy-aware section if the execution used a strategy.

    Detects strategy metadata from execution labels or events, then shows:
    - Strategy name
    - Iteration count
    - Plan steps (plan-and-execute)
    - Critic/reflection verdicts
    - Cost per iteration
    """
    from rich.panel import Panel

    # ── Detect strategy ──────────────────────────────────────────────────
    labels = execution.get("labels", {})
    strategy_name = labels.get("jamjet.strategy")

    # Fallback: check events for strategy_started or strategy labels
    if not strategy_name:
        for e in events:
            kind = e.get("kind", {})
            elabels = kind.get("labels", {})
            if "jamjet.strategy" in elabels:
                strategy_name = elabels["jamjet.strategy"]
                break
            if kind.get("type") == "strategy_started":
                strategy_name = kind.get("strategy_name", "unknown")
                break

    if not strategy_name:
        return

    # ── Iteration count ──────────────────────────────────────────────────
    iteration_count = 0
    for e in events:
        kind = e.get("kind", {})
        elabels = kind.get("labels", {})
        if kind.get("type") == "iteration_started":
            iteration_count += 1
        elif elabels.get("jamjet.strategy.event") == "iteration_started":
            iteration_count += 1

    # ── Plan steps (plan-and-execute) ────────────────────────────────────
    plan_steps: list[str] | None = None
    if strategy_name == "plan-and-execute":
        for e in events:
            kind = e.get("kind", {})
            elabels = kind.get("labels", {})
            if kind.get("type") == "plan_generated" or elabels.get("jamjet.strategy.event") == "plan_generated":
                output = kind.get("output", {})
                if isinstance(output, dict):
                    plan_steps = output.get("steps")
                elif isinstance(output, str):
                    try:
                        parsed = json.loads(output)
                        plan_steps = parsed.get("steps")
                    except (json.JSONDecodeError, AttributeError):
                        pass
                break

    # ── Critic / reflection verdicts ─────────────────────────────────────
    verdicts: list[dict] = []
    for e in events:
        kind = e.get("kind", {})
        elabels = kind.get("labels", {})
        if kind.get("type") == "critic_verdict" or elabels.get("jamjet.strategy.event") == "critic_verdict":
            output = kind.get("output", {})
            if isinstance(output, str):
                try:
                    output = json.loads(output)
                except (json.JSONDecodeError, AttributeError):
                    output = {}
            score = output.get("score")
            passed = output.get("passed")
            iteration = elabels.get("jamjet.strategy.iteration", kind.get("iteration", "?"))
            verdicts.append({"iteration": iteration, "score": score, "passed": passed})

    # ── Cost per iteration ───────────────────────────────────────────────
    iteration_costs: list[dict] = []
    current_iteration: int | None = None
    current_cost: float = 0.0
    for e in events:
        kind = e.get("kind", {})
        elabels = kind.get("labels", {})
        if kind.get("type") == "iteration_started" or elabels.get("jamjet.strategy.event") == "iteration_started":
            if current_iteration is not None:
                iteration_costs.append({"iteration": current_iteration, "cost_usd": current_cost})
            iter_str = elabels.get("jamjet.strategy.iteration", "")
            current_iteration = int(iter_str) if iter_str.isdigit() else (len(iteration_costs))
            current_cost = 0.0
        cost = kind.get("cost_usd")
        if cost is not None and current_iteration is not None:
            current_cost += cost
    if current_iteration is not None:
        iteration_costs.append({"iteration": current_iteration, "cost_usd": current_cost})

    # ── Render ───────────────────────────────────────────────────────────
    lines: list[str] = []
    lines.append(f"[bold]Strategy:[/bold] {strategy_name}")
    lines.append(f"[bold]Iterations:[/bold] {iteration_count}")

    if plan_steps:
        lines.append("")
        lines.append("[bold]Plan Steps:[/bold]")
        for idx, step in enumerate(plan_steps, 1):
            lines.append(f"  {idx}. {step}")

    if verdicts:
        lines.append("")
        verdict_table = Table(title="Critic Verdicts", show_header=True, expand=False)
        verdict_table.add_column("Iteration", justify="center")
        verdict_table.add_column("Score", justify="right")
        verdict_table.add_column("Passed", justify="center")
        for v in verdicts:
            score_str = f"{v['score']:.2f}" if v["score"] is not None else "—"
            passed_str = "[green]yes[/green]" if v["passed"] else "[red]no[/red]" if v["passed"] is not None else "—"
            verdict_table.add_row(str(v["iteration"]), score_str, passed_str)
        # Print panel header lines first, then the table
        header_text = "\n".join(lines)
        lines = []  # reset — we'll print header + table separately
        console.print(Panel(header_text, title="Strategy", border_style="blue"))
        console.print(verdict_table)
    else:
        console.print(Panel("\n".join(lines), title="Strategy", border_style="blue"))

    if iteration_costs:
        cost_table = Table(title="Cost per Iteration", show_header=True, expand=False)
        cost_table.add_column("Iteration", justify="center")
        cost_table.add_column("Cost (USD)", justify="right")
        for ic in iteration_costs:
            cost_table.add_row(str(ic["iteration"]), f"${ic['cost_usd']:.6f}")
        console.print(cost_table)


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


@agents_app.command("discover")
def agents_discover(
    url: str = typer.Argument(..., help="Base URL of the remote agent (e.g. https://agent.example.com)"),
    register: bool = typer.Option(False, "--register", "-r", help="Register the agent after discovery"),
    runtime: str = typer.Option("http://localhost:7700", "--runtime"),
) -> None:
    """Fetch and display an Agent Card from a remote A2A endpoint."""
    import httpx
    from rich.table import Table

    well_known = url.rstrip("/") + "/.well-known/agent.json"

    async def _discover() -> None:
        try:
            async with httpx.AsyncClient(timeout=10) as http:
                resp = await http.get(well_known)
                resp.raise_for_status()
                card = resp.json()
        except Exception as exc:
            console.print(f"[red]Failed to fetch Agent Card:[/red] {exc}")
            raise typer.Exit(1) from exc

        t = Table(title=f"Agent Card — {url}", show_header=False, box=None)
        t.add_column("Field", style="bold cyan", width=18)
        t.add_column("Value")
        for key in ("id", "name", "version", "description", "url"):
            if key in card:
                t.add_row(key, str(card[key]))
        console.print(t)

        skills = card.get("capabilities", {}).get("skills", [])
        if skills:
            console.print("\n[bold]Skills:[/bold]")
            for sk in skills:
                console.print(f"  · [yellow]{sk.get('id', '?')}[/yellow] — {sk.get('description', '')}")

        if register:
            async with _client(runtime) as c:
                result = await c.post("/agents/discover", json={"url": url})
                console.print(f"\n[green]Registered:[/green] {result.get('id', card.get('id'))}")

    asyncio.run(_discover())


@agents_app.command("connect")
def agents_connect(
    url: str = typer.Argument(..., help="Base URL of the remote agent to register"),
    runtime: str = typer.Option("http://localhost:7700", "--runtime", "-r"),
) -> None:
    """Discover and register a remote agent with the local registry."""

    async def _connect() -> None:
        async with _client(runtime) as c:
            result = await c.post("/agents/discover", json={"url": url})
            agent_id = result.get("id", "?")
            name = result.get("name", agent_id)
            console.print(f"[green]Connected:[/green] [bold]{name}[/bold] ({agent_id})")
            console.print(f"  URL: {url}")
            skills = result.get("capabilities", {}).get("skills", [])
            if skills:
                console.print(f"  Skills: {', '.join(s.get('id', '?') for s in skills)}")

    asyncio.run(_connect())


@agents_app.command("trace")
def agents_trace(
    agent_id: str = typer.Argument(..., help="Agent ID to show communication trace for"),
    limit: int = typer.Option(20, "--limit", "-n", help="Number of recent events to show"),
    runtime: str = typer.Option("http://localhost:7700", "--runtime", "-r"),
) -> None:
    """Show recent A2A communication events for an agent."""
    from rich.table import Table

    async def _trace() -> None:
        async with _client(runtime) as c:
            data = await c.get(f"/agents/{agent_id}/trace?limit={limit}")
            events = data.get("events", [])

        if not events:
            console.print(f"[dim]No trace events found for agent {agent_id}[/dim]")
            return

        t = Table(title=f"Agent trace — {agent_id}", show_lines=False)
        t.add_column("Time", style="dim", width=20)
        t.add_column("Direction", width=10)
        t.add_column("Type", style="cyan", width=18)
        t.add_column("Details")

        for evt in events:
            direction = "[green]→ OUT[/green]" if evt.get("direction") == "outbound" else "[blue]← IN[/blue]"
            t.add_row(
                evt.get("timestamp", "")[:19],
                direction,
                evt.get("event_type", ""),
                evt.get("summary", ""),
            )

        console.print(t)

    asyncio.run(_trace())


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
                    "clientInfo": {"name": "jamjet-cli", "version": "0.1.1"},
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

                async def _rpc(method: str, params: dict | None = None) -> dict:
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
                    "clientInfo": {"name": "jamjet-cli", "version": "0.1.1"},
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
    execution_id: str = typer.Argument(..., help="Execution ID to replay"),
    runtime: str = typer.Option("http://localhost:7700", "--runtime", "-r"),
    from_node: str | None = typer.Option(None, "--from-node", help="Replay from a specific node (coming soon)"),
    override_input: str | None = typer.Option(None, "--override-input", help="JSON to merge into the original input"),
) -> None:
    """Replay an execution — time-travel debugging and reproducibility.

    Fetches the original execution's events, extracts the initial input and
    workflow ID from the WorkflowStarted event, and creates a brand-new
    execution with the same parameters.  Use --override-input to tweak
    specific input fields for ablation studies.

    \b
    Examples:
      jamjet replay exec_abc123
      jamjet replay exec_abc123 --override-input '{"model": "gpt-4o"}'
    """

    async def _replay() -> None:
        async with _client(runtime) as c:
            # 1. Fetch events for the original execution.
            events_data = await c.get_events(execution_id)
            evts = events_data.get("events", [])

            if not evts:
                console.print(f"[red]No events found for {execution_id}[/red]")
                raise typer.Exit(1)

            # 2. Find the WorkflowStarted event to extract workflow_id and initial_input.
            workflow_id: str | None = None
            workflow_version: str | None = None
            initial_input: dict = {}

            for evt in evts:
                kind = evt.get("kind", {})
                etype = kind.get("type", "")
                if etype in ("WorkflowStarted", "workflow_started"):
                    workflow_id = kind.get("workflow_id")
                    workflow_version = kind.get("workflow_version")
                    initial_input = kind.get("initial_input") or kind.get("input") or {}
                    break

            if not workflow_id:
                console.print("[red]Could not find WorkflowStarted event. Cannot replay.[/red]")
                raise typer.Exit(1)

            # 3. Handle --from-node (future feature).
            if from_node:
                console.print(
                    "[yellow]Note:[/yellow] --from-node checkpoint-level replay is coming soon. "
                    "Replaying from start with full input."
                )

            # 4. Merge override input if provided.
            replay_input = dict(initial_input)
            if override_input:
                try:
                    overrides = json.loads(override_input)
                except json.JSONDecodeError as e:
                    console.print(f"[red]Invalid JSON in --override-input:[/red] {e}")
                    raise typer.Exit(1)
                replay_input.update(overrides)

            version_tag = f" (v{workflow_version})" if workflow_version else ""
            console.print(f"Replaying execution [cyan]{execution_id}[/cyan]...")

            # 5. Start a new execution.
            result = await c.start_execution(
                workflow_id=workflow_id,
                input=replay_input,
                workflow_version=workflow_version,
            )
            new_id = result.get("execution_id", "unknown")

            # Truncate input display for readability.
            input_str = json.dumps(replay_input)
            if len(input_str) > 80:
                input_str = input_str[:77] + "..."

            console.print(f"  [bold]Workflow:[/bold]   {workflow_id}{version_tag}")
            console.print(f"  [bold]Original:[/bold]   {execution_id}")
            console.print(f"  [bold]New:[/bold]        {new_id}")
            console.print(f"  [bold]Input:[/bold]      {input_str}")
            console.print()
            console.print(f"Track with: [bold]jamjet inspect {new_id}[/bold]")

    asyncio.run(_replay())


@app.command()
def fork(
    execution_id: str = typer.Argument(..., help="Execution ID to fork"),
    runtime: str = typer.Option("http://localhost:7700", "--runtime", "-r"),
    from_node: str | None = typer.Option(None, "--from-node", help="Fork from a specific node (coming soon)"),
    override_input: str | None = typer.Option(None, "--override-input", help="JSON to merge into the original input"),
) -> None:
    """Fork an execution with modified input — for ablation studies.

    Creates a new execution using the same workflow and input as the
    original, with --override-input fields merged in.  This is useful
    for testing how different parameters affect the outcome.

    Full checkpoint-level forking (resuming from a specific node's state)
    requires Rust runtime support and is coming in a future release.
    Currently, fork replays from start with the modified input.

    \b
    Examples:
      jamjet fork exec_abc123 --override-input '{"model": "gemini-2.0-flash"}'
      jamjet fork exec_abc123 --from-node think --override-input '{"temperature": 0.2}'
    """
    if not override_input:
        console.print(
            "[red]Error:[/red] --override-input is required for fork. Use [bold]jamjet replay[/bold] for exact replays."
        )
        raise typer.Exit(1)

    async def _fork() -> None:
        async with _client(runtime) as c:
            # 1. Fetch events for the original execution.
            events_data = await c.get_events(execution_id)
            evts = events_data.get("events", [])

            if not evts:
                console.print(f"[red]No events found for {execution_id}[/red]")
                raise typer.Exit(1)

            # 2. Find the WorkflowStarted event.
            workflow_id: str | None = None
            workflow_version: str | None = None
            initial_input: dict = {}

            for evt in evts:
                kind = evt.get("kind", {})
                etype = kind.get("type", "")
                if etype in ("WorkflowStarted", "workflow_started"):
                    workflow_id = kind.get("workflow_id")
                    workflow_version = kind.get("workflow_version")
                    initial_input = kind.get("initial_input") or kind.get("input") or {}
                    break

            if not workflow_id:
                console.print("[red]Could not find WorkflowStarted event. Cannot fork.[/red]")
                raise typer.Exit(1)

            # 3. Handle --from-node (future feature).
            if from_node:
                console.print(
                    f"[yellow]Note:[/yellow] Checkpoint-level forking from node [bold]{from_node}[/bold] "
                    f"requires Rust runtime support (coming soon). "
                    f"Forking from start with modified input."
                )

            # 4. Merge override input.
            fork_input = dict(initial_input)
            try:
                overrides = json.loads(override_input)
            except json.JSONDecodeError as e:
                console.print(f"[red]Invalid JSON in --override-input:[/red] {e}")
                raise typer.Exit(1)
            fork_input.update(overrides)

            version_tag = f" (v{workflow_version})" if workflow_version else ""
            console.print(f"Forking execution [cyan]{execution_id}[/cyan]...")

            # 5. Start a new execution with modified input.
            result = await c.start_execution(
                workflow_id=workflow_id,
                input=fork_input,
                workflow_version=workflow_version,
            )
            new_id = result.get("execution_id", "unknown")

            # Show what changed.
            input_str = json.dumps(fork_input)
            if len(input_str) > 80:
                input_str = input_str[:77] + "..."
            overrides_str = json.dumps(overrides)
            if len(overrides_str) > 80:
                overrides_str = overrides_str[:77] + "..."

            console.print(f"  [bold]Workflow:[/bold]   {workflow_id}{version_tag}")
            console.print(f"  [bold]Original:[/bold]   {execution_id}")
            console.print(f"  [bold]Forked:[/bold]     {new_id}")
            console.print(f"  [bold]Overrides:[/bold]  {overrides_str}")
            console.print(f"  [bold]Input:[/bold]      {input_str}")
            console.print()
            console.print(f"Track with: [bold]jamjet inspect {new_id}[/bold]")

    asyncio.run(_fork())


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
    from jamjet.eval.scorers import AssertionScorer, BaseScorer, CostScorer, LatencyScorer, LlmJudgeScorer

    scorers: list[BaseScorer] = []

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


@eval_app.command("export")
def eval_export(
    results_json: str = typer.Argument(..., help="Path to a JSON results file (from `jamjet eval run --output`)"),
    fmt: str = typer.Option(..., "--format", "-f", help="Output format: latex, csv, or json"),
    output: str | None = typer.Option(None, "--output", "-o", help="Output file path (default: stdout)"),
    caption: str = typer.Option("Evaluation Results", "--caption", help="Caption for LaTeX table"),
) -> None:
    """Export eval results as LaTeX, CSV, or JSON.

    \b
    Examples:
      jamjet eval export results.json --format latex --output table.tex
      jamjet eval export results.json -f csv -o results.csv
      jamjet eval export results.json -f json
      jamjet eval export results.json -f latex --caption "Table 1: QA Benchmark"
    """
    from pathlib import Path

    fmt_lower = fmt.lower()
    if fmt_lower not in ("latex", "csv", "json"):
        console.print(f"[red]Error:[/red] unsupported format '{fmt}'. Use: latex, csv, or json")
        raise typer.Exit(1)

    try:
        with open(results_json) as f:
            data = json.load(f)
    except FileNotFoundError:
        console.print(f"[red]Error:[/red] file not found: {results_json}")
        raise typer.Exit(1)
    except json.JSONDecodeError as e:
        console.print(f"[red]Error:[/red] invalid JSON: {e}")
        raise typer.Exit(1)

    if not isinstance(data, list):
        console.print("[red]Error:[/red] expected a JSON array of eval results")
        raise typer.Exit(1)

    if fmt_lower == "csv":
        content = _export_csv(data)
    elif fmt_lower == "latex":
        content = _export_latex(data, caption=caption)
    else:
        content = json.dumps(data, indent=2, default=str)

    if output:
        Path(output).parent.mkdir(parents=True, exist_ok=True)
        with open(output, "w") as f:
            f.write(content)
        console.print(f"[green]Exported {fmt_lower} to {output}[/green]")
    else:
        typer.echo(content)


def _export_csv(data: list[dict]) -> str:
    """Convert eval results list to CSV string."""
    import csv as csv_mod
    import io

    buf = io.StringIO()
    writer = csv_mod.writer(buf)
    writer.writerow(["row_id", "passed", "score", "duration_ms", "cost_usd"])
    for row in data:
        writer.writerow(
            [
                row.get("row_id", ""),
                row.get("passed", ""),
                row.get("overall_score", ""),
                f"{row.get('duration_ms', 0):.1f}" if row.get("duration_ms") is not None else "",
                f"{row.get('cost_usd', 0):.6f}" if row.get("cost_usd") is not None else "",
            ]
        )
    return buf.getvalue()


def _export_latex(data: list[dict], *, caption: str = "Evaluation Results") -> str:
    """Convert eval results list to a LaTeX booktabs table."""
    lines: list[str] = []
    lines.append(r"\begin{table}[htbp]")
    lines.append(r"\centering")
    lines.append(rf"\caption{{{_latex_esc(caption)}}}")
    lines.append(r"\begin{tabular}{lccccc}")
    lines.append(r"\toprule")
    lines.append(r"Row ID & Passed & Score & Duration (ms) & Cost (USD) \\")
    lines.append(r"\midrule")

    for row in data:
        row_id = _latex_esc(str(row.get("row_id", "")))
        passed = row.get("passed", False)
        passed_str = r"\checkmark" if passed else r"\ding{55}"
        score = row.get("overall_score")
        score_str = f"{score:.1f}" if score is not None else "--"
        duration = row.get("duration_ms")
        dur_str = f"{duration:.0f}" if duration is not None else "--"
        cost = row.get("cost_usd")
        cost_str = f"{cost:.3f}" if cost is not None else "--"
        lines.append(f"{row_id} & {passed_str} & {score_str} & {dur_str} & {cost_str} " + r"\\")

    lines.append(r"\bottomrule")
    lines.append(r"\end{tabular}")
    lines.append(r"\label{tab:eval-results}")
    lines.append(r"\end{table}")
    return "\n".join(lines) + "\n"


def _latex_esc(text: str) -> str:
    """Escape special LaTeX characters."""
    for char, replacement in [
        ("&", r"\&"),
        ("%", r"\%"),
        ("$", r"\$"),
        ("#", r"\#"),
        ("_", r"\_"),
        ("{", r"\{"),
        ("}", r"\}"),
    ]:
        text = text.replace(char, replacement)
    return text


@eval_app.command("compare")
def eval_compare(
    results_json: str = typer.Argument(..., help="Path to a GridResults JSON file (from grid.to_json())"),
    conditions: str = typer.Option(
        ..., "--conditions", "-c", help='Comma-separated pair of condition keys, e.g. "debate,react"'
    ),
    test: str = typer.Option("auto", "--test", "-t", help="Statistical test: welch, wilcoxon, mann_whitney, auto"),
    alpha: float = typer.Option(0.05, "--alpha", help="Significance level (default 0.05)"),
    fmt: str = typer.Option("table", "--format", "-f", help="Output format: table (default) or json"),
) -> None:
    """Compare two conditions with statistical tests.

    \b
    Examples:
      jamjet eval compare results.json --conditions "debate,react"
      jamjet eval compare results.json -c "strategy=debate,strategy=react" --test welch
      jamjet eval compare results.json -c "debate,react" --test auto --alpha 0.01
      jamjet eval compare results.json -c "debate,react" --format json
    """
    from jamjet.eval.grid import GridResults

    # Parse conditions argument — expect exactly 2 items.
    parts = [p.strip() for p in conditions.split(",")]
    if len(parts) != 2:
        console.print("[red]Error:[/red] --conditions must have exactly 2 values separated by a comma")
        raise typer.Exit(1)

    # Load the GridResults JSON.
    try:
        grid_results = GridResults.from_json(results_json)
    except FileNotFoundError:
        console.print(f"[red]Error:[/red] file not found: {results_json}")
        raise typer.Exit(1)
    except (json.JSONDecodeError, ValueError) as e:
        console.print(f"[red]Error:[/red] {e}")
        raise typer.Exit(1)

    # Resolve condition dicts from the short names.
    # Users can pass either "react" (matched against any condition value)
    # or "strategy=react" (exact key=value match).
    agg = grid_results._aggregate_by_condition()
    cond_a = _resolve_condition(parts[0], agg)
    cond_b = _resolve_condition(parts[1], agg)

    if cond_a is None:
        console.print(f"[red]Error:[/red] condition not found: {parts[0]}")
        console.print(f"Available conditions: {', '.join(agg.keys())}")
        raise typer.Exit(1)
    if cond_b is None:
        console.print(f"[red]Error:[/red] condition not found: {parts[1]}")
        console.print(f"Available conditions: {', '.join(agg.keys())}")
        raise typer.Exit(1)

    # Parse condition strings back to dicts.
    cond_dict_a = dict(part.split("=", 1) for part in cond_a.split(", "))
    cond_dict_b = dict(part.split("=", 1) for part in cond_b.split(", "))

    try:
        result = grid_results.compare(cond_dict_a, cond_dict_b, test=test, alpha=alpha)
    except ValueError as e:
        console.print(f"[red]Error:[/red] {e}")
        raise typer.Exit(1)

    if fmt.lower() == "json":
        import dataclasses

        typer.echo(json.dumps(dataclasses.asdict(result), indent=2, default=str))
    else:
        _print_comparison_table(result, alpha=alpha)


def _resolve_condition(name: str, agg: Mapping[str, object]) -> str | None:
    """Resolve a short condition name to a full condition key from the aggregation."""
    # Exact match first.
    if name in agg:
        return name
    # Try matching as a value substring (e.g. "react" matches "strategy=react").
    for key in agg:
        # Check if name appears as a value in any key=value pair.
        parts = key.split(", ")
        for part in parts:
            if "=" in part:
                _k, v = part.split("=", 1)
                if v == name:
                    return key
    return None


def _print_comparison_table(result: ComparisonResult, *, alpha: float = 0.05) -> None:
    """Print a Rich table summarizing a ComparisonResult."""
    table = Table(title="Statistical Comparison", show_header=True, header_style="bold")
    table.add_column("Metric", style="cyan")
    table.add_column("Value", justify="right")

    table.add_row("Condition A", result.condition_a)
    table.add_row("Condition B", result.condition_b)
    table.add_row("Mean A", f"{result.mean_a:.4f}")
    table.add_row("Mean B", f"{result.mean_b:.4f}")
    table.add_row("Mean Difference (A - B)", f"{result.mean_diff:+.4f}")
    table.add_row("Sample Sizes", f"n_a={result.sample_sizes[0]}, n_b={result.sample_sizes[1]}")
    table.add_row("Test", result.test_name)

    if result.statistic is not None:
        table.add_row("Test Statistic", f"{result.statistic:.4f}")
    else:
        table.add_row("Test Statistic", "-")

    if result.p_value is not None:
        sig_style = "[green]" if result.significant else "[dim]"
        table.add_row("p-value", f"{sig_style}{result.p_value:.6f}[/]")
    else:
        table.add_row("p-value", "-")

    sig_label = f"[green]Yes (p < {alpha})[/green]" if result.significant else f"[dim]No (p >= {alpha})[/dim]"
    if result.p_value is None:
        sig_label = "[yellow]N/A (scipy not installed)[/yellow]"
    table.add_row("Significant", sig_label)

    if result.effect_size is not None:
        magnitude = _effect_size_label(result.effect_size)
        table.add_row("Cohen's d", f"{result.effect_size:.4f} ({magnitude})")
    else:
        table.add_row("Cohen's d", "-")

    if result.ci_lower is not None and result.ci_upper is not None:
        ci_pct = int((1 - alpha) * 100)
        table.add_row(f"{ci_pct}% CI for Mean Diff", f"[{result.ci_lower:.4f}, {result.ci_upper:.4f}]")
    else:
        table.add_row("CI for Mean Diff", "-")

    console.print(table)


def _effect_size_label(d: float) -> str:
    """Return a human-readable label for Cohen's d magnitude."""
    abs_d = abs(d)
    if abs_d < 0.2:
        return "negligible"
    elif abs_d < 0.5:
        return "small"
    elif abs_d < 0.8:
        return "medium"
    else:
        return "large"


if __name__ == "__main__":
    app()
