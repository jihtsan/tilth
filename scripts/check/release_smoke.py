#!/usr/bin/env python3
"""Release smoke check: a built tilth binary runs on its own platform.

The release workflow runs this on each packaged binary before anything is
published. It checks, in order, and stops at the first failure:

1. `tilth --version` prints the expected version.
2. An MCP server answers `initialize` with that version.
3. In the same session, `tilth_search` finds the one function in the
   fixture next to this script (`release_smoke/greeting.rs`).

Usage:
  python3 scripts/check/release_smoke.py TILTH_BINARY VERSION

Exits 0 when every check passes, 1 on the first that fails.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

FIXTURE = Path(__file__).resolve().parent / "release_smoke"
SYMBOL = "smoke_greeting"
# A hung binary fails the check; a working one answers in well under a second.
TIMEOUT_SECONDS = 60


class SmokeFailure(Exception):
    pass


def check_version(binary: str, version: str) -> None:
    out = subprocess.run(
        [binary, "--version"],
        capture_output=True,
        encoding="utf-8",
        timeout=TIMEOUT_SECONDS,
        check=False,
    )
    printed = out.stdout.strip()
    if out.returncode != 0 or printed != f"tilth {version}":
        raise SmokeFailure(
            f"--version exited {out.returncode} and printed {printed!r}, "
            f"expected 'tilth {version}'\nstderr: {out.stderr}"
        )


def mcp_session(binary: str) -> dict[int, dict]:
    """Send initialize and one search, close stdin, return the answers by id."""
    requests = [
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "release-smoke", "version": "0"},
            },
        },
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "tilth_search",
                "arguments": {"query": SYMBOL, "scope": str(FIXTURE)},
            },
        },
    ]
    stdin = "".join(json.dumps(r) + "\n" for r in requests)
    out = subprocess.run(
        [binary, "--mcp"],
        cwd=FIXTURE,
        input=stdin,
        capture_output=True,
        encoding="utf-8",
        timeout=TIMEOUT_SECONDS,
        check=False,
    )
    if out.returncode != 0:
        raise SmokeFailure(f"--mcp exited {out.returncode}\nstderr: {out.stderr}")
    answers = {}
    for line in out.stdout.splitlines():
        message = json.loads(line)
        if "id" in message:
            answers[message["id"]] = message
    return answers


def check_initialize(answer: dict | None, version: str) -> None:
    info = (answer or {}).get("result", {}).get("serverInfo", {})
    if info.get("name") != "tilth" or info.get("version") != version:
        raise SmokeFailure(f"initialize answered {json.dumps(answer)[:500]}")


def check_search(answer: dict | None) -> None:
    result = (answer or {}).get("result", {})
    text = "".join(c.get("text", "") for c in result.get("content", []))
    if result.get("isError") or "greeting.rs" not in text or "[definition]" not in text:
        raise SmokeFailure(
            f"tilth_search for {SYMBOL} did not find its definition:\n"
            f"{text or json.dumps(answer)[:500]}"
        )


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 1
    # Absolute, because the MCP server starts in the fixture directory and a
    # relative path would resolve against that.
    binary, version = str(Path(sys.argv[1]).resolve()), sys.argv[2]
    try:
        check_version(binary, version)
        print(f"ok: --version is {version}", flush=True)
        answers = mcp_session(binary)
        check_initialize(answers.get(1), version)
        print("ok: MCP initialize answered", flush=True)
        check_search(answers.get(2))
        print(f"ok: tilth_search found {SYMBOL}", flush=True)
    except (SmokeFailure, OSError, subprocess.TimeoutExpired, json.JSONDecodeError) as e:
        print(f"release smoke check failed: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
