# Deployment Guide

---

## Local Dev (SQLite)

```bash
jamjet dev
```

Starts the runtime locally with SQLite. No external services needed. Good for development and testing.

---

## Production (Postgres + Workers)

### 1. Set up Postgres

```bash
# Run migrations
jamjet db migrate --database-url postgresql://user:pass@host/jamjet
```

### 2. Start the runtime

```bash
JAMJET_DATABASE_URL=postgresql://user:pass@host/jamjet \
JAMJET_PORT=7700 \
jamjet server start
```

### 3. Start workers

```bash
# General worker
jamjet worker start --queues general,tool,model

# Dedicated model worker
jamjet worker start --queues model --concurrency 10

# Privileged worker (for sensitive tools)
jamjet worker start --queues privileged --concurrency 2
```

### 4. Deploy your workflow

```bash
jamjet deploy --runtime http://your-runtime:7700
```

---

## Docker

```dockerfile
FROM jamjet/runtime:latest

COPY workflow.yaml agents.yaml tools.yaml schemas.py ./
```

```bash
docker run -e DATABASE_URL=postgresql://... jamjet/runtime
```

---

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `JAMJET_DATABASE_URL` | Postgres connection URL | (SQLite if unset) |
| `JAMJET_PORT` | API server port | `7700` |
| `JAMJET_LOG_LEVEL` | Log level | `info` |
| `JAMJET_SNAPSHOT_INTERVAL` | Events between snapshots | `50` |
| `JAMJET_WORKER_CONCURRENCY` | Max concurrent tasks per worker | `5` |
| `JAMJET_LEASE_TIMEOUT_SECS` | Worker lease expiry | `60` |
| `JAMJET_OTEL_ENDPOINT` | OpenTelemetry OTLP endpoint | (disabled) |

---

## Health Checks

```bash
# Runtime health
GET /health

# Detailed status
GET /status
```
