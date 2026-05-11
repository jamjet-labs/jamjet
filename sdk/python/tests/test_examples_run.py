"""Run each example/main.py and assert exit 0 + expected-output.txt match."""

import subprocess
import sys
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[3]
EXAMPLES = [
    "01-block-unsafe-tool",
    "02-human-approval",
    "03-budget-cap",
    "04-mcp-tool-policy",
]


@pytest.mark.parametrize("example", EXAMPLES)
def test_example_exits_zero_and_matches_expected(example: str) -> None:
    example_dir = REPO_ROOT / "examples" / example
    main_py = example_dir / "main.py"
    expected_path = example_dir / "expected-output.txt"
    assert main_py.exists(), f"{main_py} missing"
    assert expected_path.exists(), f"{expected_path} missing"

    proc = subprocess.run(
        [sys.executable, str(main_py)],
        capture_output=True,
        text=True,
        cwd=example_dir,
        timeout=30,
    )
    assert proc.returncode == 0, f"{example} exit={proc.returncode}\n{proc.stderr}"
    assert proc.stdout.strip() == expected_path.read_text().strip(), \
        f"{example} output drift"
