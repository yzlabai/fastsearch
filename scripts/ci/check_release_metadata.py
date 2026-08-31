#!/usr/bin/env python3
"""FS-004 release gate: keep legal, version and publication metadata aligned."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[2]
RUST_VERSION = "0.2.0-rc.1"
TYPESCRIPT_VERSION = "0.3.0"
PYTHON_VERSION = "0.2.0"
ACTIVE_PLAN = "2026-08-30-迭代开发计划.md"
HISTORICAL_PLANS = (
    "2026-06-25-初始快照-bootstrap.md",
    "2026-06-25-派生索引持久化与崩溃安全.md",
    "2026-06-25-多模态数据支持-需求分析.md",
    "2026-06-26-A9-向量HNSW与量化设计.md",
    "2026-06-26-B6-pgvector直查档设计.md",
    "2026-06-26-docparse融合开发计划.md",
    "2026-06-28-CLI改为REST客户端设计.md",
    "2026-06-30-下一步改进路线图.md",
    "2026-07-05-代码审查修复迭代计划.md",
    "2026-07-21-向量量化压缩主索引-TurboQuant借鉴.md",
    "2026-07-22-FHT结构化旋转.md",
    "2026-08-24-知识库引擎迭代计划.md",
    "2026-08-26-KB-2.2-N路具名融合设计.md",
)


class Checks:
    def __init__(self) -> None:
        self.failures: list[str] = []
        self.count = 0

    def require(self, condition: bool, message: str) -> None:
        self.count += 1
        if not condition:
            self.failures.append(message)

    def contains(self, path: Path, needles: tuple[str, ...], label: str) -> None:
        self.require(path.is_file(), f"{label}: missing {path.relative_to(ROOT)}")
        if not path.is_file():
            return
        text = path.read_text(encoding="utf-8")
        for needle in needles:
            self.require(needle in text, f"{label}: missing {needle!r}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--third-party-notices",
        type=Path,
        help="cargo-about output to validate in release CI",
    )
    return parser.parse_args()


def check_legal(checks: Checks, notices: Path | None) -> None:
    checks.contains(ROOT / "LICENSE", ("Apache License", "Version 2.0"), "LICENSE")
    checks.contains(
        ROOT / "NOTICE",
        ("fastsearch", "vendor/docparse/NOTICE", "THIRD-PARTY-NOTICES.md"),
        "NOTICE",
    )
    checks.contains(
        ROOT / "CHANGELOG.md", (RUST_VERSION, "0.1.0"), "CHANGELOG"
    )
    if notices is not None:
        path = notices if notices.is_absolute() else ROOT / notices
        checks.require(path.is_file(), f"cargo-about output missing: {path}")
        if path.is_file():
            checks.require(path.stat().st_size > 100, "cargo-about output is unexpectedly empty")


def check_packages(checks: Checks) -> None:
    cargo = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    package = cargo["workspace"]["package"]
    checks.require(package.get("version") == RUST_VERSION, "Rust workspace version mismatch")
    checks.require(package.get("license") == "Apache-2.0", "Rust workspace license mismatch")
    checks.require(
        package.get("repository") == "https://github.com/yzlabai/fastsearch",
        "Rust workspace repository mismatch",
    )
    for manifest in sorted((ROOT / "crates").glob("*/Cargo.toml")):
        crate = tomllib.loads(manifest.read_text(encoding="utf-8"))["package"]
        for field in ("version", "license", "repository"):
            checks.require(
                crate.get(field, {}).get("workspace") is True,
                f"{manifest.relative_to(ROOT)} must inherit workspace {field}",
            )
    cargo_lock = tomllib.loads((ROOT / "Cargo.lock").read_text(encoding="utf-8"))
    locked = {
        item["name"]: item["version"]
        for item in cargo_lock["package"]
        if item["name"].startswith("fastsearch-")
    }
    checks.require(bool(locked), "Cargo.lock contains no fastsearch workspace packages")
    for name, version in sorted(locked.items()):
        checks.require(version == RUST_VERSION, f"Cargo.lock {name} version mismatch")

    ts = json.loads((ROOT / "clients/typescript/package.json").read_text(encoding="utf-8"))
    checks.require(ts.get("version") == TYPESCRIPT_VERSION, "TypeScript SDK version mismatch")
    checks.require(ts.get("license") == "Apache-2.0", "TypeScript SDK license mismatch")
    checks.require(ts.get("repository", {}).get("directory") == "clients/typescript", "TypeScript repository metadata mismatch")
    checks.require(set(ts.get("files", ())) >= {"dist", "src"}, "TypeScript package files mismatch")
    ts_lock = json.loads((ROOT / "clients/typescript/package-lock.json").read_text(encoding="utf-8"))
    checks.require(ts_lock.get("version") == TYPESCRIPT_VERSION, "TypeScript lock version mismatch")

    python = tomllib.loads((ROOT / "clients/python/pyproject.toml").read_text(encoding="utf-8"))["project"]
    checks.require(python.get("version") == PYTHON_VERSION, "Python SDK version mismatch")
    checks.require(python.get("license", {}).get("text") == "Apache-2.0", "Python SDK license mismatch")
    checks.require(
        python.get("urls", {}).get("Repository") == "https://github.com/yzlabai/fastsearch/tree/main/clients/python",
        "Python SDK repository metadata mismatch",
    )

    example = json.loads((ROOT / "example/package.json").read_text(encoding="utf-8"))
    checks.require(
        example.get("dependencies", {}).get("fastsearch-client") == "file:../clients/typescript",
        "example must consume the repository TypeScript SDK",
    )
    example_lock = json.loads((ROOT / "example/package-lock.json").read_text(encoding="utf-8"))
    locked_sdk = example_lock.get("packages", {}).get("node_modules/fastsearch-client", {})
    checks.require(locked_sdk.get("link") is True, "example lock must link the repository SDK")
    checks.require(
        locked_sdk.get("resolved") == "../clients/typescript",
        "example lock repository SDK path mismatch",
    )


def check_documents(checks: Checks) -> None:
    for readme in (ROOT / "README.md", ROOT / "README.zh-CN.md"):
        checks.contains(
            readme,
            (RUST_VERSION, TYPESCRIPT_VERSION, PYTHON_VERSION, "LICENSE", "NOTICE"),
            str(readme.relative_to(ROOT)),
        )
    for name in HISTORICAL_PLANS:
        checks.contains(
            ROOT / "docs/plans" / name,
            ("历史计划，已归档", ACTIVE_PLAN),
            name,
        )
    checks.contains(
        ROOT / "docs/plans" / ACTIVE_PLAN,
        ("唯一活动执行看板",),
        ACTIVE_PLAN,
    )


def check_ci(checks: Checks) -> None:
    checks.contains(
        ROOT / ".github/workflows/ci.yml",
        (
            "python3 scripts/ci/check_release_metadata.py --third-party-notices THIRD-PARTY-NOTICES.md",
            "cargo about generate about.hbs > THIRD-PARTY-NOTICES.md",
        ),
        "CI release gate",
    )


def main() -> int:
    args = parse_args()
    checks = Checks()
    check_legal(checks, args.third_party_notices)
    check_packages(checks)
    check_documents(checks)
    check_ci(checks)
    if checks.failures:
        for failure in checks.failures:
            print(f"release metadata: FAIL: {failure}", file=sys.stderr)
        print(
            f"release-metadata: FAIL ({len(checks.failures)}/{checks.count} requirements)",
            file=sys.stderr,
        )
        return 1
    print(f"release-metadata: PASS ({checks.count} requirements)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
