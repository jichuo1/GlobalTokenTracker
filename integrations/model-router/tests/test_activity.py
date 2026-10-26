import json
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest

from gtt_router.activity import ActivityWatch
from gtt_router.adapters import ExecutionContext, ProcessChannel, antigravity, native_slot, zcode
from gtt_router.models import Route, Task, WorkerFailure
from gtt_router.native_agents import opencode, print_agent


class ActivityWatchTests(unittest.TestCase):
    def watch(self, **values):
        self.current = 0
        return ActivityWatch(10, mode="activity", hard_deadline=40, idle_s=3,
                             stall_grace_s=1, clock=lambda: self.current, **values)

    def test_new_output_and_reasoning_extend_soft_deadline_with_finite_horizon(self):
        watch = self.watch()
        for current in [1, 4, 7, 10, 13, 16, 19, 22, 25, 28, 31, 34, 37, 39]:
            self.current = current
            self.assertTrue(watch.content("reasoning", "m", "x" * current))
            self.assertIsNone(watch.expiry_reason())
        self.current = 40
        self.assertEqual(watch.expiry_reason(), "hard_deadline")
        self.assertFalse(watch.content("output", "late", "cannot resurrect"))

    def test_heartbeat_empty_and_same_phase_cannot_extend(self):
        watch = self.watch()
        self.assertTrue(watch.record("native_phase", key="m", version="thinking"))
        for current in [3, 7, 9]:
            self.current = current
            self.assertFalse(watch.record("heartbeat", version=current))
            self.assertFalse(watch.content("output", "m", ""))
            self.assertFalse(watch.record("native_phase", key="m", version="thinking"))
        self.current = 10
        self.assertEqual(watch.expiry_reason(), "first_activity_timeout")

    def test_phase_only_does_not_shorten_first_response_window(self):
        self.current = 0
        watch = ActivityWatch(600, mode="activity", hard_deadline=900, ttft_s=600,
            idle_s=30, stall_grace_s=5, clock=lambda: self.current)
        self.current = 7
        watch.record("native_phase", key="m", version="started")
        self.current = 203
        self.assertIsNone(watch.expiry_reason())
        self.assertEqual(watch.snapshot()["expires_in_s"], 397)
        self.current = 600
        self.assertEqual(watch.expiry_reason(), "first_activity_timeout")
        self.current = 0
        active = ActivityWatch(600, mode="activity", hard_deadline=900, ttft_s=600,
            idle_s=30, stall_grace_s=5, clock=lambda: self.current)
        self.current = 7
        active.record("native_phase", key="m", version="started")
        self.current = 20
        active.content("output", "m", "actual response")
        self.current = 55
        self.assertEqual(active.expiry_reason(), "inactivity_timeout")
        self.current = 0
        bounded = ActivityWatch(600, mode="activity", hard_deadline=250, ttft_s=600,
            idle_s=30, clock=lambda: self.current)
        bounded.record("native_phase", key="m", version="started")
        self.current = 250
        self.assertEqual(bounded.expiry_reason(), "hard_deadline")

    def test_first_activity_window_idle_grace_and_hard_are_distinct(self):
        watch = self.watch(ttft_s=4)
        self.current = 4
        self.assertEqual(watch.expiry_reason(), "first_activity_timeout")
        watch = self.watch()
        self.current = 9
        watch.record("tool_start", key="search-1", version="started")
        self.current = 12.9
        self.assertIsNone(watch.expiry_reason())
        self.current = 13
        self.assertEqual(watch.expiry_reason(), "inactivity_timeout")
        watch = self.watch()
        self.current = 1
        watch.record("tool_start", key="search-early", version="started")
        self.current = 5
        self.assertEqual(watch.expiry_reason(), "inactivity_timeout")

    def test_hard_mode_and_replayed_content_do_not_extend(self):
        self.current = 0
        watch = ActivityWatch(10, mode="hard", hard_deadline=40, clock=lambda: self.current)
        self.current = 9
        self.assertTrue(watch.content("output", "m", "abcdef"))
        self.assertFalse(watch.content("output", "m", "abc"))
        self.assertFalse(watch.content("output", "m", "abcdef"))
        self.current = 10
        self.assertEqual(watch.expiry_reason(), "hard_deadline")

    def test_tool_result_and_actual_phase_change_count_once(self):
        watch = self.watch()
        for kind, value in [("tool_start", "running"), ("tool_complete", "done"),
                            ("tool_result", {"hits": [1, 2]}), ("native_phase", "reviewing")]:
            self.assertTrue(watch.record(kind, key="search", version=value))
            self.assertFalse(watch.record(kind, key="search", version=value))
        self.assertEqual(watch.snapshot()["activity_count"], 4)

    def test_invalid_or_unbounded_deadline_is_rejected(self):
        for values in [{"deadline": float("inf")}, {"deadline": 10, "hard_deadline": float("nan")},
                       {"deadline": 10, "idle_s": -1}, {"deadline": 10, "mode": "forever"}]:
            with self.assertRaises(ValueError):
                ActivityWatch(**values)

    def test_long_sequenced_stream_has_no_artificial_event_count_ceiling(self):
        watch = self.watch()
        for sequence in range(20000):
            self.current = sequence / 1000
            self.assertTrue(watch.content("output", "m", "x", delta=True, version=sequence))
        self.assertEqual(watch.snapshot()["output_chars"], 20000)
        self.assertFalse(watch.content("output", "m", "x", delta=True, version=1))


class ActivityAdapterTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="gtt-activity-")
        self.root = Path(self.temp.name)
        self.fixture = str(Path(__file__).with_name("activity_stream_fixture.py"))

    def tearDown(self):
        self.temp.cleanup()

    def context(self, soft=.8, hard=4, mode="activity", idle=.3, grace=.2):
        return ExecutionContext(threading.Event(), time.monotonic() + soft, self.root,
            timeout_mode=mode, hard_deadline=time.monotonic() + hard, idle_s=idle, stall_grace_s=grace)

    def route(self, mode):
        return Route("fixture", "provider", "model", runtime={"command": [sys.executable, self.fixture, mode],
            "builtin_config": "unused", "provider_config": "unused", "mcp_names": []}, max_parallel=1)

    def consume(self, mode, context):
        channel = ProcessChannel([sys.executable, self.fixture, mode], context)
        try:
            while True:
                event = channel.read()
                if event.get("type") == "final":
                    return event
        finally:
            channel.close()
            self.assertIsNotNone(channel.process.poll())
            self.assertTrue(all(not r.is_alive() for r in channel.readers))

    def test_real_streaming_output_and_thinking_survive_soft_deadline(self):
        for mode in ["output", "thinking"]:
            with self.subTest(mode=mode):
                context = self.context()
                self.consume(mode, context)
                self.assertGreater(time.monotonic(), context.soft_deadline)
                self.assertGreater(context.public_activity()["activity_count"], 10)

    def test_silent_empty_repeated_phase_and_timestamp_heartbeat_timeout(self):
        for mode in ["silence", "empty", "repeat", "heartbeat"]:
            with self.subTest(mode=mode):
                context = self.context(soft=.6, idle=.15, grace=.05)
                started = time.monotonic()
                with self.assertRaises(WorkerFailure) as failed:
                    self.consume(mode, context)
                self.assertEqual(failed.exception.code, "timeout")
                self.assertLess(time.monotonic() - started, 3)

    def test_hard_mode_and_cancellation_are_not_overridden_by_streaming(self):
        context = self.context(soft=.7, mode="hard")
        with self.assertRaises(WorkerFailure) as failed:
            self.consume("output", context)
        self.assertEqual(failed.exception.details["timeout_reason"], "hard_deadline")
        context = self.context()
        timer = threading.Timer(.5, context.cancel.set)
        timer.start()
        try:
            with self.assertRaises(WorkerFailure) as failed:
                self.consume("thinking", context)
            self.assertEqual(failed.exception.code, "cancelled")
        finally:
            timer.join()

    def test_blocked_input_still_times_out_and_writer_is_cleaned(self):
        context = self.context(soft=.5)
        channel = ProcessChannel([sys.executable, self.fixture, "heartbeat"], context)
        try:
            with self.assertRaises(WorkerFailure) as failed:
                channel.send({"text": "x" * 200000})
            self.assertEqual(failed.exception.code, "timeout")
        finally:
            channel.close()
        self.assertFalse(channel.writer.is_alive())

    def test_native_slot_shares_watch_and_counters(self):
        context = self.context()
        route = self.route("output")
        with native_slot(route, context) as slot:
            self.assertIs(slot.activity, context.activity)
            slot.report_content("reasoning", "m", "private supplied material")
        public = context.public_activity()
        self.assertEqual(public["reasoning_chars"], 25)
        self.assertNotIn("private", json.dumps(public))

    def test_native_fixed_timeouts_use_safety_horizon_not_soft_expectation(self):
        for mode, adapter in [("zcode", zcode), ("agy", antigravity)]:
            with self.subTest(mode=mode):
                context = self.context()
                result = adapter(self.route(mode), "fixture input", Task("extract", "fixture"), context)
                self.assertIn("activity fixture", result["text"])
                self.assertGreater(time.monotonic(), context.soft_deadline)
                timeout_file = next(self.root.rglob("native-timeout.json"))
                timeout = json.loads(timeout_file.read_text())
                self.assertGreater(timeout.get("timeout_ms", timeout.get("timeout_s", 0) * 1000), 2000)
                timeout_file.unlink()

    def test_http_message_growth_is_activity_and_poll_timestamps_are_not(self):
        context = self.context(soft=1.2, hard=5)
        result = opencode(self.route("http_active"), "fixture", Task("extract", "fixture"), context)
        self.assertGreater(time.monotonic(), context.soft_deadline)
        self.assertIn("activity fixture", result["text"])
        self.assertGreater(context.public_activity()["output_chars"], 10)
        context = self.context(soft=1, hard=4)
        with self.assertRaises(WorkerFailure) as failed:
            opencode(self.route("http_heartbeat"), "fixture", Task("extract", "fixture"), context)
        self.assertEqual(failed.exception.code, "timeout")
        self.assertEqual(failed.exception.usage["total_tokens"], 15)
        self.assertEqual(failed.exception.metering["native_dedup_key"], "opencode:msg:msg_activity")

    def test_claude_partial_flag_thinking_and_usage_are_observed(self):
        route = self.route("print_active")
        route.adapter = "claude_cli"
        context = self.context()
        result = print_agent(route, "fixture", Task("extract", "fixture"), context)
        args = json.loads(next(self.root.rglob("print-args.json")).read_text())
        self.assertIn("--include-partial-messages", args)
        self.assertGreater(time.monotonic(), context.soft_deadline)
        self.assertEqual(context.public_activity()["reasoning_chars"], 16)
        self.assertEqual(result["usage"]["total_tokens"], 26)
        route = self.route("print_stall")
        route.adapter = "claude_cli"
        context = self.context(soft=1, idle=.2, grace=.1)
        with self.assertRaises(WorkerFailure) as failed:
            print_agent(route, "fixture", Task("extract", "fixture"), context)
        self.assertEqual(failed.exception.code, "timeout")
        self.assertEqual(failed.exception.usage["input_tokens"], 10)
        self.assertEqual(failed.exception.usage["output_tokens"], 1)


if __name__ == "__main__":
    unittest.main()
