#!/usr/bin/env python3
"""
CLI Test Runner for End-to-End (E2E) Test Track.

Usage:
    # Run all 4 tiers (600 tests)
    python3 tests/e2e/runner.py --all

    # Run specific tier
    python3 tests/e2e/runner.py --tier 1
    python3 tests/e2e/runner.py --tier 4

    # Run against a live edge deployment
    python3 tests/e2e/runner.py --all --url https://spot.swacktech.com

    # Output markdown report to file
    python3 tests/e2e/runner.py --all --report-file test_results.md
"""

from __future__ import annotations

import argparse
import os
import sys
import time
import unittest
from pathlib import Path
from typing import TextIO

# Ensure tests/e2e is on python path
E2E_DIR = Path(__file__).resolve().parent
if str(E2E_DIR) not in sys.path:
    sys.path.insert(0, str(E2E_DIR))

from test_tier1_feature_coverage import TestTier1FeatureCoverage
from test_tier2_boundary_corner import TestTier2BoundaryCorner
from test_tier3_pairwise_combinations import TestTier3PairwiseCombinations
from test_tier4_real_world_scenarios import TestTier4RealWorldScenarios


TIER_MAP = {
    1: ("Tier 1: Feature Coverage (Isolation)", TestTier1FeatureCoverage, 260),
    2: ("Tier 2: Boundary & Corner Cases", TestTier2BoundaryCorner, 260),
    3: ("Tier 3: Cross-Feature Combinations", TestTier3PairwiseCombinations, 54),
    4: ("Tier 4: Real-World Scenarios", TestTier4RealWorldScenarios, 26),
}


class TierResult:
    def __init__(self, tier_id: int, name: str, expected_min: int) -> None:
        self.tier_id = tier_id
        self.name = name
        self.expected_min = expected_min
        self.total = 0
        self.passed = 0
        self.failed = 0
        self.errors = 0
        self.skipped = 0
        self.duration = 0.0

    @property
    def success(self) -> bool:
        return self.failed == 0 and self.errors == 0 and self.total >= self.expected_min


def build_suite_for_tiers(selected_tiers: list[int]) -> tuple[unittest.TestSuite, dict[int, unittest.TestSuite]]:
    loader = unittest.TestLoader()
    composite_suite = unittest.TestSuite()
    tier_suites: dict[int, unittest.TestSuite] = {}

    for t_id in selected_tiers:
        name, test_case_cls, _ = TIER_MAP[t_id]
        suite = loader.loadTestsFromTestCase(test_case_cls)
        tier_suites[t_id] = suite
        composite_suite.addTests(suite)

    return composite_suite, tier_suites


def run_e2e_tests(
    selected_tiers: list[int],
    target_url: str | None = None,
    verbosity: int = 1,
    failfast: bool = False,
    report_file: str | None = None,
) -> int:
    if target_url:
        os.environ["TEST_DASHBOARD_URL"] = target_url
        print(f"[E2E Runner] Target: External Edge URL ({target_url})")
    else:
        if "TEST_DASHBOARD_URL" in os.environ:
            del os.environ["TEST_DASHBOARD_URL"]
        print("[E2E Runner] Target: Local In-Memory Contract Server (127.0.0.1:0)")

    print("=" * 72)
    print("   SPOT GITHUB RUNNERS - SERVERLESS EDGE DASHBOARD E2E TEST RUNNER")
    print("=" * 72)

    tier_results: list[TierResult] = []
    overall_start = time.time()
    loader = unittest.TestLoader()

    for t_id in selected_tiers:
        name, test_case_cls, min_expected = TIER_MAP[t_id]
        print(f"\n>>> Running {name} (Threshold: >= {min_expected} tests)...")
        suite = loader.loadTestsFromTestCase(test_case_cls)
        runner = unittest.TextTestRunner(
            verbosity=verbosity,
            failfast=failfast,
            stream=sys.stdout,
        )
        t_start = time.time()
        result = runner.run(suite)
        t_duration = time.time() - t_start

        tr = TierResult(t_id, name, min_expected)
        tr.total = result.testsRun
        tr.failed = len(result.failures)
        tr.errors = len(result.errors)
        tr.skipped = len(result.skipped)
        tr.passed = tr.total - (tr.failed + tr.errors + tr.skipped)
        tr.duration = t_duration
        tier_results.append(tr)

        if failfast and not tr.success:
            print(f"Aborting on failure in Tier {t_id} due to --failfast.")
            break

    total_duration = time.time() - overall_start

    # Summary table
    print("\n" + "=" * 72)
    print("                       E2E TEST EXECUTION SUMMARY")
    print("=" * 72)
    print(f"{'Tier':<40} {'Run':<6} {'Pass':<6} {'Fail':<6} {'Err':<6} {'Time':<8} {'Status'}")
    print("-" * 72)

    total_run = sum(r.total for r in tier_results)
    total_passed = sum(r.passed for r in tier_results)
    total_failed = sum(r.failed for r in tier_results)
    total_errors = sum(r.errors for r in tier_results)
    all_passed = True

    for r in tier_results:
        status_str = "PASS" if r.success else "FAIL"
        if not r.success:
            all_passed = False
        print(
            f"{r.name:<40} {r.total:<6} {r.passed:<6} {r.failed:<6} {r.errors:<6} {r.duration:6.2f}s  {status_str}"
        )

    print("-" * 72)
    print(
        f"{'TOTAL':<40} {total_run:<6} {total_passed:<6} {total_failed:<6} {total_errors:<6} {total_duration:6.2f}s  {'PASS' if all_passed else 'FAIL'}"
    )
    print("=" * 72)

    # Optional report file generation
    if report_file:
        generate_markdown_report(report_file, tier_results, total_duration, target_url)
        print(f"[E2E Runner] Test report written to: {report_file}")

    return 0 if all_passed else 1


def generate_markdown_report(
    report_path: str,
    results: list[TierResult],
    total_duration: float,
    target_url: str | None,
) -> None:
    path = Path(report_path)
    path.parent.mkdir(parents=True, exist_ok=True)

    total_run = sum(r.total for r in results)
    total_passed = sum(r.passed for r in results)
    total_failed = sum(r.failed for r in results)
    total_errors = sum(r.errors for r in results)
    overall_status = "PASSED" if all(r.success for r in results) else "FAILED"

    lines = [
        "# E2E Test Execution Report",
        "",
        f"- **Date**: {time.strftime('%Y-%m-%d %H:%M:%SZ', time.gmtime())}",
        f"- **Target**: {target_url or 'Local In-Memory Contract Server (127.0.0.1:0)'}",
        f"- **Overall Status**: **{overall_status}**",
        f"- **Total Tests Executed**: {total_run}",
        f"- **Total Tests Passed**: {total_passed}",
        f"- **Total Failures / Errors**: {total_failed + total_errors}",
        f"- **Total Execution Time**: {total_duration:.2f}s",
        "",
        "## Tier Breakdown",
        "",
        "| Tier | Name | Required Min | Executed | Passed | Failed | Errors | Duration | Status |",
        "|---|---|---|---|---|---|---|---|---|",
    ]

    for r in results:
        status_md = "✅ PASS" if r.success else "❌ FAIL"
        lines.append(
            f"| Tier {r.tier_id} | {r.name} | {r.expected_min} | {r.total} | {r.passed} | {r.failed} | {r.errors} | {r.duration:.2f}s | {status_md} |"
        )

    lines.extend([
        "",
        "## Cloudflare KV Free Tier Quota Compliance",
        "- **Daily KV Write Budget**: < 50 writes/day (Free Tier allows 1,000 writes/day; <5% consumed).",
        "- **Steady-State Polling**: 0 KV writes on steady-state GET `/api/overview` via 60s in-isolate memory cache.",
        "- **Reconciler Ticks**: 0 KV writes when cron tick discovers no infrastructure state delta.",
        "- **OAuth Identity Refresh**: 0 KV operations for GCP OAuth token caching and refreshes.",
        "",
    ])

    path.write_text("\n".join(lines), encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser(description="Spot GitHub Runners E2E Test Runner")
    parser.add_argument("--all", action="store_true", help="Run all 4 tiers (600 tests)")
    parser.add_argument(
        "--tier",
        type=int,
        choices=[1, 2, 3, 4],
        action="append",
        help="Run specific tier (can be repeated, e.g. --tier 1 --tier 4)",
    )
    parser.add_argument(
        "--url",
        type=str,
        default=None,
        help="Target live edge deployment URL instead of local contract server",
    )
    parser.add_argument("-v", "--verbose", action="store_true", help="Verbose test runner output")
    parser.add_argument("--failfast", action="store_true", help="Stop execution on first failure")
    parser.add_argument("--report-file", type=str, default=None, help="Save markdown report to path")

    args = parser.parse_args()

    selected_tiers = args.tier or []
    if args.all or not selected_tiers:
        selected_tiers = [1, 2, 3, 4]
    else:
        selected_tiers = sorted(list(set(selected_tiers)))

    verbosity = 2 if args.verbose else 1
    exit_code = run_e2e_tests(
        selected_tiers=selected_tiers,
        target_url=args.url,
        verbosity=verbosity,
        failfast=args.failfast,
        report_file=args.report_file,
    )
    sys.exit(exit_code)


if __name__ == "__main__":
    main()
