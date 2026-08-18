#!/usr/bin/env python3
# Copyright (c) scx_cohort authors.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.
"""Unit tests for analyze.py's pure functions (run by `make test`)."""

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from analyze import direction, mann_whitney  # noqa: E402


class TestDirection(unittest.TestCase):
    def test_suite_metrics(self):
        # Every metric run-suite.sh emits today, pinned to the direction
        # the comparison must judge it by.
        expected = {
            "elapsed_sec": "lower",
            "time_sec": "lower",
            "affinity_pct": "higher",
            "plan_pct": "higher",
            "throughput_msgs_per_sec": "higher",
            "rtt_mean_ns": "lower",
            "rtt_p50_ns": "lower",
            "rtt_p90_ns": "lower",
            "rtt_p99_ns": "lower",
            "rtt_p999_ns": "lower",
            "rtt_max_ns": "lower",
            # The regression this file exists for: a rate of a bad event
            # is a cost, even though it ends in "per_sec".
            "migrations_per_sec": "lower",
        }
        for metric, want in expected.items():
            self.assertEqual(direction(metric), want, metric)

    def test_mangohud_metrics(self):
        self.assertEqual(direction("fps_avg"), "higher")
        self.assertEqual(direction("fps_1pct_low"), "higher")
        self.assertEqual(direction("fps_01pct_low"), "higher")
        self.assertEqual(direction("frametime_p99_ms"), "lower")

    def test_bad_event_beats_rate_suffixes(self):
        # "drops" contains "ops" and "cache_misses_per_sec" contains
        # "per_sec"; the bad-event check must win over both.
        self.assertEqual(direction("dropped_frame_drops"), "lower")
        self.assertEqual(direction("cache_misses_per_sec"), "lower")
        self.assertEqual(direction("page_faults_per_sec"), "lower")

    def test_unknown_metric_has_no_verdict(self):
        self.assertIsNone(direction("mystery"))


class TestMannWhitney(unittest.TestCase):
    def test_complete_separation_n5(self):
        # Fully separated 5-vs-5 samples: exact two-sided p = 2/C(10,5)
        # = 2/252 — the 0.008 floor quoted in bench/README.md.
        a = [1, 2, 3, 4, 5]
        b = [10, 20, 30, 40, 50]
        p = mann_whitney(a, b)
        self.assertAlmostEqual(p, 2 / 252, places=6)

    def test_symmetry(self):
        a = [1.0, 3.0, 5.0, 7.0, 9.0]
        b = [2.0, 4.0, 6.0, 8.0, 10.0]
        self.assertAlmostEqual(mann_whitney(a, b), mann_whitney(b, a),
                               places=9)

    def test_identical_samples_not_significant(self):
        a = [5.0] * 5
        self.assertAlmostEqual(mann_whitney(a, list(a)), 1.0, places=6)

    def test_empty_side(self):
        self.assertIsNone(mann_whitney([], [1.0]))


if __name__ == "__main__":
    unittest.main()
