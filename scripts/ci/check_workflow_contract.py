#!/usr/bin/env python3
"""FS-003 CI 入口契约：确保关键能力不会从 workflow 中静默消失。"""

from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"


def main() -> int:
    text = WORKFLOW.read_text(encoding="utf-8")
    requirements = {
        "parse-vlm compile gate": r"cargo check -p fastsearch-cli --features parse-vlm",
        "both Python SDK suites": r"python3 test_integrations\.py && python3 test_ingest\.py",
        "environment gate summary": r"scripts/ci/run_environment_gates\.sh --require-pg",
        "MCP/server binary job": r"(?m)^  mcp-server-e2e:",
        "MCP/server binary runner": r"scripts/ci/mcp_server_e2e\.sh",
        "example test/typecheck job": (
            r"(?ms)^  example:\n(?:(?!^  \S).)*?run: npm test && npm run typecheck"
        ),
    }
    missing = [name for name, pattern in requirements.items() if not re.search(pattern, text)]
    if missing:
        for name in missing:
            print(f"missing CI contract: {name}", file=sys.stderr)
        return 1
    print(f"ci-workflow-contract: PASS ({len(requirements)} requirements)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
