"""
Unified End-to-End (E2E) Test Suite for 100% Rust Serverless Cloudflare Worker Migration.

Consolidates all 4 test tiers:
- Tier 1: Feature Coverage (N=52 features in isolation, 260 tests)
- Tier 2: Boundary & Corner Cases (N=52 features, 260 tests)
- Tier 3: Cross-Feature Combinations (54 pairwise interaction tests)
- Tier 4: Real-World Application Scenarios (26 comprehensive end-to-end scenarios)

Total Suite: 600 tests.

Can be run standalone with:
    python3 -m unittest tests/e2e/test_edge_dashboard_e2e.py
or via runner:
    python3 tests/e2e/runner.py --all
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

# Ensure tests/e2e is on path for relative module imports
E2E_DIR = Path(__file__).resolve().parent
if str(E2E_DIR) not in sys.path:
    sys.path.insert(0, str(E2E_DIR))

# Import all test tier suites
from test_tier1_feature_coverage import TestTier1FeatureCoverage
from test_tier2_boundary_corner import TestTier2BoundaryCorner
from test_tier3_pairwise_combinations import TestTier3PairwiseCombinations
from test_tier4_real_world_scenarios import TestTier4RealWorldScenarios


def load_tests(loader: unittest.TestLoader, tests: unittest.TestSuite, pattern: str | None) -> unittest.TestSuite:
    """Standard unittest load_tests protocol to provide consolidated suite."""
    suite = unittest.TestSuite()
    suite.addTests(loader.loadTestsFromTestCase(TestTier1FeatureCoverage))
    suite.addTests(loader.loadTestsFromTestCase(TestTier2BoundaryCorner))
    suite.addTests(loader.loadTestsFromTestCase(TestTier3PairwiseCombinations))
    suite.addTests(loader.loadTestsFromTestCase(TestTier4RealWorldScenarios))
    return suite


if __name__ == "__main__":
    unittest.main()
