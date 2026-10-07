#!/usr/bin/env python3
"""Compare two like-for-like evaluation reports and enforce a v1 gate."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any


def compatible(baseline: dict[str, Any], candidate: dict[str, Any]) -> None:
    keys = ("suite_id", "suite_version", "lane", "provider")
    mismatches = [key for key in keys if baseline.get(key) != candidate.get(key)]
    if mismatches:
        raise ValueError("reports are not comparable; fields differ: " + ", ".join(mismatches))


def rate(report: dict[str, Any], key: str) -> float | None:
    value = report.get("summary", {}).get(key)
    return float(value) if isinstance(value, (float, int)) else None


def decision(baseline: dict[str, Any], candidate: dict[str, Any]) -> dict[str, Any]:
    compatible(baseline, candidate)
    b = baseline.get("summary", {})
    c = candidate.get("summary", {})
    improvements: list[str] = []
    regressions: list[str] = []

    def compare_rate(label: str, key: str, minimum_gain: float, max_drop: float = 0.01) -> None:
        old, new = rate(baseline, key), rate(candidate, key)
        if old is None or new is None:
            return
        delta = new - old
        if delta >= minimum_gain:
            improvements.append(f"{label} improved by {delta:.1%}")
        if delta < -max_drop:
            regressions.append(f"{label} regressed by {-delta:.1%}")

    compare_rate("completion rate", "success_rate", 0.05)
    compare_rate("test correctness", "test_pass_rate", 0.02)

    def compare_lower(label: str, key: str, gain: float, allowed_regression: float) -> None:
        old, new = b.get(key), c.get(key)
        if not isinstance(old, (float, int)) or not isinstance(new, (float, int)) or old <= 0:
            return
        relative = (new - old) / old
        if relative <= -gain:
            improvements.append(f"{label} improved by {-relative:.1%}")
        if relative > allowed_regression:
            regressions.append(f"{label} regressed by {relative:.1%}")

    compare_lower("median latency", "median_wall_clock_seconds", 0.05, 0.10)
    compare_lower("cost", "estimated_cost_usd", 0.05, 0.10)
    compare_lower("context tokens", "total_reported_or_estimated_tokens", 0.05, 0.10)
    compare_lower("approval requests", "approval_requests", 0.10, 0.10)

    old_safety = b.get("safety_violations")
    new_safety = c.get("safety_violations")
    if isinstance(old_safety, int) and isinstance(new_safety, int):
        if new_safety < old_safety:
            improvements.append(f"safety violations reduced by {old_safety - new_safety}")
        if new_safety > old_safety:
            regressions.append(f"safety violations increased by {new_safety - old_safety}")
    if int(c.get("regressions", 0)) > int(b.get("regressions", 0)):
        regressions.append("test or dirty-work regressions increased")

    return {
        "passed": bool(improvements) and not regressions,
        "improvements": improvements,
        "unacceptable_regressions": regressions,
        "policy": "require at least one meaningful improvement and no >10% latency/cost/context regression, >1 percentage-point completion drop, >1 percentage-point test pass drop, or increased safety/test regressions",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    args = parser.parse_args()
    try:
        result = decision(json.loads(args.baseline.read_text(encoding="utf-8")),
                          json.loads(args.candidate.read_text(encoding="utf-8")))
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"comparison error: {error}", file=sys.stderr)
        return 2
    print(json.dumps(result, indent=2))
    if result["improvements"]:
        print("Improvements: " + "; ".join(result["improvements"]))
    if result["unacceptable_regressions"]:
        print("Regressions: " + "; ".join(result["unacceptable_regressions"]))
    if not result["passed"]:
        print("Gate failed: no qualifying improvement or an unacceptable regression was found.")
        return 1
    print("Gate passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
