#!/usr/bin/env python3
import json
import tempfile
import unittest
from pathlib import Path

import bench


class BenchParserTests(unittest.TestCase):
    def test_parse_structured_render(self) -> None:
        event = bench.parse_line(
            "bench: render view=Reading mode=Fast page=12 chapter=5 layout_ms=22 flush_ms=438 prestage_ms=15 t_ms=96260",
            "page-turn",
        )[0]
        self.assertEqual(event["event"], "render")
        self.assertEqual(event["view"], "Reading")
        self.assertEqual(event["mode"], "Fast")
        self.assertEqual(event["flush_ms"], 438)

    def test_parse_staged_render(self) -> None:
        event = bench.parse_line(
            "bench: render view=Reading mode=Fast page=13 chapter=5 staged=true stage=hit layout_ms=2 flush_ms=421 prestage_ms=16 t_ms=97000",
            "page-turn",
        )[0]
        self.assertIs(event["staged"], True)
        self.assertEqual(event["stage"], "hit")
        self.assertEqual(event["layout_ms"], 2)

    def test_parse_legacy_render(self) -> None:
        event = bench.parse_line(
            "bench: render Reading Fast page=12 ch=5 layout=24ms flush=438ms prestage=16ms t=93958",
            "page-turn",
        )[0]
        self.assertEqual(event["event"], "render")
        self.assertEqual(event["view"], "Reading")
        self.assertEqual(event["mode"], "Fast")
        self.assertTrue(event["legacy"])

    def test_button_normalization(self) -> None:
        event = bench.parse_line(
            "bench: input button=Some(Next) aux=2061 nav=5 page_raw=2937 t_ms=10524",
            "page-turn",
        )[0]
        self.assertEqual(event["button"], "Next")


    def test_parse_image_decode(self) -> None:
        event = bench.parse_line(
            "bench: image_decode slot=12 src_bytes=7198 out=310x436 ms=212 spool_ms=40 outcome=decoded",
            "image",
        )[0]
        self.assertEqual(event["event"], "image_decode")
        self.assertEqual(event["out"], "310x436")
        self.assertEqual(event["ms"], 212)
        self.assertEqual(event["outcome"], "decoded")


class BenchReportTests(unittest.TestCase):
    def test_image_summary_splits_decodes_spools_and_page_waits(self) -> None:
        lines = [
            "bench: image_decode slot=1 src_bytes=516 out=138x90 ms=14 spool_ms=0 outcome=decoded",
            "bench: image_decode slot=2 src_bytes=9000 out=310x436 ms=900 spool_ms=300 outcome=decoded",
            "bench: image_decode slot=3 src_bytes=9000 out=310x436 ms=50 spool_ms=0 outcome=cancelled",
            "bench: image_session pass=request images=2 checked=2 decoded=1 failed=0 cancelled=0 retry=0 elapsed_ms=930 pressed=false t_ms=5",
            "bench: image_session pass=request images=1 checked=1 decoded=0 failed=0 cancelled=0 retry=0 elapsed_ms=12 pressed=false t_ms=9",
            "bench: image_session pass=window images=4 checked=4 decoded=2 failed=0 cancelled=0 retry=0 elapsed_ms=930 pressed=false t_ms=20",
        ]
        events = [event for line in lines for event in bench.parse_line(line, "image")]
        summary = bench.image_summary(events)
        self.assertEqual(summary["outcomes"], {"cancelled": 1, "decoded": 2})
        self.assertEqual(summary["decode_ms"], [14, 900])
        self.assertEqual(summary["spool_ms"], [300])
        self.assertEqual(summary["wait_ms"], [930])
        self.assertEqual(summary["check_ms"], [12])

    def test_page_turn_duration_pairs_input_with_next_reading_render(self) -> None:
        events = [
            {"suite": "page-turn", "event": "input", "button": "Next", "t_ms": 100},
            {
                "suite": "page-turn",
                "event": "render",
                "view": "Reading",
                "mode": "Fast",
                "t_ms": 560,
            },
        ]
        self.assertEqual(bench.page_turn_durations(events), [460])

    def test_page_turn_durations_split_staged_turns(self) -> None:
        events = [
            {"event": "input", "button": "Next", "t_ms": 100},
            {"event": "render", "view": "Reading", "staged": False, "t_ms": 580},
            {"event": "input", "button": "Next", "t_ms": 2000},
            {"event": "render", "view": "Reading", "staged": True, "t_ms": 2430},
        ]
        self.assertEqual(bench.page_turn_durations(events), [480, 430])
        self.assertEqual(bench.page_turn_durations(events, staged=True), [430])
        self.assertEqual(bench.page_turn_durations(events, staged=False), [480])

    def test_staged_page_that_differs_is_a_warning(self) -> None:
        lines = [
            "bench: stage_check page=13 done=true match=true first_diff=-1 card=false check_ms=41 t_ms=5",
            "bench: stage_check page=14 done=false t_ms=9",
            "bench: stage_check page=15 done=true match=false first_diff=812 card=false check_ms=40 t_ms=12",
        ]
        events = [event for line in lines for event in bench.parse_line(line, "page-turn")]
        warnings = bench.evaluate_stage_checks(events)
        self.assertEqual(len(warnings), 1)
        self.assertIn("pages 15", warnings[0])
        self.assertEqual(bench.evaluate_stage_checks(events[:2]), [])

    def test_budget_warning_for_slow_page_turn(self) -> None:
        events = [
            {"suite": "page-turn", "event": "input", "button": "Next", "t_ms": 100},
            {
                "suite": "page-turn",
                "event": "render",
                "view": "Reading",
                "mode": "Fast",
                "t_ms": 800,
            },
        ]
        warnings = bench.evaluate_budgets(
            events,
            {"page-turn": {"median_press_to_settled_ms": 550}},
        )
        self.assertTrue(any("page-turn median" in warning for warning in warnings))

    def test_suite_signal_warning_for_empty_capture(self) -> None:
        warnings = bench.evaluate_suite_signals(
            [
                {"suite": "storage-cache", "event": "run_start"},
                {"suite": "storage-cache", "event": "run_end"},
            ]
        )
        self.assertEqual(warnings, ["storage-cache: no parsed bench telemetry"])

    def test_strict_report_uses_suite_signal_validation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "empty.jsonl"
            path.write_text(
                "\n".join(
                    [
                        json.dumps({"suite": "storage-cache", "event": "run_start"}),
                        json.dumps({"suite": "storage-cache", "event": "run_end"}),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            warnings = bench.summarize_paths([path], None, validate_suites=True)
        self.assertEqual(warnings, ["storage-cache: no parsed bench telemetry"])


class BenchBudgetTests(unittest.TestCase):
    def test_checked_in_budgets_load(self) -> None:
        self.assertIn("page-turn", bench.load_budgets(bench.DEFAULT_BUDGETS))

    def test_unreadable_budgets_fail_instead_of_skipping(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            broken = Path(tmp) / "benches.toml"
            broken.write_text("[page-turn\n", encoding="utf-8")
            with self.assertRaises(SystemExit):
                bench.load_budgets(broken)
            with self.assertRaises(SystemExit):
                bench.load_budgets(Path(tmp) / "missing.toml")


if __name__ == "__main__":
    unittest.main()
