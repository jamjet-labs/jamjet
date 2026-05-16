#!/usr/bin/env bash
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$DIR"

echo "Creating virtual environment..."
PYTHON_BIN="${PYTHON:-python3}"
if ! command -v "$PYTHON_BIN" >/dev/null 2>&1; then
  PYTHON_BIN="python"
fi
"$PYTHON_BIN" -m venv .venv

if [ -f ".venv/bin/activate" ]; then
  ACTIVATE_SCRIPT=".venv/bin/activate"
elif [ -f ".venv/Scripts/activate" ]; then
  ACTIVATE_SCRIPT=".venv/Scripts/activate"
else
  echo "Could not find virtual environment activation script." >&2
  exit 1
fi
source "$ACTIVATE_SCRIPT"

echo "Installing dependencies..."
python -m pip install -q --upgrade pip
python -m pip install -q -r requirements.txt

echo ""
echo "Done. To run:"
echo ""
echo "  source $ACTIVATE_SCRIPT"
echo "  python main.py"
