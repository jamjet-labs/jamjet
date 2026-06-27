# Running the model seam sidecar

The JamJet runtime routes durable-path model calls through a local Python process that wraps `jamjet.model.Model`. This gives the Rust engine the same governed seam as in-process `Agent.run()`: provider allowlist, PII redaction, cost metering, and any middleware you register.

## Install

```
pip install "jamjet[sidecar]"
```

This pulls in `starlette` and `uvicorn` alongside the core model dependencies.

## Start

```
uvicorn jamjet.model.sidecar_server:app --host 127.0.0.1 --port 4280
```

## Configure the runtime

Set this environment variable before starting the runtime process:

```
JAMJET_MODEL_SEAM_URL=http://127.0.0.1:4280
```

The runtime probes `GET /health` at startup. If the sidecar is unreachable or returns a non-200 status, the runtime refuses to start with a clear error message. This fail-loud guard ensures model calls never silently bypass the governed seam. If the variable is absent, the runtime falls back to native Rust adapters, which is the default for development.
