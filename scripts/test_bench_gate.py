#!/usr/bin/env python3
"""Unit tests for `scripts/bench_gate.py` — stdlib `unittest`, no new dependency
(same reasoning as `test_release_verify.py`: this is dev tooling, not something
the engine ships).

Every test here fixes a DECISION, not an implementation detail. The three that
matter most are the ones that keep the gate from repeating 2026-08-10: a run the
bench called unpublishable is not judged, a slowdown the anchors share is not
blamed on the engine, and a slowdown with no anchor at all is refused instead of
guessed.

Run: python3 scripts/test_bench_gate.py
"""
import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bench_gate as bg  # noqa: E402

CPU = "AMD Ryzen 9 7945HX"


def run_json(
    *,
    delonix=80,
    docker=300,
    podman=280,
    publishable=True,
    threads=32,
    cpu=CPU,
    spread=None,
    drop_spread=False,
):
    """A run as `bench.sh --json` emits it.

    `spread` overrides the per-tool dispersion (a dict); `drop_spread` removes
    the field entirely, which is what a run produced by an older `bench.sh`
    looks like.
    """
    spread = spread or {}

    def cell(v, tool):
        if v is None:
            return None
        c = {"median_ms": v, "min_ms": v - 5, "max_ms": v + 5, "samples_ms": [v]}
        if not drop_spread:
            c["spread"] = spread.get(tool, 1.1)
        return c

    return {
        "schema": 1,
        "when": "2026-09-23T10:00:00Z",
        "runs": 10,
        "bench": {
            "cpu": cpu,
            "threads": threads,
            "mem": "31 GiB",
            "kernel": "7.0.0-31-generic",
            "load1": 1.5,
            "threshold": 16.0,
            "density": 0,
            "publishable": publishable,
        },
        "tools": {"docker": "Docker version 29.8.1", "podman": "podman version 5", "delonix": "delonix 4.3.0"},
        "lines": {
            "4a": {
                "docker": cell(docker, "docker"),
                "podman": cell(podman, "podman"),
                "delonix": cell(delonix, "delonix"),
            }
        },
    }


def baseline_json(*, delonix=80, docker=300, podman=280, threads=32, cpu=CPU):
    return {
        "schema": 1,
        "entries": [
            {
                "machine": {"cpu": cpu, "threads": threads},
                "recorded": "2026-09-23",
                "commit": "abc1234",
                "delonix_version": "delonix 4.3.0",
                "runs": 10,
                "lines": {"4a": {"delonix": delonix, "docker": docker, "podman": podman}},
            }
        ],
    }


class GateTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.baseline = Path(self.tmp.name) / "bench_baseline.json"
        self.patch = mock.patch.object(bg, "BASELINE", self.baseline)
        self.patch.start()
        self.addCleanup(self.patch.stop)
        self.addCleanup(self.tmp.cleanup)

    def write_baseline(self, **kw):
        self.baseline.write_text(json.dumps(baseline_json(**kw)))

    def check(self, run, tolerance=bg.DEFAULT_TOLERANCE):
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            rc = bg.check(run, tolerance)
        return rc, out.getvalue() + err.getvalue()

    # --- the happy path, and the one number that decides it ---

    def test_within_tolerance_passes(self):
        self.write_baseline()
        rc, _ = self.check(run_json(delonix=96))  # +20%, under 25%
        self.assertEqual(rc, 0)

    def test_regression_with_steady_anchors_fails(self):
        self.write_baseline()
        rc, txt = self.check(run_json(delonix=160))  # 2x, anchors unchanged
        self.assertEqual(rc, 1)
        self.assertIn("×2.00", txt)

    # --- the three refusals: what this gate exists for ---

    def test_a_shared_slowdown_is_the_bench_not_the_engine(self):
        # 2026-08-10 in one test: everything got ~2x slower at once. Blaming the
        # engine here is the false red that retired the original battery.
        self.write_baseline()
        rc, txt = self.check(run_json(delonix=160, docker=600, podman=560))
        self.assertEqual(rc, bg.REFUSED)
        self.assertIn("âncoras degradaram", txt)

    def test_a_regression_with_no_anchor_is_refused_not_guessed(self):
        self.write_baseline(docker=None, podman=None)
        rc, txt = self.check(run_json(delonix=160, docker=None, podman=None))
        self.assertEqual(rc, bg.REFUSED)
        self.assertIn("âncora", txt)

    def test_an_unpublishable_run_is_never_judged(self):
        self.write_baseline()
        rc, txt = self.check(run_json(delonix=1600, publishable=False))
        self.assertEqual(rc, bg.REFUSED)
        self.assertIn("NÃO PUBLICÁVEL", txt)

    def test_an_unknown_machine_is_refused_with_the_record_command(self):
        self.write_baseline(threads=8, cpu="Some Other CPU")
        rc, txt = self.check(run_json(delonix=160))
        self.assertEqual(rc, bg.REFUSED)
        self.assertIn("--record", txt)

    # --- a refusal is a third outcome, never a quiet green ---

    def test_every_refusal_says_it_did_not_judge(self):
        self.write_baseline()
        _, txt = self.check(run_json(delonix=1600, publishable=False))
        self.assertIn("não julgou nada", txt)

    # --- dispersion: the noise the load threshold does not catch ---
    #
    # Measured 2026-09-23 on a box at load 6.44 — comfortably under the limit —
    # where one ten-sample docker line ran 434 ms to 5407 ms. The median made it
    # look like a number. These four fix what happens instead.

    def test_a_wildly_dispersed_subject_line_is_refused(self):
        self.write_baseline()
        rc, txt = self.check(run_json(delonix=160, spread={"delonix": 12.4}))
        self.assertEqual(rc, bg.REFUSED)
        self.assertIn("12.4x", txt)

    def test_a_dispersed_anchor_drops_out_of_the_judgement(self):
        # docker is noise; podman is steady and says the machine did NOT move.
        # The regression still fails — dropping the bad anchor must not turn a
        # real failure into a refusal.
        self.write_baseline()
        rc, txt = self.check(run_json(delonix=160, docker=4000, spread={"docker": 9.0}))
        self.assertEqual(rc, 1)
        self.assertIn("instável", txt)

    def test_with_every_anchor_dispersed_there_is_nothing_to_attribute_with(self):
        self.write_baseline()
        rc, txt = self.check(
            run_json(delonix=160, spread={"docker": 9.0, "podman": 7.0})
        )
        self.assertEqual(rc, bg.REFUSED)
        self.assertIn("âncora", txt)

    def test_a_run_without_the_spread_field_is_unknown_not_stable(self):
        # An older `bench.sh` did not emit it. Reading a missing number as 1.0
        # would quietly re-admit the noisy lines this check keeps out.
        self.write_baseline()
        rc, _ = self.check(run_json(delonix=160, drop_spread=True))
        self.assertEqual(rc, bg.REFUSED)

    def test_record_refuses_a_dispersed_run(self):
        self.write_baseline()
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            rc = bg.record(run_json(spread={"docker": 12.4}))
        self.assertEqual(rc, bg.REFUSED)
        self.assertEqual(json.loads(self.baseline.read_text())["entries"][0]["lines"]["4a"]["delonix"], 80)

    def test_recording_is_stricter_than_judging(self):
        # The run that motivated the second ceiling. 2.69x is under the judging
        # bar — a run this noisy still gets JUDGED, and here the median happens
        # to sit inside the tolerance, so it passes. The same run must never
        # become the baseline: on the box where it was measured it would have
        # recorded 165 ms for an engine that did 88 ms minutes earlier.
        self.write_baseline()
        noisy = run_json(delonix=88, spread={"delonix": 2.69})
        rc, _ = self.check(noisy)
        self.assertEqual(rc, 0)
        with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
            rc_rec = bg.record(noisy)
        self.assertEqual(rc_rec, bg.REFUSED)
        self.assertEqual(json.loads(self.baseline.read_text())["entries"][0]["lines"]["4a"]["delonix"], 80)

    # --- improvement: warn, never fail ---

    def test_a_big_improvement_passes_and_warns(self):
        self.write_baseline()
        rc, txt = self.check(run_json(delonix=40))
        self.assertEqual(rc, 0)
        self.assertIn("MAIS RÁPIDO", txt)

    # --- the baseline is per machine ---

    def test_machine_ignores_the_kernel_and_normalises_spacing(self):
        m = bg.machine_of(run_json(cpu="AMD   Ryzen 9   7945HX"))
        self.assertEqual(m, {"cpu": "AMD Ryzen 9 7945HX", "threads": 32})
        self.assertNotIn("kernel", m)

    def test_record_refuses_an_unpublishable_run(self):
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            rc = bg.record(run_json(publishable=False))
        self.assertEqual(rc, bg.REFUSED)
        self.assertFalse(self.baseline.exists())

    def test_record_replaces_the_entry_of_the_same_machine(self):
        self.write_baseline(delonix=80)
        out = io.StringIO()
        with redirect_stdout(out):
            rc = bg.record(run_json(delonix=120))
        self.assertEqual(rc, 0)
        data = json.loads(self.baseline.read_text())
        self.assertEqual(len(data["entries"]), 1)  # replaced, never appended twice
        self.assertEqual(data["entries"][0]["lines"]["4a"]["delonix"], 120)

    def test_record_keeps_the_entry_of_another_machine(self):
        self.write_baseline(cpu="Some Other CPU", threads=8)
        with redirect_stdout(io.StringIO()):
            bg.record(run_json())
        data = json.loads(self.baseline.read_text())
        self.assertEqual(len(data["entries"]), 2)


if __name__ == "__main__":
    unittest.main(verbosity=2)
