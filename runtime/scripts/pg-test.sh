#!/usr/bin/env bash
# A local Postgres in Docker for the Postgres tests.
#
#   runtime/scripts/pg-test.sh up      start it, or find it running, and print the export line
#   runtime/scripts/pg-test.sh url     print the URL the tests read from JAMJET_TEST_POSTGRES_URL
#   runtime/scripts/pg-test.sh reset   drop the databases tests left behind (jj_t_*, jj_tmpl_*)
#   runtime/scripts/pg-test.sh down    stop and remove the container
#
# Port 55440 stays clear of the Postgres servers other stacks run on 5432.
# The script never pulls the image (a pull can hang on a Docker credential
# helper), so run `docker pull postgres:16-alpine` once first.
set -euo pipefail

NAME=jamjet-pg-test
PORT=55440
IMAGE=postgres:16-alpine
PASSWORD=jamjet-test
URL="postgres://postgres:${PASSWORD}@127.0.0.1:${PORT}/postgres"

need_docker() {
  if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
    echo "Docker is not available; Postgres tests will skip" >&2
    exit 1
  fi
}

# true, false, or missing
running() {
  local state
  if state=$(docker inspect -f '{{.State.Running}}' "$NAME" 2>/dev/null); then
    echo "$state"
  else
    echo missing
  fi
}

wait_ready() {
  # Over TCP: while the image sets itself up, its server listens only on a socket.
  for _ in $(seq 1 60); do
    if docker exec "$NAME" pg_isready -q -U postgres -h 127.0.0.1; then
      return 0
    fi
    sleep 0.5
  done
  echo "Postgres in $NAME is not ready after 30 s; see docker logs $NAME" >&2
  exit 1
}

psql_admin() {
  docker exec "$NAME" psql -q -U postgres -h 127.0.0.1 -v ON_ERROR_STOP=1 -At "$@"
}

case "${1:-}" in
  up)
    need_docker
    case "$(running)" in
      true) ;;
      false) docker start "$NAME" >/dev/null ;;
      *)
        docker run -d --pull never --name "$NAME" -p "127.0.0.1:${PORT}:5432" \
          -e POSTGRES_PASSWORD="$PASSWORD" "$IMAGE" >/dev/null
        ;;
    esac
    wait_ready
    echo "export JAMJET_TEST_POSTGRES_URL=$URL"
    ;;
  url)
    echo "$URL"
    ;;
  reset)
    need_docker
    if [ "$(running)" != true ]; then
      echo "$NAME is not running; nothing to reset" >&2
      exit 1
    fi
    # Only names the test kit makes. A database still in use is kept: it
    # belongs to a run that is still going.
    for db in $(psql_admin -c "SELECT datname FROM pg_database WHERE datname ~ '^jj_(t|tmpl)_[a-z0-9_]+\$' ORDER BY 1"); do
      if psql_admin -c "DROP DATABASE \"$db\"" 2>/dev/null; then
        echo "dropped $db"
      else
        echo "kept $db: in use" >&2
      fi
    done
    ;;
  down)
    need_docker
    if [ "$(running)" = missing ]; then
      echo "$NAME does not exist"
    else
      docker rm -f "$NAME" >/dev/null
      echo "removed $NAME"
    fi
    ;;
  *)
    echo "usage: $0 up|url|reset|down" >&2
    exit 2
    ;;
esac
