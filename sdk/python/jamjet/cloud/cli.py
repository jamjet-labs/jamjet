"""Plan 5 Phase 6.3 — `jamjet-cloud` CLI entry point.

Commands:
  jamjet-cloud replay <trace_id>   Download and display a cloud trace bundle,
                                   then print re-run instructions.

Authentication: set JAMJET_API_KEY (or --api-key).
"""

from __future__ import annotations

import io
import json
import os
import tarfile
import tempfile
from pathlib import Path
from typing import Annotated

import httpx
import typer
from rich.console import Console
from rich.padding import Padding
from rich.table import Table
from rich import box

from .replay import ReplayBundle, load_bundle_from_bytes

console = Console()
app = typer.Typer(
    name="jamjet-cloud",
    help="JamJet Cloud CLI — governance, replay, and trace tools.",
    no_args_is_help=True,
    add_completion=False,
)


@app.callback()
def _root() -> None:
    """JamJet Cloud CLI."""

_DEFAULT_API_URL = "https://api.jamjet.dev"

# Status colour mapping
_STATUS_STYLE = {
    "ok": "green",
    "blocked": "red",
    "pending_approval": "yellow",
    "approved": "cyan",
    "error": "red",
}

_KIND_ICON = {
    "llm_call": "🤖",
    "tool_call": "🔧",
    "custom": "⚙️",
}


@app.command("replay")
def replay(
    trace_id: Annotated[str, typer.Argument(help="Cloud trace ID to replay (e.g. tr_abc123)")],
    stub_models: Annotated[bool, typer.Option("--stub-models", help="Return recorded LLM responses instead of re-issuing API calls")] = False,
    api_key: Annotated[str | None, typer.Option("--api-key", envvar="JAMJET_API_KEY", help="JamJet Cloud API key")] = None,
    api_url: Annotated[str, typer.Option("--api-url", envvar="JAMJET_API_URL", help="Cloud API base URL")] = _DEFAULT_API_URL,
    extract_to: Annotated[Path | None, typer.Option("--extract-to", help="Directory to extract the bundle into (default: ~/.jamjet/replays/<trace_id>)")] = None,
) -> None:
    """Download a cloud trace bundle, display the timeline, and print re-run instructions.

    Re-run the trace locally by setting the environment variables printed at the end:

    \b
    export JAMJET_REPLAY_BUNDLE=<path>      # or --stub-models also sets JAMJET_STUB_MODELS=1
    python your_agent.py
    """
    resolved_key = api_key or os.environ.get("JAMJET_API_KEY")
    if not resolved_key:
        console.print("[red]Error:[/red] API key required. Set JAMJET_API_KEY or use --api-key.")
        raise typer.Exit(1)

    # --- Download ---
    url = f"{api_url.rstrip('/')}/v1/traces/{trace_id}/replay"
    console.print(f"[dim]Downloading replay bundle for[/dim] [bold]{trace_id}[/bold] …")
    try:
        r = httpx.get(url, headers={"Authorization": f"Bearer {resolved_key}"}, timeout=30.0)
    except httpx.ConnectError as exc:
        console.print(f"[red]Connection error:[/red] {exc}")
        raise typer.Exit(1) from exc

    if r.status_code == 404:
        console.print(f"[red]Trace not found:[/red] {trace_id}")
        raise typer.Exit(1)
    if not r.is_success:
        body = r.text[:200]
        console.print(f"[red]API error {r.status_code}:[/red] {body}")
        raise typer.Exit(1)

    schema_header = r.headers.get("x-jj-replay-schema", "unknown")
    if not schema_header.startswith("replay-1."):
        console.print(f"[yellow]Warning:[/yellow] bundle schema {schema_header!r} — this CLI expects replay-1.x")

    bundle_bytes = r.content

    # --- Extract ---
    dest = extract_to or Path.home() / ".jamjet" / "replays" / _sanitize(trace_id)
    dest.mkdir(parents=True, exist_ok=True)
    _extract_bundle(bundle_bytes, dest)
    console.print(f"[dim]Extracted to[/dim] {dest}")

    # --- Parse and display ---
    bundle = load_bundle_from_bytes(bundle_bytes)
    _display_bundle(bundle, stub_models=stub_models)

    # --- Re-run instructions ---
    console.print()
    console.rule("[bold]Re-run instructions[/bold]")
    stub_line = f"export JAMJET_STUB_MODELS=1\n" if stub_models else ""
    console.print(Padding(
        f"[dim]# Set these environment variables, then run your agent as normal:[/dim]\n"
        f"[bold]export JAMJET_REPLAY_BUNDLE={dest}[/bold]\n"
        f"{stub_line}"
        f"[bold]python your_agent.py[/bold]\n\n"
        f"[dim]Tool calls will replay from the recording.\n"
        f"{'LLM calls will return recorded responses (stub mode).' if stub_models else 'LLM calls will re-issue against real APIs (costs money).'}"
        f"[/dim]",
        pad=(0, 0, 0, 2),
    ))


def _display_bundle(bundle: ReplayBundle, *, stub_models: bool) -> None:
    m = bundle.manifest

    # Header
    console.print()
    console.rule(f"[bold]Trace[/bold] [dim]{m.get('trace_id', '')}[/dim]")
    console.print(
        f"  Schema [dim]{m.get('schema_version')}[/dim]  ·  "
        f"Events [bold]{m.get('event_count', len(bundle.events))}[/bold]  ·  "
        f"Cost [bold]${bundle.total_cost_usd:.4f}[/bold]  ·  "
        f"{'[yellow]stub-models[/yellow]' if stub_models else '[dim]live models[/dim]'}"
    )

    # Agents
    if bundle.agents:
        agent_names = ", ".join(a.get("name", "?") for a in bundle.agents)
        console.print(f"  Agents [cyan]{agent_names}[/cyan]")

    # Originating trace
    if m.get("originating_trace_id"):
        console.print(f"  [dim]← originated from[/dim] {m['originating_trace_id']}")

    # Events table
    console.print()
    tbl = Table(box=box.SIMPLE_HEAD, show_header=True, header_style="dim", expand=False)
    tbl.add_column("#", style="dim", width=4)
    tbl.add_column("Kind", width=11)
    tbl.add_column("Name", min_width=24)
    tbl.add_column("Agent", width=16)
    tbl.add_column("Status", width=10)
    tbl.add_column("Cost", width=9, justify="right")
    tbl.add_column("ms", width=7, justify="right")

    violations = 0
    for ev in bundle.events:
        status = ev.get("status", "ok")
        if status == "blocked":
            violations += 1
        style = _STATUS_STYLE.get(status, "")
        icon = _KIND_ICON.get(ev.get("kind", ""), "  ")
        cost = ev.get("cost_usd")
        cost_str = f"${float(cost):.4f}" if cost is not None else ""
        dur = ev.get("duration_ms")
        dur_str = str(int(dur)) if dur is not None else ""
        tbl.add_row(
            str(ev.get("sequence", "")),
            f"{icon} {ev.get('kind', '')}",
            ev.get("name", ""),
            ev.get("agent_name") or "",
            f"[{style}]{status}[/{style}]" if style else status,
            cost_str,
            dur_str,
        )

    console.print(tbl)

    # Audit violations
    if bundle.audit:
        console.print(f"  [red]⚠ {len(bundle.audit)} audit entr{'y' if len(bundle.audit) == 1 else 'ies'}:[/red]")
        for row in bundle.audit:
            action = row.get("action", "")
            detail = json.dumps(row.get("detail") or {})[:80]
            console.print(f"    [dim]{action}[/dim]  {detail}")


def _extract_bundle(data: bytes, dest: Path) -> None:
    """Extract tar.gz bundle into dest, stripping the top-level directory prefix.

    Only regular files are written; symlinks and hardlinks are skipped.
    Each resolved output path is checked to remain inside dest to prevent
    path-traversal via crafted tar entries.
    """
    dest_resolved = dest.resolve()
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as tar:
        for member in tar.getmembers():
            # Skip non-regular-file entries (symlinks, hardlinks, devices).
            if not member.isfile():
                continue
            stripped = member.name.split("/", 1)
            if len(stripped) < 2 or not stripped[1]:
                continue
            f = tar.extractfile(member)
            if f is None:
                continue
            out_path = (dest / stripped[1]).resolve()
            # Guard against path traversal (e.g. ../../etc/passwd in entry name).
            if not str(out_path).startswith(str(dest_resolved)):
                continue
            out_path.write_bytes(f.read())


def _sanitize(s: str) -> str:
    return "".join(c if c.isalnum() or c in "-_" else "_" for c in s)


@app.command("audit-verify")
def audit_verify(
    package: Path = typer.Argument(..., exists=True, dir_okay=False, readable=True,
                                   help="Path to the .json audit package."),
    metadata: Path = typer.Option(..., "--metadata", "-m", exists=True, dir_okay=False, readable=True,
                                   help="Path to the metadata JSON saved from POST /v1/audit/export."),
    api_url: str = typer.Option(_DEFAULT_API_URL, "--api-url",
                                help="Cloud base URL (override for self-hosted or local dev)."),
    pdf: Path | None = typer.Option(None, "--pdf", exists=True, dir_okay=False, readable=True,
                                    help="Optional PDF report — cross-check that its embedded bundle_sha256 matches."),
    otlp: Path | None = typer.Option(None, "--otlp", exists=True, dir_okay=False, readable=True,
                                     help="Optional OTLP JSON file — cross-check _jamjet_audit.bundle_sha256."),
    siem_splunk: Path | None = typer.Option(None, "--siem-splunk", exists=True, dir_okay=False, readable=True,
                                            help="Optional Splunk JSONL — cross-check fields.jj_audit_bundle_sha256."),
    siem_datadog: Path | None = typer.Option(None, "--siem-datadog", exists=True, dir_okay=False, readable=True,
                                             help="Optional Datadog JSONL — cross-check jj_audit_bundle_sha256."),
) -> None:
    """Verify the Ed25519 signature on an audit export package."""
    from .audit_verify import verify_from_files
    res = verify_from_files(
        package,
        metadata,
        api_url=api_url,
        pdf_path=pdf,
        otlp_path=otlp,
        siem_splunk_path=siem_splunk,
        siem_datadog_path=siem_datadog,
    )
    if res.ok:
        console.print(f"[green]OK[/green] · sha256={res.digest} · key_id={res.key_id}")
        raise typer.Exit(code=0)
    console.print(f"[red]FAIL[/red] · {res.reason} · sha256={res.digest}")
    raise typer.Exit(code=2)


def main() -> None:
    app()
