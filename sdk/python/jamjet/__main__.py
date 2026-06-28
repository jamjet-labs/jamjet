"""Enable `python -m jamjet ...` as an alias for the `jamjet` console script.

This makes `jamjet dev` able to spawn the worker as a subprocess using the same
interpreter/venv (`python -m jamjet worker ...`) without relying on the console
script being on PATH.
"""

from __future__ import annotations

from jamjet.cli.main import app

if __name__ == "__main__":
    app()
