# WASI Tool Sandboxing Guide

> **Status:** Planned — Phase 3 (v1), Phase 4 (production hardening). This guide will be written when WASI sandboxing is implemented.

---

## Overview

JamJet supports executing tools in **WASI/Wasm sandboxes** via Wasmtime — a Rust-native WebAssembly runtime. Sandboxed tools run in capability-constrained containers: they can only access filesystem paths, network hosts, and environment variables that are explicitly declared.

This is particularly useful for:
- Third-party or community-contributed tools you don't fully trust
- Tools that should never access your host filesystem or network
- Reproducible, deterministic tool execution

## Why Wasmtime

Wasmtime is Rust-native and integrates directly with Tokio's async runtime — making it a natural fit for JamJet's execution core with minimal overhead.

## Planned YAML shape

```yaml
tools:
  code_executor:
    type: wasi_sandbox
    module: tools/code_exec.wasm    # pre-compiled Wasm module
    capabilities:
      filesystem:
        - path: /tmp/scratch
          mode: read_write
      network:
        - host: api.github.com
          port: 443
      env:
        - LANG
        - TZ
    limits:
      memory_mb: 256
      cpu_time_ms: 30000
      fuel: 1000000                 # Wasmtime fuel for deterministic CPU limiting
```

## Planned guarantees

- Sandbox enforces declared capabilities — no undeclared filesystem/network access
- Tools respect memory and CPU limits
- Performance overhead target: < 10% vs native tool execution for typical tools
- Wasm modules are portable across platforms

## Tracking

Follow progress in `progress-tracker.md` under Phase 3, tasks 3.24–3.27.
