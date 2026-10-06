import json
import tempfile
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from unittest.mock import patch

from gepa import optimize
from myco_gepa import MycoAdapter, dataset

FAKE = '''#!/usr/bin/env python3
import json, sys
from pathlib import Path
args = sys.argv[1:]
assert args[0] == "run" and "--free-only" in args
case = Path(args[1]); output = Path(args[args.index("--output") + 1]) / "attempt"
(output / "workspace").mkdir(parents=True)
prelude = Path(args[args.index("--prelude") + 1]).read_text()
reflect = json.loads((case / "case.json").read_text())["name"] == "prelude-reflection"
if reflect:
    (output / "workspace/prelude.md").write_text("Verify the resulting artifact before finishing.")
score = float(reflect or "Verify" in prelude)
(output / "events.jsonl").write_text('{"event":"tool_finished","is_error":false}\\n')
(output / "result.json").write_text(json.dumps({"status":"completed","score":score,
    "feedback":"Verify the artifact.","agent":{"answer":"Finished.",
    "metrics":{"requests":1,"input_tokens":100,"output_tokens":10}}}))
print(json.dumps({"groups":[{"estimated_cost_usd":0.0}]}))
'''


class MycoGepaTests(unittest.TestCase):
    def adapter(self, root, **kwargs):
        return MycoAdapter(root / "binary", root / "config", "free", root / "runs",
                           max_reflections=1, **kwargs)

    def fail_reflection(self, adapter):
        with patch.object(adapter, "_run", side_effect=RuntimeError("provider failed")) as run:
            with self.assertRaisesRegex(RuntimeError, "provider failed"):
                adapter.propose_new_texts({"prelude": "seed"}, {}, ["prelude"])
            self.assertEqual(run.call_count, 1)

    def test_failed_reflection_cannot_spend_again_after_stale_checkpoint_restore(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.fail_reflection(self.adapter(root))
            restarted = self.adapter(root)
            restarted.set_adapter_state({"reflections": 0})
            with patch.object(restarted, "_run") as run:
                with self.assertRaisesRegex(RuntimeError, "reflection budget"):
                    restarted.propose_new_texts({"prelude": "seed"}, {}, ["prelude"])
                run.assert_not_called()

    def test_concurrent_adapters_share_one_durable_reflection_allowance(self):
        with tempfile.TemporaryDirectory() as directory:
            adapters = [self.adapter(Path(directory)) for _ in range(2)]
            def attempt(adapter):
                with patch.object(adapter, "_run", side_effect=RuntimeError("provider failed")) as run:
                    try:
                        adapter.propose_new_texts({"prelude": "seed"}, {}, ["prelude"])
                    except RuntimeError:
                        pass
                    return run.call_count
            with ThreadPoolExecutor(max_workers=2) as workers:
                self.assertEqual(sum(workers.map(attempt, adapters)), 1)

    def test_existing_proposals_count_even_without_a_saved_adapter_counter(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "runs/proposals/old-attempt/workspace").mkdir(parents=True)
            adapter = self.adapter(root)
            with patch.object(adapter, "_run") as run:
                with self.assertRaisesRegex(RuntimeError, "reflection budget"):
                    adapter.propose_new_texts({"prelude": "seed"}, {}, ["prelude"])
                run.assert_not_called()

    def test_unflushed_reservation_stops_work_and_remains_spent_after_restart(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            adapter = self.adapter(root)
            with patch("myco_gepa.sync_directory", side_effect=[None, OSError("disk full")]):
                with patch.object(adapter, "_run") as run:
                    with self.assertRaisesRegex(OSError, "disk full"):
                        adapter.propose_new_texts({"prelude": "seed"}, {}, ["prelude"])
                    run.assert_not_called()
            restarted = self.adapter(root)
            with patch.object(restarted, "_run") as run:
                with self.assertRaisesRegex(RuntimeError, "reflection budget"):
                    restarted.propose_new_texts({"prelude": "seed"}, {}, ["prelude"])
                run.assert_not_called()

    def test_visible_ancestor_after_failed_flush_cannot_bypass_admission_durability(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def reject_ancestor(path):
                if path == root:
                    raise OSError("ancestor flush failed")
            with patch("myco_gepa.sync_directory", side_effect=reject_ancestor):
                for _ in range(2):
                    with patch.object(MycoAdapter, "_run") as run:
                        with self.assertRaisesRegex(OSError, "ancestor flush failed"):
                            self.adapter(root / "new-parent").propose_new_texts(
                                {"prelude": "seed"}, {}, ["prelude"])
                        run.assert_not_called()

    def test_real_gepa_loop_uses_myco_adapter_reflection_and_keeps_test_split_held_out(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "fake-myco-eval"
            binary.write_text(FAKE)
            binary.chmod(0o755)
            config = root / "config.toml"
            config.write_text("no credentials used")
            cases = root / "cases"
            for split in ["train", "validation", "test"]:
                case = cases / split
                case.mkdir(parents=True)
                (case / "case.json").write_text(json.dumps({"name":split, "split":split, "source":None}))
                (case / "context.json").write_text(json.dumps([
                    {"UserMessage":{"content":[{"Text":{"text":f"Task {split}"}}]}}]))
            splits = dataset(cases)
            adapter = MycoAdapter(binary, config, "free", root / "runs", max_reflections=2)
            result = optimize(seed_candidate={"prelude":"baseline"}, trainset=splits["train"],
                              valset=splits["validation"], adapter=adapter, max_metric_calls=6,
                              reflection_minibatch_size=1, seed=0, raise_on_exception=True)
            self.assertIn("Verify", result.best_candidate["prelude"])
            self.assertEqual(adapter.reflections, 1)
            state = adapter.get_adapter_state()
            adapter.reflections = 0
            adapter.set_adapter_state(state)
            self.assertEqual(adapter.reflections, 1)
            for prompt in (root / "runs/proposals").rglob("context.json"):
                self.assertNotIn("Task test", prompt.read_text())
                self.assertNotIn("Task validation", prompt.read_text())

    def test_related_session_cuts_cannot_cross_splits(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for split in ["train", "validation"]:
                case = root / split
                case.mkdir()
                (case / "case.json").write_text(json.dumps({"split":split, "source":{"session_id":"same"}}))
            with self.assertRaisesRegex(ValueError, "multiple splits"):
                dataset(root)


if __name__ == "__main__":
    unittest.main()
